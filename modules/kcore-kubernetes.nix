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
      "net.ipv4.ip_forward" = lib.mkDefault 1;
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

    environment.etc."kubernetes/pki/apiserver-kubelet-client.crt" = lib.mkIf (cfg.apiserverKubeletClientCrt != "") {
      text = cfg.apiserverKubeletClientCrt;
      mode = "0644";
    };

    environment.etc."kubernetes/pki/apiserver-kubelet-client.key" = lib.mkIf (cfg.apiserverKubeletClientKey != "") {
      text = cfg.apiserverKubeletClientKey;
      mode = "0600";
    };

    environment.etc."kubernetes/pki/apiserver-etcd-client.crt" = lib.mkIf (cfg.apiserverEtcdClientCrt != "") {
      text = cfg.apiserverEtcdClientCrt;
      mode = "0644";
    };

    environment.etc."kubernetes/pki/apiserver-etcd-client.key" = lib.mkIf (cfg.apiserverEtcdClientKey != "") {
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
        tlsCertFile: /var/lib/kubelet/pki/kubelet-client-current.pem
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
