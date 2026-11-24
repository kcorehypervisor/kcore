# Eval-only guard for kcore.kubernetes.version: the releases file's hostPin
# matches pkgs.kubernetes, an empty version or the pin keeps pkgs.kubernetes,
# and every listed release resolves to its own package for the primary
# cluster and for an extra cluster on the same host.
{
  pkgs,
  lib,
  nixosSystem,
  mkNode,
}:
let
  releases = builtins.fromJSON (builtins.readFile ../modules/kubernetes-releases.json);

  base = {
    networking.hostName = "k8s-eval";
    system.stateVersion = "25.11";
    boot.loader.grub.enable = false;
    fileSystems."/" = {
      device = "nodev";
      fsType = "tmpfs";
    };
  };

  mkHost =
    primary: extra:
    nixosSystem {
      inherit (pkgs) system;
      modules = [
        ../modules/kcore-kubernetes.nix
        (
          _:
          base
          // {
            kcore.kubernetes = {
              enable = true;
              role = "worker";
              advertiseAddress = "10.0.0.2";
              apiServerEndpoint = "10.0.0.1:6443";
              version = primary;
              additionalClusters.b = {
                role = "worker";
                advertiseAddress = "10.0.0.2";
                apiServerEndpoint = "10.0.0.1:7443";
                version = extra;
              };
            };
          }
        )
      ];
    };

  kubeletOf = host: unit: host.config.systemd.services.${unit}.serviceConfig.ExecStart;
  runs =
    host: unit: name:
    lib.hasInfix "-${name}/bin/kubelet" (kubeletOf host unit);

  # An installed node builds through its host flake, which only imports the
  # host files mkNode lists. The agent writes both Kubernetes files there.
  installed = mkNode { root = ./fixtures/kubernetes-host; };

  controlPlane = nixosSystem {
    inherit (pkgs) system;
    modules = [
      ../modules/kcore-kubernetes.nix
      (
        _:
        base
        // {
          kcore.kubernetes = {
            enable = true;
            role = "control-plane";
            advertiseAddress = "10.0.0.1";
            apiServerEndpoint = "10.0.0.1:6443";
            haEnabled = true;
            vipAddress = "10.0.0.100";
            vipInterface = "eth0";
            additionalClusters.b = {
              role = "control-plane";
              advertiseAddress = "10.0.0.1";
              apiServerEndpoint = "10.0.0.1:7443";
              scheduleOnControlPlane = true;
              haEnabled = true;
              vipAddress = "10.0.0.101";
              vipInterface = "eth0";
            };
          };
        }
      )
    ];
  };
  # `kubectl exec` must not depend on DNS for node names.
  roleScript =
    unit: builtins.readFile controlPlane.config.systemd.services.${unit}.serviceConfig.ExecStart;
  dialsInternalIp =
    unit:
    lib.hasInfix "--kubelet-preferred-address-types=InternalIP,"
      controlPlane.config.systemd.services.${unit}.serviceConfig.ExecStart;

  # Stale etcd data from another cluster is set aside, never served.
  retireScript = host: host.config.system.activationScripts.kcoreRetiredEtcd.text;
  retiresPrimary = host: lib.hasInfix "kcore_retire_etcd /var/lib/etcd\n" (retireScript host);
  preparesEtcd =
    unit:
    lib.hasInfix "kcore-etcd-prepare" "${controlPlane.config.systemd.services.${unit}.serviceConfig.ExecStartPre
    }";

  # Kubelets point pods at clusterDns; something has to answer there.
  coreDns = host: path: host.config.environment.etc.${path}.text or "";
  hasCoreDnsUnit = host: unit: host.config.systemd.services ? ${unit};

  # The VIP must leave a node whose API server is down, and a config change
  # must reach the running keepalived.
  keepalivedConf = path: controlPlane.config.environment.etc.${path}.text;
  tracksApiServer =
    path: port:
    lib.hasInfix "track_script" (keepalivedConf path)
    && lib.hasInfix "kcore-apiserver-check-${toString port}" (keepalivedConf path);
  reloadsOnChange = unit: controlPlane.config.systemd.services.${unit}.reloadTriggers != [ ];

  pinned = mkHost "" releases.hostPin;
  upgraded = lib.mapAttrsToList (version: _: {
    inherit version;
    host = mkHost version version;
  }) releases.releases;
in
assert releases.hostPin == pkgs.kubernetes.version;
assert runs installed "kubelet" "kubernetes-${pkgs.kubernetes.version}";
assert installed.config.networking.firewall.checkReversePath == "loose";
assert lib.elem 4240 installed.config.networking.firewall.allowedTCPPorts;
assert dialsInternalIp "kube-apiserver";
assert dialsInternalIp "kube-apiserver-b";
assert lib.hasInfix "control-plane:NoSchedule --overwrite" (
  roleScript "kcore-kubernetes-node-roles"
);
assert lib.hasInfix "node-role.kubernetes.io/worker-" (roleScript "kcore-kubernetes-node-roles");
assert lib.hasInfix "control-plane:NoSchedule-" (roleScript "kcore-kubernetes-node-roles-b");
assert !lib.hasInfix "NoSchedule --overwrite" (roleScript "kcore-kubernetes-node-roles-b");
assert preparesEtcd "etcd";
assert preparesEtcd "etcd-b";
assert !retiresPrimary controlPlane;
assert lib.hasInfix "b|-)" (retireScript controlPlane);
assert lib.hasInfix "clusterIP: 10.96.0.10" (coreDns controlPlane "kubernetes/coredns.yaml");
assert lib.hasInfix "clusterIP: 10.112.0.10" (coreDns controlPlane "kubernetes/b/coredns.yaml");
assert hasCoreDnsUnit controlPlane "kcore-kubernetes-coredns";
assert hasCoreDnsUnit controlPlane "kcore-kubernetes-coredns-b";
assert !hasCoreDnsUnit installed "kcore-kubernetes-coredns";
assert tracksApiServer "keepalived/keepalived.conf" 6443;
assert tracksApiServer "keepalived/keepalived-b.conf" 7443;
assert reloadsOnChange "keepalived";
assert reloadsOnChange "keepalived-b";
assert retiresPrimary pinned;
assert !lib.hasInfix "b|-)" (retireScript pinned);
assert pinned.config.kcore.kubernetes.package == pkgs.kubernetes;
assert runs pinned "kubelet" "kubernetes-${pkgs.kubernetes.version}";
assert runs pinned "kubelet-b" "kubernetes-${pkgs.kubernetes.version}";
assert lib.all (
  { version, host }:
  host.config.kcore.kubernetes.package.version == version
  && runs host "kubelet" "kubernetes-${version}"
  && runs host "kubelet-b" "kubernetes-${version}"
) upgraded;
pkgs.runCommand "kubernetes-module-eval-ok" { } ''
  echo ok > "$out"
''
