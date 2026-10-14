{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.kcore.kubernetes;

  # Cilium ports
  ciliumPorts = {
    healthPort = 9879;
    metricsPort = 9962;
    vxlanPort = 8472;
    genevePort = 6081;
  };

  # Kubernetes component ports
  k8sPorts = {
    apiServer = 6443;
    kubeletApi = 10250;
    kubeletReadOnly = 10255;
    kubeletHealthz = 10248;
    kubeScheduler = 10259;
    kubeControllerManager = 10257;
    etcdClient = 2379;
    etcdPeer = 2380;
  };

  # Protocol 112 must skip conntrack before keepalived transmits. The VM
  # masquerade otherwise rewrites the advert source, and a second master
  # keeps the API address.
  keepalivedNotrack = pkgs.writeShellScript "kcore-keepalived-notrack" ''
    ${pkgs.iptables}/bin/iptables -C nixos-fw -p 112 -j nixos-fw-accept 2>/dev/null || ${pkgs.iptables}/bin/iptables -A nixos-fw -p 112 -j nixos-fw-accept || true
    ${pkgs.iptables}/bin/iptables -t raw -C PREROUTING -p 112 -j CT --notrack 2>/dev/null || ${pkgs.iptables}/bin/iptables -t raw -I PREROUTING 1 -p 112 -j CT --notrack
    ${pkgs.iptables}/bin/iptables -t raw -C OUTPUT -p 112 -j CT --notrack 2>/dev/null || ${pkgs.iptables}/bin/iptables -t raw -I OUTPUT 1 -p 112 -j CT --notrack
  '';

  provisionerApply =
    kubeconfig: manifestPath:
    pkgs.writeShellScript "kcore-apply-provisioner" ''
      set -eu
      kube=${pkgs.kubectl}/bin/kubectl
      for _ in $(seq 1 90); do
        if "$kube" --kubeconfig=${kubeconfig} get --raw=/readyz >/dev/null 2>&1; then
          "$kube" --kubeconfig=${kubeconfig} apply -f ${manifestPath}
          exit 0
        fi
        sleep 2
      done
      echo "Kubernetes API was not ready for the local provisioner" >&2
      exit 1
    '';

  # Kubelet 1.33 refuses node-role.kubernetes.io/* on --node-labels and
  # exits before the node registers. Apply those labels through the API.
  roleLabelArgs =
    role: scheduleOnControlPlane:
    if role == "worker" then
      "node-role.kubernetes.io/worker="
    else if scheduleOnControlPlane then
      "node-role.kubernetes.io/control-plane= node-role.kubernetes.io/worker="
    else
      "node-role.kubernetes.io/control-plane=";

  nodeRoleApply =
    kubeconfig: hostname: labels:
    pkgs.writeShellScript "kcore-node-roles" ''
      set -eu
      kube=${pkgs.kubectl}/bin/kubectl
      for _ in $(${pkgs.coreutils}/bin/seq 1 90); do
        if "$kube" --kubeconfig=${kubeconfig} get node ${hostname} >/dev/null 2>&1; then
          "$kube" --kubeconfig=${kubeconfig} label node ${hostname} ${labels} --overwrite
          exit 0
        fi
        sleep 2
      done
      echo "node ${hostname} was not registered for role labels" >&2
      exit 1
    '';

  # etcd initial cluster string
  inherit (cfg) etcdInitialCluster;

  # Kubeconfig for local components
  kubeconfigPath = "/etc/kubernetes/admin.conf";

  # A second cluster on the same host cannot share etcd, the API port, the
  # kubelet, or Cilium's BPF pins. Each extra cluster gets its own units,
  # ports, data directory, and a bridge CNI on a private containerd.
  mkExtra =
    name: c:
    let
      apiPort = c.apiServerPort;
      etcdClient = c.etcdClientPort;
      etcdPeer = c.etcdPeerPort;
      inherit (c) kubeletPort;
      kubeletHealthz = c.kubeletHealthzPort;
      root = "/etc/kubernetes/${name}";
      pki = "${root}/pki";
      etcdDir = "/var/lib/etcd-${name}";
      kubeconfigFile = "${root}/admin.conf";
      sock = "/run/containerd-${name}/containerd.sock";
      bridge = lib.substring 0 15 ("kbr-" + lib.replaceStrings [ "-" ] [ "" ] name);
      podSubnet = if c.podSubnet != "" then c.podSubnet else c.clusterCidr;
      members = lib.filter (member: member != "") (lib.splitString "," c.etcdInitialCluster);
      ipOf =
        member:
        let
          url = lib.last (lib.splitString "=" member);
          hostport = lib.removePrefix "https://" url;
        in
        lib.head (lib.splitString ":" hostport);
      peers = lib.filter (ip: ip != c.advertiseAddress) (map ipOf members);
      octets = lib.splitString "." c.advertiseAddress;
      last = if octets == [ ] then 100 else lib.toInt (lib.last octets);
      priority = if last > 0 && last < 255 then last else 100;
      vrrpName = "kcore_" + lib.replaceStrings [ "-" ] [ "_" ] name;
    in
    lib.optionalAttrs c.enable {
      assertions = [
        {
          assertion = c.advertiseAddress != "";
          message = "kcore.kubernetes.additionalClusters.${name}.advertiseAddress is required";
        }
        {
          assertion = c.apiServerEndpoint != "";
          message = "kcore.kubernetes.additionalClusters.${name}.apiServerEndpoint is required";
        }
        {
          assertion = c.role == "control-plane" -> c.etcdInitialCluster != "";
          message = "kcore.kubernetes.additionalClusters.${name}.etcdInitialCluster is required";
        }
        {
          assertion = c.haEnabled -> (c.vipAddress != "" && c.vipInterface != "");
          message = "kcore.kubernetes.additionalClusters.${name} needs vipAddress and vipInterface";
        }
      ];

      boot.kernelModules = [
        "br_netfilter"
        "overlay"
      ];
      boot.kernel.sysctl."net.ipv4.ip_forward" = lib.mkDefault 1;

      environment.systemPackages =
        with pkgs;
        [
          kubernetes
          etcd
          kubectl
          containerd
          cni-plugins
        ]
        ++ lib.optionals c.haEnabled [ keepalived ];

      networking.firewall.allowedTCPPorts = [
        apiPort
        etcdClient
        etcdPeer
        kubeletPort
        kubeletHealthz
      ];
      networking.firewall.trustedInterfaces = [ bridge ];

      environment.etc."containerd/${name}/config.toml" = {
        mode = "0644";
        text = ''
          version = 2
          root = "/var/lib/containerd-${name}"
          state = "/run/containerd-${name}"
          [grpc]
            address = "${sock}"
          [debug]
            address = "/run/containerd-${name}/debug.sock"
          [plugins."io.containerd.grpc.v1.cri"]
            sandbox_image = "registry.k8s.io/pause:3.10"
            [plugins."io.containerd.grpc.v1.cri".cni]
              bin_dir = "${pkgs.cni-plugins}/bin"
              conf_dir = "/etc/cni/${name}/net.d"
            [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
              runtime_type = "io.containerd.runc.v2"
              [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc.options]
                SystemdCgroup = true
        '';
      };

      environment.etc."cni/${name}/net.d/10-bridge.conflist" = {
        mode = "0644";
        text = ''
          {
            "cniVersion": "1.0.0",
            "name": "${name}",
            "plugins": [
              {
                "type": "bridge",
                "bridge": "${bridge}",
                "isGateway": true,
                "ipMasq": true,
                "hairpinMode": true,
                "ipam": {
                  "type": "host-local",
                  "ranges": [[{ "subnet": "${podSubnet}" }]],
                  "routes": [{ "dst": "0.0.0.0/0" }]
                }
              },
              {
                "type": "portmap",
                "capabilities": { "portMappings": true }
              }
            ]
          }
        '';
      };

      environment.etc."cni/${name}/net.d/99-loopback.conf" = {
        mode = "0644";
        text = ''
          {
            "cniVersion": "1.0.0",
            "name": "lo",
            "type": "loopback"
          }
        '';
      };

      environment.etc."kubernetes/${name}/admin.conf" = lib.mkIf (c.kubeconfigData != "") {
        text = c.kubeconfigData;
        mode = "0600";
      };
      environment.etc."kubernetes/${name}/kubelet.conf" = lib.mkIf (c.kubeconfigData != "") {
        text = c.kubeconfigData;
        mode = "0600";
      };
      environment.etc."kubernetes/${name}/bootstrap-kubelet.conf" = lib.mkIf (c.kubeconfigData != "") {
        text = c.kubeconfigData;
        mode = "0600";
      };

      environment.etc."kubernetes/${name}/pki/ca.crt" = lib.mkIf (c.caCrt != "") {
        text = c.caCrt;
        mode = "0644";
      };
      environment.etc."kubernetes/${name}/pki/ca.key" = lib.mkIf (c.caKey != "") {
        text = c.caKey;
        mode = "0600";
      };
      environment.etc."kubernetes/${name}/pki/sa.key" = lib.mkIf (c.saKey != "") {
        text = c.saKey;
        mode = "0600";
      };
      environment.etc."kubernetes/${name}/pki/sa.pub" = lib.mkIf (c.saPub != "") {
        text = c.saPub;
        mode = "0644";
      };
      environment.etc."kubernetes/${name}/pki/apiserver.crt" = lib.mkIf (c.apiserverCrt != "") {
        text = c.apiserverCrt;
        mode = "0644";
      };
      environment.etc."kubernetes/${name}/pki/apiserver.key" = lib.mkIf (c.apiserverKey != "") {
        text = c.apiserverKey;
        mode = "0600";
      };
      environment.etc."kubernetes/${name}/pki/apiserver-kubelet-client.crt" =
        lib.mkIf (c.apiserverKubeletClientCrt != "")
          {
            text = c.apiserverKubeletClientCrt;
            mode = "0644";
          };
      environment.etc."kubernetes/${name}/pki/apiserver-kubelet-client.key" =
        lib.mkIf (c.apiserverKubeletClientKey != "")
          {
            text = c.apiserverKubeletClientKey;
            mode = "0600";
          };
      environment.etc."kubernetes/${name}/pki/apiserver-etcd-client.crt" =
        lib.mkIf (c.apiserverEtcdClientCrt != "")
          {
            text = c.apiserverEtcdClientCrt;
            mode = "0644";
          };
      environment.etc."kubernetes/${name}/pki/apiserver-etcd-client.key" =
        lib.mkIf (c.apiserverEtcdClientKey != "")
          {
            text = c.apiserverEtcdClientKey;
            mode = "0600";
          };
      environment.etc."kubernetes/${name}/pki/etcd/ca.crt" = lib.mkIf (c.etcdCaCrt != "") {
        text = c.etcdCaCrt;
        mode = "0644";
      };
      environment.etc."kubernetes/${name}/pki/etcd/ca.key" = lib.mkIf (c.etcdCaKey != "") {
        text = c.etcdCaKey;
        mode = "0600";
      };
      environment.etc."kubernetes/${name}/pki/etcd/server.crt" = lib.mkIf (c.etcdServerCrt != "") {
        text = c.etcdServerCrt;
        mode = "0644";
      };
      environment.etc."kubernetes/${name}/pki/etcd/server.key" = lib.mkIf (c.etcdServerKey != "") {
        text = c.etcdServerKey;
        mode = "0600";
      };
      environment.etc."kubernetes/${name}/pki/etcd/peer.crt" = lib.mkIf (c.etcdPeerCrt != "") {
        text = c.etcdPeerCrt;
        mode = "0644";
      };
      environment.etc."kubernetes/${name}/pki/etcd/peer.key" = lib.mkIf (c.etcdPeerKey != "") {
        text = c.etcdPeerKey;
        mode = "0600";
      };

      environment.etc."kubernetes/${name}/kubelet-config.yaml" = {
        mode = "0644";
        text = ''
          apiVersion: kubelet.config.k8s.io/v1beta1
          kind: KubeletConfiguration
          authentication:
            anonymous:
              enabled: false
            webhook:
              enabled: true
            x509:
              clientCAFile: ${pki}/ca.crt
          authorization:
            mode: Webhook
          cgroupDriver: systemd
          # The primary cluster already owns kubepods.slice. A second
          # kubelet in that hierarchy treats this cluster's pods as
          # orphans and deletes their cgroups.
          cgroupRoot: /kubepods-${name}
          clusterDNS:
            - ${c.clusterDns}
          clusterDomain: cluster.local
          containerRuntimeEndpoint: unix://${sock}
          cpuManagerReconcilePeriod: 10s
          evictionHard:
            imagefs.available: 15%
            memory.available: 100Mi
            nodefs.available: 10%
          failSwapOn: false
          hairpinMode: hairpin-veth
          healthzBindAddress: 0.0.0.0
          healthzPort: ${toString kubeletHealthz}
          imageMinimumGCAge: 2m0s
          logging:
            format: text
          maxPods: 110
          podCIDR: ${podSubnet}
          port: ${toString kubeletPort}
          resolvConf: /etc/resolv.conf
          runtimeRequestTimeout: 15m
          staticPodPath: ${root}/manifests
          streamingConnectionIdleTimeout: 4h0m0s
          syncFrequency: 1m0s
          volumePluginDir: /usr/libexec/kubernetes/kubelet-plugins/volume/exec/
        '';
      };

      systemd.services."containerd-${name}" = {
        description = "containerd for Kubernetes cluster ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        path = with pkgs; [
          runc
          iptables
          iproute2
          cni-plugins
        ];
        serviceConfig = {
          Type = "notify";
          ExecStart = "${pkgs.containerd}/bin/containerd --config /etc/containerd/${name}/config.toml";
          Restart = "always";
          RestartSec = "5s";
          Delegate = true;
          KillMode = "process";
          LimitNOFILE = 1048576;
          LimitNPROC = "infinity";
          LimitCORE = "infinity";
          TasksMax = "infinity";
          OOMScoreAdjust = -999;
          RuntimeDirectory = "containerd-${name}";
          StateDirectory = "containerd-${name}";
        };
      };

      systemd.services."etcd-${name}" = lib.mkIf (c.role == "control-plane") {
        description = "etcd for Kubernetes cluster ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "notify";
          ExecStartPre = "${pkgs.writeShellScript "kcore-etcd-clear-empty-${name}" ''
            if [ ! -e ${etcdDir}/member/snap/db ] && [ ! -d ${etcdDir}/member/wal ]; then
              rm -rf ${etcdDir}/member
            fi
          ''}";
          ExecStart = ''
            ${pkgs.etcd}/bin/etcd \
              --name=${config.networking.hostName} \
              --data-dir=${etcdDir} \
              --listen-peer-urls=https://${c.advertiseAddress}:${toString etcdPeer} \
              --listen-client-urls=https://${c.advertiseAddress}:${toString etcdClient},https://127.0.0.1:${toString etcdClient} \
              --advertise-client-urls=https://${c.advertiseAddress}:${toString etcdClient} \
              --initial-advertise-peer-urls=https://${c.advertiseAddress}:${toString etcdPeer} \
              ${
                if c.initialCluster then "--initial-cluster-state=new" else "--initial-cluster-state=existing"
              } \
              --initial-cluster=${c.etcdInitialCluster} \
              --client-cert-auth=true \
              --trusted-ca-file=${pki}/etcd/ca.crt \
              --cert-file=${pki}/etcd/server.crt \
              --key-file=${pki}/etcd/server.key \
              --peer-client-cert-auth=true \
              --peer-trusted-ca-file=${pki}/etcd/ca.crt \
              --peer-cert-file=${pki}/etcd/peer.crt \
              --peer-key-file=${pki}/etcd/peer.key
          '';
          Restart = "always";
          RestartSec = "10s";
          LimitNOFILE = 65536;
          StateDirectory = "etcd-${name}";
        };
      };

      systemd.services."kube-apiserver-${name}" = lib.mkIf (c.role == "control-plane") {
        description = "Kubernetes API server for ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [
          "network-online.target"
          "etcd-${name}.service"
        ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "notify";
          ExecStart = ''
            ${pkgs.kubernetes}/bin/kube-apiserver \
              --advertise-address=${c.advertiseAddress} \
              --bind-address=0.0.0.0 \
              --secure-port=${toString apiPort} \
              --etcd-servers=https://127.0.0.1:${toString etcdClient} \
              --etcd-cafile=${pki}/etcd/ca.crt \
              --etcd-certfile=${pki}/apiserver-etcd-client.crt \
              --etcd-keyfile=${pki}/apiserver-etcd-client.key \
              --client-ca-file=${pki}/ca.crt \
              --tls-cert-file=${pki}/apiserver.crt \
              --tls-private-key-file=${pki}/apiserver.key \
              --kubelet-client-certificate=${pki}/apiserver-kubelet-client.crt \
              --kubelet-client-key=${pki}/apiserver-kubelet-client.key \
              --service-account-key-file=${pki}/sa.pub \
              --service-account-signing-key-file=${pki}/sa.key \
              --service-account-issuer=https://${c.apiServerEndpoint} \
              --service-cluster-ip-range=${c.serviceCidr} \
              --authorization-mode=Node,RBAC \
              --enable-admission-plugins=NodeRestriction \
              --allow-privileged=true
          '';
          Restart = "always";
          RestartSec = "10s";
        };
      };

      systemd.services."kube-controller-manager-${name}" = lib.mkIf (c.role == "control-plane") {
        description = "Kubernetes controller manager for ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "kube-apiserver-${name}.service" ];
        serviceConfig = {
          Type = "simple";
          ExecStart = ''
            ${pkgs.kubernetes}/bin/kube-controller-manager \
              --bind-address=127.0.0.1 \
              --secure-port=${toString c.controllerManagerPort} \
              --allocate-node-cidrs=true \
              --cluster-cidr=${c.clusterCidr} \
              --cluster-name=${c.clusterName} \
              --node-cidr-mask-size=24 \
              --cluster-signing-cert-file=${pki}/ca.crt \
              --cluster-signing-key-file=${pki}/ca.key \
              --kubeconfig=${kubeconfigFile} \
              --leader-elect=true \
              --root-ca-file=${pki}/ca.crt \
              --service-account-private-key-file=${pki}/sa.key \
              --service-cluster-ip-range=${c.serviceCidr} \
              --use-service-account-credentials=true
          '';
          Restart = "always";
          RestartSec = "10s";
        };
      };

      systemd.services."kube-scheduler-${name}" = lib.mkIf (c.role == "control-plane") {
        description = "Kubernetes scheduler for ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "kube-apiserver-${name}.service" ];
        serviceConfig = {
          Type = "simple";
          ExecStart = ''
            ${pkgs.kubernetes}/bin/kube-scheduler \
              --bind-address=127.0.0.1 \
              --secure-port=${toString c.schedulerPort} \
              --kubeconfig=${kubeconfigFile} \
              --leader-elect=true
          '';
          Restart = "always";
          RestartSec = "10s";
        };
      };

      # Hyphens in a cgroup name become underscores in the systemd slice
      # path. kubelet looks for that slice before it will start.
      systemd.slices."kubepods_${lib.replaceStrings [ "-" ] [ "_" ] name}" = {
        description = "Pod cgroup root for Kubernetes cluster ${name}";
      };

      systemd.services."kubelet-${name}" = {
        description = "Kubelet for Kubernetes cluster ${name}";
        wantedBy = [ "multi-user.target" ];
        requires = [ "kubepods_${lib.replaceStrings [ "-" ] [ "_" ] name}.slice" ];
        after = [
          "network-online.target"
          "containerd-${name}.service"
          "kubepods_${lib.replaceStrings [ "-" ] [ "_" ] name}.slice"
        ]
        ++ lib.optionals (c.role == "control-plane") [ "kube-apiserver-${name}.service" ];
        wants = [
          "network-online.target"
          "containerd-${name}.service"
        ];
        path = with pkgs; [
          util-linux
          iptables
          iproute2
          ethtool
          kmod
        ];
        serviceConfig = {
          Type = "simple";
          ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p ${root}/manifests";
          ExecStart = ''
            ${pkgs.kubernetes}/bin/kubelet \
              --config=${root}/kubelet-config.yaml \
              --kubeconfig=${root}/kubelet.conf \
              --bootstrap-kubeconfig=${root}/bootstrap-kubelet.conf \
              --container-runtime-endpoint=unix://${sock} \
              --root-dir=/var/lib/kubelet-${name} \
              --cert-dir=/var/lib/kubelet-${name}/pki \
              --node-ip=${c.advertiseAddress} \
              --hostname-override=${config.networking.hostName}
          '';
          Restart = "always";
          RestartSec = "10s";
          KillMode = "process";
          StateDirectory = "kubelet-${name}";
        };
      };

      systemd.services."kcore-kubernetes-node-roles-${name}" = lib.mkIf (c.role == "control-plane") {
        description = "Apply node role labels for Kubernetes cluster ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [
          "kube-apiserver-${name}.service"
          "kubelet-${name}.service"
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = nodeRoleApply "${root}/admin.conf" config.networking.hostName (
            roleLabelArgs c.role c.scheduleOnControlPlane
          );
          Restart = "on-failure";
          RestartSec = "15s";
        };
      };

      systemd.services."keepalived-${name}" = lib.mkIf (c.role == "control-plane" && c.haEnabled) {
        description = "Keepalived for Kubernetes cluster ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "forking";
          PIDFile = "/run/keepalived-${name}.pid";
          ExecStartPre = keepalivedNotrack;
          # A second keepalived cannot share the primary daemon's
          # /run/vrrp.pid. -r gives this cluster its own VRRP pid file.
          ExecStart = "${pkgs.keepalived}/bin/keepalived -f /etc/keepalived/keepalived-${name}.conf -p /run/keepalived-${name}.pid -r /run/vrrp-${name}.pid";
          Restart = "always";
          RestartSec = "5s";
        };
      };

      environment.etc."kubernetes/${name}/provisioner.yaml" = lib.mkIf (c.provisionerManifest != "") {
        mode = "0644";
        text = c.provisionerManifest;
      };

      systemd.services."kcore-kubernetes-provisioner-${name}" =
        lib.mkIf (c.role == "control-plane" && c.provisionerManifest != "")
          {
            description = "Apply local volume provisioner for Kubernetes cluster ${name}";
            wantedBy = [ "multi-user.target" ];
            after = [
              "kube-apiserver-${name}.service"
              "kubelet-${name}.service"
            ];
            serviceConfig = {
              Type = "oneshot";
              RemainAfterExit = true;
              ExecStart = provisionerApply "${root}/admin.conf" "${root}/provisioner.yaml";
              Restart = "on-failure";
              RestartSec = "15s";
            };
          };

      environment.etc."keepalived/keepalived-${name}.conf" =
        lib.mkIf (c.role == "control-plane" && c.haEnabled)
          {
            mode = "0644";
            text = ''
              global_defs {
                router_id ${config.networking.hostName}-${name}
              }
              vrrp_instance ${vrrpName} {
                state BACKUP
                interface ${c.vipInterface}
                virtual_router_id ${toString c.virtualRouterId}
                priority ${toString priority}
                advert_int 1
                nopreempt
                unicast_src_ip ${c.advertiseAddress}
                unicast_peer {
              ${lib.concatMapStrings (ip: "    ${ip}\n") peers}
                }
                authentication {
                  auth_type PASS
                  auth_pass k${toString c.virtualRouterId}
                }
                virtual_ipaddress {
                  ${c.vipAddress}/24
                }
              }
            '';
          };
    };

  # Built only when a merged config value is read. Building it while the
  # module config is still being assembled recurses through cfg.
  extras = lib.mapAttrsToList mkExtra cfg.additionalClusters;
in
{
  options.kcore.kubernetes = {
    enable = lib.mkEnableOption "kcore Kubernetes cluster services";

    role = lib.mkOption {
      type = lib.types.enum [
        "control-plane"
        "worker"
      ];
      default = "worker";
      description = "Node role in the Kubernetes cluster.";
    };

    scheduleOnControlPlane = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Register this control-plane node as a schedulable worker.";
    };

    clusterName = lib.mkOption {
      type = lib.types.str;
      default = "kcore-k8s";
      description = "Kubernetes cluster name.";
    };

    clusterCidr = lib.mkOption {
      type = lib.types.str;
      default = "10.244.0.0/16";
      description = "Pod network CIDR.";
    };

    serviceCidr = lib.mkOption {
      type = lib.types.str;
      default = "10.96.0.0/12";
      description = "Service network CIDR.";
    };

    clusterDns = lib.mkOption {
      type = lib.types.str;
      default = "10.96.0.10";
      description = "DNS service IP (CoreDNS).";
    };

    advertiseAddress = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "This node's advertised address for kubelet/API server.";
    };

    iface = lib.mkOption {
      type = lib.types.str;
      default = "enp6s0";
      description = "Primary network interface for Kubernetes traffic.";
    };

    apiServerEndpoint = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "API server endpoint (VIP:port for HA, or first control plane).";
    };

    initialCluster = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "True for the first control plane node (bootstraps etcd).";
    };

    etcdInitialCluster = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "etcd initial cluster string (name1=https://ip1:2380,name2=https://ip2:2380,...).";
    };

    # CNI Configuration
    cni = lib.mkOption {
      type = lib.types.str;
      default = "cilium";
      description = "CNI plugin (only 'cilium' is supported).";
    };

    ciliumMode = lib.mkOption {
      type = lib.types.str;
      default = "native-routing";
      description = "Cilium routing mode: native-routing or tunnel.";
    };

    ciliumMtu = lib.mkOption {
      type = lib.types.int;
      default = 1500;
      description = "Cilium MTU.";
    };

    ciliumTunnel = lib.mkOption {
      type = lib.types.str;
      default = "disabled";
      description = "Cilium tunnel mode: disabled, vxlan, or geneve.";
    };

    ciliumAutoDirectNodeRoutes = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Enable Cilium auto-direct node routes (native routing).";
    };

    ciliumLoadBalancerIpPools = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Cilium LoadBalancer IP pool CIDRs (e.g., ['192.168.40.220/29']).";
    };

    # High Availability
    haEnabled = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable HA with keepalived VIP for API server.";
    };

    vipAddress = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Virtual IP address for HA API server.";
    };

    vipInterface = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Interface to bind the VIP.";
    };

    # Kubeconfig data (written by controller)
    kubeconfigData = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Base64-encoded kubeconfig content (written by node-agent).";
    };

    provisionerManifest = lib.mkOption {
      type = lib.types.lines;
      default = "";
      description = ''
        Local volume provisioner manifests. The control plane applies them
        after the API is ready. Empty leaves the cluster without that apply.
      '';
    };

    # PKI Certificates - Kubernetes CA
    caCrt = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Kubernetes CA certificate (PEM).";
    };

    caKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Kubernetes CA private key (PEM).";
    };

    # Service Account keys
    saKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Service account signing private key (PEM).";
    };

    saPub = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Service account public key (PEM).";
    };

    # API Server certificates
    apiserverCrt = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "API server certificate (PEM).";
    };

    apiserverKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "API server private key (PEM).";
    };

    apiserverKubeletClientCrt = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "API server kubelet client certificate (PEM).";
    };

    apiserverKubeletClientKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "API server kubelet client private key (PEM).";
    };

    apiserverEtcdClientCrt = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "API server etcd client certificate (PEM).";
    };

    apiserverEtcdClientKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "API server etcd client private key (PEM).";
    };

    # etcd CA and certificates
    etcdCaCrt = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "etcd CA certificate (PEM).";
    };

    etcdCaKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "etcd CA private key (PEM).";
    };

    etcdServerCrt = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "etcd server certificate (PEM).";
    };

    etcdServerKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "etcd server private key (PEM).";
    };

    etcdPeerCrt = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "etcd peer certificate (PEM).";
    };

    etcdPeerKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "etcd peer private key (PEM).";
    };

    additionalClusters = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            enable = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Run this cluster beside the primary one.";
            };
            role = lib.mkOption {
              type = lib.types.enum [
                "control-plane"
                "worker"
              ];
              default = "control-plane";
            };
            scheduleOnControlPlane = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Register this control-plane node as a schedulable worker.";
            };
            clusterName = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            clusterCidr = lib.mkOption {
              type = lib.types.str;
              default = "10.245.0.0/16";
            };
            serviceCidr = lib.mkOption {
              type = lib.types.str;
              default = "10.112.0.0/12";
            };
            clusterDns = lib.mkOption {
              type = lib.types.str;
              default = "10.112.0.10";
            };
            advertiseAddress = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            iface = lib.mkOption {
              type = lib.types.str;
              default = "br0";
            };
            apiServerEndpoint = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            initialCluster = lib.mkOption {
              type = lib.types.bool;
              default = true;
            };
            etcdInitialCluster = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            haEnabled = lib.mkOption {
              type = lib.types.bool;
              default = false;
            };
            vipAddress = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            vipInterface = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            apiServerPort = lib.mkOption {
              type = lib.types.port;
              default = 7443;
            };
            etcdClientPort = lib.mkOption {
              type = lib.types.port;
              default = 3379;
            };
            etcdPeerPort = lib.mkOption {
              type = lib.types.port;
              default = 3380;
            };
            kubeletPort = lib.mkOption {
              type = lib.types.port;
              default = 11250;
            };
            kubeletHealthzPort = lib.mkOption {
              type = lib.types.port;
              default = 11248;
            };
            controllerManagerPort = lib.mkOption {
              type = lib.types.port;
              default = 11257;
            };
            schedulerPort = lib.mkOption {
              type = lib.types.port;
              default = 11259;
            };
            virtualRouterId = lib.mkOption {
              type = lib.types.ints.between 1 255;
              default = 41;
            };
            podSubnet = lib.mkOption {
              type = lib.types.str;
              default = "";
              description = "Per-node pod CIDR for the bridge CNI.";
            };
            kubeconfigData = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            provisionerManifest = lib.mkOption {
              type = lib.types.lines;
              default = "";
              description = "Local volume provisioner manifests for this cluster.";
            };
            caCrt = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            caKey = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            saKey = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            saPub = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            apiserverCrt = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            apiserverKey = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            apiserverKubeletClientCrt = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            apiserverKubeletClientKey = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            apiserverEtcdClientCrt = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            apiserverEtcdClientKey = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            etcdCaCrt = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            etcdCaKey = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            etcdServerCrt = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            etcdServerKey = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            etcdPeerCrt = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
            etcdPeerKey = lib.mkOption {
              type = lib.types.str;
              default = "";
            };
          };
        }
      );
      default = { };
      description = ''
        Extra Kubernetes clusters on this same node. The primary cluster keeps
        the standard ports and Cilium. Each extra cluster uses its own etcd,
        API server, kubelet, containerd, and bridge network.
      '';
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      assertions = [
        {
          assertion = cfg.advertiseAddress != "";
          message = "kcore.kubernetes.advertiseAddress is required";
        }
        {
          assertion = cfg.apiServerEndpoint != "";
          message = "kcore.kubernetes.apiServerEndpoint is required";
        }
        {
          assertion = cfg.role == "control-plane" -> cfg.etcdInitialCluster != "";
          message = "kcore.kubernetes.etcdInitialCluster is required for control-plane nodes";
        }
        {
          assertion = cfg.haEnabled -> (cfg.vipAddress != "" && cfg.vipInterface != "");
          message = "kcore.kubernetes.vipAddress and vipInterface are required when haEnabled";
        }
        {
          assertion = cfg.cni == "cilium";
          message = "Only 'cilium' CNI is currently supported";
        }
      ];

      # Kernel modules for Kubernetes
      boot.kernelModules = [
        "br_netfilter"
        "overlay"
      ];

      boot.kernel.sysctl = {
        "net.bridge.bridge-nf-call-iptables" = 1;
        "net.bridge.bridge-nf-call-ip6tables" = 1;
        "net.ipv4.ip_forward" = lib.mkDefault 1;
        "net.ipv4.conf.all.forwarding" = 1;
        "net.ipv6.conf.all.forwarding" = 1;
        # The kernel default of 128 inotify instances is already used by one
        # kubelet plus the VMs on this host. A second kubelet then exits in
        # cAdvisor with "too many open files" and never joins.
        "fs.inotify.max_user_instances" = 1024;
      };

      environment.systemPackages =
        with pkgs;
        [
          kubernetes
          etcd
          cilium-cli
          kubectl
          containerd
        ]
        ++ lib.optionals cfg.haEnabled [ keepalived ];

      # containerd configuration
      virtualisation.containerd = {
        enable = true;
        settings = {
          version = 2;
          plugins."io.containerd.grpc.v1.cri" = {
            cni = {
              bin_dir = "/opt/cni/bin";
              conf_dir = "/etc/cni/net.d";
            };
            containerd = {
              runtimes = {
                runc = {
                  runtime_type = "io.containerd.runc.v2";
                  options = {
                    SystemdCgroup = true;
                  };
                };
              };
            };
          };
        };
      };

      # Firewall configuration
      networking.firewall = {
        enable = true;
        allowedTCPPorts = [
          k8sPorts.apiServer
          k8sPorts.kubeletApi
          k8sPorts.kubeletHealthz
          k8sPorts.etcdClient
          k8sPorts.etcdPeer
        ]
        ++ lib.optionals (cfg.role == "control-plane") [
          k8sPorts.kubeScheduler
          k8sPorts.kubeControllerManager
        ];
        allowedUDPPorts =
          lib.optionals (cfg.ciliumTunnel == "vxlan") [ ciliumPorts.vxlanPort ]
          ++ lib.optionals (cfg.ciliumTunnel == "geneve") [ ciliumPorts.genevePort ];
        trustedInterfaces = [
          "cni0"
          "cilium_host"
          "cilium_net"
        ];
        # Unicast keepalived speaks IP protocol 112. The filter accept
        # stops the firewall from dropping adverts. The raw NOTRACK rules
        # stop conntrack from applying the VM-network masquerade, which
        # otherwise rewrites the advert source to the VM gateway and the
        # real master is never heard.
        extraCommands =
          lib.mkIf (cfg.haEnabled || lib.any (c: c.haEnabled) (lib.attrValues cfg.additionalClusters))
            ''
              iptables -C nixos-fw -p 112 -j nixos-fw-accept 2>/dev/null || iptables -A nixos-fw -p 112 -j nixos-fw-accept
              iptables -t raw -C PREROUTING -p 112 -j CT --notrack 2>/dev/null || iptables -t raw -I PREROUTING 1 -p 112 -j CT --notrack
              iptables -t raw -C OUTPUT -p 112 -j CT --notrack 2>/dev/null || iptables -t raw -I OUTPUT 1 -p 112 -j CT --notrack
            '';
        # Leave the raw NOTRACK rules in place when the firewall unit
        # reloads. Deleting them opens a window where VM masquerade
        # rewrites keepalived adverts for every cluster on this host.
        extraStopCommands =
          lib.mkIf (cfg.haEnabled || lib.any (c: c.haEnabled) (lib.attrValues cfg.additionalClusters))
            ''
              while iptables -D nixos-fw -p 112 -j nixos-fw-accept 2>/dev/null; do :; done
            '';
      };

      # Write kubeconfig if provided
      environment.etc."kubernetes/admin.conf" = lib.mkIf (cfg.kubeconfigData != "") {
        text = cfg.kubeconfigData;
        mode = "0600";
      };

      # Write PKI certificates
      environment.etc."kubernetes/pki/ca.crt" = lib.mkIf (cfg.caCrt != "") {
        text = cfg.caCrt;
        mode = "0644";
      };

      environment.etc."kubernetes/pki/ca.key" = lib.mkIf (cfg.caKey != "") {
        text = cfg.caKey;
        mode = "0600";
      };

      environment.etc."kubernetes/pki/sa.key" = lib.mkIf (cfg.saKey != "") {
        text = cfg.saKey;
        mode = "0600";
      };

      environment.etc."kubernetes/pki/sa.pub" = lib.mkIf (cfg.saPub != "") {
        text = cfg.saPub;
        mode = "0644";
      };

      environment.etc."kubernetes/pki/apiserver.crt" = lib.mkIf (cfg.apiserverCrt != "") {
        text = cfg.apiserverCrt;
        mode = "0644";
      };

      environment.etc."kubernetes/pki/apiserver.key" = lib.mkIf (cfg.apiserverKey != "") {
        text = cfg.apiserverKey;
        mode = "0600";
      };

      environment.etc."kubernetes/pki/apiserver-kubelet-client.crt" =
        lib.mkIf (cfg.apiserverKubeletClientCrt != "")
          {
            text = cfg.apiserverKubeletClientCrt;
            mode = "0644";
          };

      environment.etc."kubernetes/pki/apiserver-kubelet-client.key" =
        lib.mkIf (cfg.apiserverKubeletClientKey != "")
          {
            text = cfg.apiserverKubeletClientKey;
            mode = "0600";
          };

      environment.etc."kubernetes/pki/apiserver-etcd-client.crt" =
        lib.mkIf (cfg.apiserverEtcdClientCrt != "")
          {
            text = cfg.apiserverEtcdClientCrt;
            mode = "0644";
          };

      environment.etc."kubernetes/pki/apiserver-etcd-client.key" =
        lib.mkIf (cfg.apiserverEtcdClientKey != "")
          {
            text = cfg.apiserverEtcdClientKey;
            mode = "0600";
          };

      environment.etc."kubernetes/pki/etcd/ca.crt" = lib.mkIf (cfg.etcdCaCrt != "") {
        text = cfg.etcdCaCrt;
        mode = "0644";
      };

      environment.etc."kubernetes/pki/etcd/ca.key" = lib.mkIf (cfg.etcdCaKey != "") {
        text = cfg.etcdCaKey;
        mode = "0600";
      };

      environment.etc."kubernetes/pki/etcd/server.crt" = lib.mkIf (cfg.etcdServerCrt != "") {
        text = cfg.etcdServerCrt;
        mode = "0644";
      };

      environment.etc."kubernetes/pki/etcd/server.key" = lib.mkIf (cfg.etcdServerKey != "") {
        text = cfg.etcdServerKey;
        mode = "0600";
      };

      environment.etc."kubernetes/pki/etcd/peer.crt" = lib.mkIf (cfg.etcdPeerCrt != "") {
        text = cfg.etcdPeerCrt;
        mode = "0644";
      };

      environment.etc."kubernetes/pki/etcd/peer.key" = lib.mkIf (cfg.etcdPeerKey != "") {
        text = cfg.etcdPeerKey;
        mode = "0600";
      };

      # Kubelet configuration file
      environment.etc."kubernetes/kubelet-config.yaml" = {
        text = ''
          apiVersion: kubelet.config.k8s.io/v1beta1
          kind: KubeletConfiguration
          authentication:
            anonymous:
              enabled: false
            webhook:
              enabled: true
            x509:
              clientCAFile: /etc/kubernetes/pki/ca.crt
          authorization:
            mode: Webhook
          cgroupDriver: systemd
          clusterDNS:
            - ${cfg.clusterDns}
          clusterDomain: cluster.local
          containerRuntimeEndpoint: unix:///run/containerd/containerd.sock
          cpuManagerReconcilePeriod: 10s
          evictionHard:
            imagefs.available: 15%
            memory.available: 100Mi
            nodefs.available: 10%
          failSwapOn: false
          hairpinMode: hairpin-veth
          healthzBindAddress: 0.0.0.0
          healthzPort: ${toString k8sPorts.kubeletHealthz}
          imageMinimumGCAge: 2m0s
          logging:
            format: text
          maxPods: 110
          podCIDR: ${cfg.clusterCidr}
          port: ${toString k8sPorts.kubeletApi}
          resolvConf: /etc/resolv.conf
          runtimeRequestTimeout: 15m
          staticPodPath: /etc/kubernetes/manifests
          streamingConnectionIdleTimeout: 4h0m0s
          syncFrequency: 1m0s
          volumePluginDir: /usr/libexec/kubernetes/kubelet-plugins/volume/exec/
        '';
        mode = "0644";
      };

      # Bootstrap kubeconfig for kubelet (minimal, will be replaced after TLS bootstrap)
      environment.etc."kubernetes/bootstrap-kubelet.conf" = lib.mkIf (cfg.kubeconfigData != "") {
        text = cfg.kubeconfigData;
        mode = "0600";
      };

      # Kubelet kubeconfig (will be created by kubelet TLS bootstrap)
      environment.etc."kubernetes/kubelet.conf" = lib.mkIf (cfg.kubeconfigData != "") {
        text = cfg.kubeconfigData;
        mode = "0600";
      };

      # etcd service (control-plane only)
      systemd.services.etcd = lib.mkIf (cfg.role == "control-plane") {
        description = "etcd - highly-available key value store";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];

        serviceConfig = {
          Type = "notify";
          # A crash before the first WAL (missing peer certs) leaves an empty
          # member directory. etcd then refuses --initial-cluster-state=new.
          # Keep a directory that already has a snapshot or WAL.
          ExecStartPre = "${pkgs.writeShellScript "kcore-etcd-clear-empty-member" ''
            if [ ! -e /var/lib/etcd/member/snap/db ] && [ ! -d /var/lib/etcd/member/wal ]; then
              rm -rf /var/lib/etcd/member
            fi
          ''}";
          ExecStart = ''
            ${pkgs.etcd}/bin/etcd \
              --name=${config.networking.hostName} \
              --data-dir=/var/lib/etcd \
              --listen-peer-urls=https://${cfg.advertiseAddress}:${toString k8sPorts.etcdPeer} \
              --listen-client-urls=https://${cfg.advertiseAddress}:${toString k8sPorts.etcdClient},https://127.0.0.1:${toString k8sPorts.etcdClient} \
              --advertise-client-urls=https://${cfg.advertiseAddress}:${toString k8sPorts.etcdClient} \
              --initial-advertise-peer-urls=https://${cfg.advertiseAddress}:${toString k8sPorts.etcdPeer} \
              ${
                if cfg.initialCluster then "--initial-cluster-state=new" else "--initial-cluster-state=existing"
              } \
              --initial-cluster=${etcdInitialCluster} \
              --client-cert-auth=true \
              --trusted-ca-file=/etc/kubernetes/pki/etcd/ca.crt \
              --cert-file=/etc/kubernetes/pki/etcd/server.crt \
              --key-file=/etc/kubernetes/pki/etcd/server.key \
              --peer-client-cert-auth=true \
              --peer-trusted-ca-file=/etc/kubernetes/pki/etcd/ca.crt \
              --peer-cert-file=/etc/kubernetes/pki/etcd/peer.crt \
              --peer-key-file=/etc/kubernetes/pki/etcd/peer.key
          '';
          Restart = "always";
          RestartSec = "10s";
          LimitNOFILE = 65536;
          StateDirectory = "etcd";
        };
      };

      # kube-apiserver (control-plane only)
      systemd.services.kube-apiserver = lib.mkIf (cfg.role == "control-plane") {
        description = "Kubernetes API Server";
        wantedBy = [ "multi-user.target" ];
        after = [
          "network-online.target"
          "etcd.service"
        ];
        wants = [ "network-online.target" ];

        serviceConfig = {
          Type = "notify";
          ExecStart = ''
            ${pkgs.kubernetes}/bin/kube-apiserver \
              --advertise-address=${cfg.advertiseAddress} \
              --bind-address=0.0.0.0 \
              --secure-port=${toString k8sPorts.apiServer} \
              --etcd-servers=https://127.0.0.1:${toString k8sPorts.etcdClient} \
              --etcd-cafile=/etc/kubernetes/pki/etcd/ca.crt \
              --etcd-certfile=/etc/kubernetes/pki/apiserver-etcd-client.crt \
              --etcd-keyfile=/etc/kubernetes/pki/apiserver-etcd-client.key \
              --client-ca-file=/etc/kubernetes/pki/ca.crt \
              --tls-cert-file=/etc/kubernetes/pki/apiserver.crt \
              --tls-private-key-file=/etc/kubernetes/pki/apiserver.key \
              --kubelet-client-certificate=/etc/kubernetes/pki/apiserver-kubelet-client.crt \
              --kubelet-client-key=/etc/kubernetes/pki/apiserver-kubelet-client.key \
              --service-account-key-file=/etc/kubernetes/pki/sa.pub \
              --service-account-signing-key-file=/etc/kubernetes/pki/sa.key \
              --service-account-issuer=https://${cfg.apiServerEndpoint} \
              --service-cluster-ip-range=${cfg.serviceCidr} \
              --authorization-mode=Node,RBAC \
              --enable-admission-plugins=NodeRestriction \
              --allow-privileged=true
          '';
          Restart = "always";
          RestartSec = "10s";
        };
      };

      # kube-controller-manager (control-plane only)
      systemd.services.kube-controller-manager = lib.mkIf (cfg.role == "control-plane") {
        description = "Kubernetes Controller Manager";
        wantedBy = [ "multi-user.target" ];
        after = [ "kube-apiserver.service" ];

        serviceConfig = {
          Type = "simple";
          ExecStart = ''
            ${pkgs.kubernetes}/bin/kube-controller-manager \
              --bind-address=127.0.0.1 \
              --allocate-node-cidrs=true \
              --cluster-cidr=${cfg.clusterCidr} \
              --cluster-name=${cfg.clusterName} \
              --node-cidr-mask-size=24 \
              --cluster-signing-cert-file=/etc/kubernetes/pki/ca.crt \
              --cluster-signing-key-file=/etc/kubernetes/pki/ca.key \
              --kubeconfig=${kubeconfigPath} \
              --leader-elect=true \
              --root-ca-file=/etc/kubernetes/pki/ca.crt \
              --service-account-private-key-file=/etc/kubernetes/pki/sa.key \
              --service-cluster-ip-range=${cfg.serviceCidr} \
              --use-service-account-credentials=true
          '';
          Restart = "always";
          RestartSec = "10s";
        };
      };

      # kube-scheduler (control-plane only)
      systemd.services.kube-scheduler = lib.mkIf (cfg.role == "control-plane") {
        description = "Kubernetes Scheduler";
        wantedBy = [ "multi-user.target" ];
        after = [ "kube-apiserver.service" ];

        serviceConfig = {
          Type = "simple";
          ExecStart = ''
            ${pkgs.kubernetes}/bin/kube-scheduler \
              --bind-address=127.0.0.1 \
              --kubeconfig=${kubeconfigPath} \
              --leader-elect=true
          '';
          Restart = "always";
          RestartSec = "10s";
        };
      };

      # kubelet (all nodes)
      systemd.services.kubelet = {
        description = "Kubernetes Kubelet";
        wantedBy = [ "multi-user.target" ];
        after = [
          "network-online.target"
        ]
        ++ lib.optionals (cfg.role == "control-plane") [ "kube-apiserver.service" ];
        wants = [ "network-online.target" ];
        # Volume setup execs mount(8). The default unit PATH is the NixOS
        # activation set, which does not include util-linux.
        path = with pkgs; [
          util-linux
          iptables
          iproute2
          ethtool
          kmod
        ];

        serviceConfig = {
          Type = "simple";
          ExecStart = ''
            ${pkgs.kubernetes}/bin/kubelet \
              --config=/etc/kubernetes/kubelet-config.yaml \
              --kubeconfig=/etc/kubernetes/kubelet.conf \
              --bootstrap-kubeconfig=/etc/kubernetes/bootstrap-kubelet.conf \
              --container-runtime-endpoint=unix:///run/containerd/containerd.sock \
              --node-ip=${cfg.advertiseAddress} \
              --hostname-override=${config.networking.hostName}
          '';
          Restart = "always";
          RestartSec = "10s";
          KillMode = "process";
        };
      };

      systemd.services.kcore-kubernetes-node-roles = lib.mkIf (cfg.role == "control-plane") {
        description = "Apply Kubernetes node role labels";
        wantedBy = [ "multi-user.target" ];
        after = [
          "kube-apiserver.service"
          "kubelet.service"
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = nodeRoleApply "/etc/kubernetes/admin.conf" config.networking.hostName (
            roleLabelArgs cfg.role cfg.scheduleOnControlPlane
          );
          Restart = "on-failure";
          RestartSec = "15s";
        };
      };

      # keepalived for HA VIP (control-plane only when haEnabled)
      systemd.services.keepalived = lib.mkIf (cfg.role == "control-plane" && cfg.haEnabled) {
        description = "Keepalived for Kubernetes API HA";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];

        serviceConfig = {
          Type = "forking";
          PIDFile = "/run/keepalived.pid";
          ExecStartPre = keepalivedNotrack;
          ExecStart = "${pkgs.keepalived}/bin/keepalived -f /etc/keepalived/keepalived.conf -p /run/keepalived.pid";
          Restart = "always";
          RestartSec = "5s";
        };
      };

      environment.etc."kubernetes/provisioner.yaml" = lib.mkIf (cfg.provisionerManifest != "") {
        text = cfg.provisionerManifest;
        mode = "0644";
      };

      systemd.services.kcore-kubernetes-provisioner =
        lib.mkIf (cfg.role == "control-plane" && cfg.provisionerManifest != "")
          {
            description = "Apply the local volume provisioner";
            wantedBy = [ "multi-user.target" ];
            after = [
              "kube-apiserver.service"
              "kubelet.service"
            ];
            serviceConfig = {
              Type = "oneshot";
              RemainAfterExit = true;
              ExecStart = provisionerApply "/etc/kubernetes/admin.conf" "/etc/kubernetes/provisioner.yaml";
              Restart = "on-failure";
              RestartSec = "15s";
            };
          };

      # VRRP for the API VIP. The unit used to start with no config file, so
      # keepalived exited before it could own vipAddress.
      environment.etc."keepalived/keepalived.conf" =
        lib.mkIf (cfg.role == "control-plane" && cfg.haEnabled)
          {
            mode = "0644";
            text =
              let
                members = lib.filter (member: member != "") (lib.splitString "," cfg.etcdInitialCluster);
                ipOf =
                  member:
                  let
                    url = lib.last (lib.splitString "=" member);
                    hostport = lib.removePrefix "https://" url;
                  in
                  lib.head (lib.splitString ":" hostport);
                peers = lib.filter (ip: ip != cfg.advertiseAddress) (map ipOf members);
                octets = lib.splitString "." cfg.advertiseAddress;
                last = if octets == [ ] then 100 else lib.toInt (lib.last octets);
                priority = if last > 0 && last < 255 then last else 100;
              in
              ''
                global_defs {
                  router_id ${config.networking.hostName}
                }
                vrrp_instance kcore_api {
                  state BACKUP
                  interface ${cfg.vipInterface}
                  virtual_router_id 40
                  priority ${toString priority}
                  advert_int 1
                  nopreempt
                  unicast_src_ip ${cfg.advertiseAddress}
                  unicast_peer {
                ${lib.concatMapStrings (ip: "    ${ip}\n") peers}
                  }
                  authentication {
                    auth_type PASS
                    auth_pass kcoreapi
                  }
                  virtual_ipaddress {
                    ${cfg.vipAddress}/24
                  }
                }
              '';
          };

      # The primary cluster's Cilium is installed into the API. An extra
      # cluster on this host uses the bridge CNI from mkExtra, because a
      # second Cilium agent would share the primary BPF pins.
    })
    {
      assertions = lib.concatLists (map (extra: extra.assertions or [ ]) extras);
      boot.kernelModules = lib.concatLists (map (extra: extra.boot.kernelModules or [ ]) extras);
      boot.kernel.sysctl = lib.mkMerge (map (extra: extra.boot.kernel.sysctl or { }) extras);
      environment.systemPackages = lib.concatLists (
        map (extra: extra.environment.systemPackages or [ ]) extras
      );
      environment.etc = lib.mkMerge (map (extra: extra.environment.etc or { }) extras);
      networking.firewall.allowedTCPPorts = lib.concatLists (
        map (extra: extra.networking.firewall.allowedTCPPorts or [ ]) extras
      );
      networking.firewall.trustedInterfaces = lib.concatLists (
        map (extra: extra.networking.firewall.trustedInterfaces or [ ]) extras
      );
      systemd.services = lib.mkMerge (map (extra: extra.systemd.services or { }) extras);
    }
  ];
}
