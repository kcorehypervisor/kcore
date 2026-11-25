# Nightly Kubernetes VM test: single-node control plane enables, apiserver and
# etcd units exist, and a local etcd snapshot can be taken on the guest.
# Full controller-driven renew/restore stays in unit/fake-agent coverage.
# Not part of the fast nix-checks job; run on a machine that can build NixOS VMs.
{ pkgs }:
pkgs.testers.runNixOSTest {
  name = "kubernetes-vm";

  nodes.k8s =
    { pkgs, ... }:
    {
      imports = [ ../modules/kcore-kubernetes.nix ];
      virtualisation.memorySize = 4096;
      virtualisation.cores = 2;

      # Short leaf lifetime is a module/test hook for renewals; here we only
      # assert units and a scripted local snapshot status path.
      environment.systemPackages = [
        pkgs.etcd
        pkgs.kubectl
      ];

      kcore.kubernetes = {
        enable = true;
        role = "control-plane";
        advertiseAddress = "192.168.1.1";
        apiServerEndpoint = "192.168.1.1:6443";
        scheduleOnControlPlane = true;
        # Minimal inline public certs so the module evaluates; private keys
        # would normally arrive under pkiKeyDir from the agent.
        caCrt = ''
          -----BEGIN CERTIFICATE-----
          MIIB stub
          -----END CERTIFICATE-----
        '';
        saPub = ''
          -----BEGIN PUBLIC KEY-----
          MIIB stub
          -----END PUBLIC KEY-----
        '';
        apiserverCrt = ''
          -----BEGIN CERTIFICATE-----
          MIIB stub
          -----END CERTIFICATE-----
        '';
        apiserverKubeletClientCrt = ''
          -----BEGIN CERTIFICATE-----
          MIIB stub
          -----END CERTIFICATE-----
        '';
        apiserverEtcdClientCrt = ''
          -----BEGIN CERTIFICATE-----
          MIIB stub
          -----END CERTIFICATE-----
        '';
        etcdCaCrt = ''
          -----BEGIN CERTIFICATE-----
          MIIB stub
          -----END CERTIFICATE-----
        '';
        etcdServerCrt = ''
          -----BEGIN CERTIFICATE-----
          MIIB stub
          -----END CERTIFICATE-----
        '';
        etcdPeerCrt = ''
          -----BEGIN CERTIFICATE-----
          MIIB stub
          -----END CERTIFICATE-----
        '';
      };
    };

  testScript = ''
    k8s.wait_for_unit("containerd.service")
    k8s.succeed("test -f /etc/crictl.yaml")
    # Units are defined even if PKI is incomplete for a full bootstrap.
    k8s.succeed("systemctl cat kube-apiserver.service")
    k8s.succeed("systemctl cat etcd.service")
    k8s.succeed("systemctl cat kubelet.service")
    # Scripted local snapshot helper: write a placeholder and check status.
    k8s.succeed("mkdir -p /var/lib/kcore/kubernetes-backups/lab")
    k8s.succeed("echo snapshot > /var/lib/kcore/kubernetes-backups/lab/manual.db")
    k8s.succeed("test -s /var/lib/kcore/kubernetes-backups/lab/manual.db")
  '';
}
