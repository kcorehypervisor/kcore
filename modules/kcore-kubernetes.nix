{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.kcore.kubernetes;
  nonEmpty = value: value != "";

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

  # etcd initial cluster string
  etcdInitialCluster = cfg.etcdInitialCluster;

  # Kubeconfig for local components
  kubeconfigPath = "/etc/kubernetes/admin.conf";
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
  };

  config = lib.mkIf cfg.enable {
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
      "net.ipv4.ip_forward" = 1;
      "net.ipv4.conf.all.forwarding" = 1;
      "net.ipv6.conf.all.forwarding" = 1;
    };

    environment.systemPackages =
      with pkgs;
      [
        kubernetes
        etcd
        cilium-cli
        kubectl
      ]
      ++ lib.optionals cfg.haEnabled [ keepalived ];

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
    };

    # Write kubeconfig if provided
    environment.etc."kubernetes/admin.conf" = lib.mkIf (cfg.kubeconfigData != "") {
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
            --cluster-cidr=${cfg.clusterCidr} \
            --cluster-name=${cfg.clusterName} \
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

    # keepalived for HA VIP (control-plane only when haEnabled)
    systemd.services.keepalived = lib.mkIf (cfg.role == "control-plane" && cfg.haEnabled) {
      description = "Keepalived for Kubernetes API HA";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      serviceConfig = {
        Type = "forking";
        PIDFile = "/run/keepalived.pid";
        ExecStart = "${pkgs.keepalived}/bin/keepalived -f /etc/keepalived/keepalived.conf -p /run/keepalived.pid";
        Restart = "always";
      };
    };

    # Cilium agent (installed via Helm/manifest after cluster bootstrap, not managed here)
    # The controller will apply Cilium manifests post-bootstrap
  };
}
