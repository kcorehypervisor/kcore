# Nightly Kubernetes VM test: single-node control plane with generated PKI,
# units reach active, etcd answers health, and a local etcd snapshot succeeds.
# Controller-driven renew/restore stays in unit/fake-agent coverage.
# Not part of the fast nix-checks job; run on a machine that can build NixOS VMs.
{ pkgs }:
let
  # Deterministic lab PKI for 192.168.1.1 (advertise + API). Private keys stay
  # inside this derivation; the VM copies them to pkiKeyDir at activation.
  k8sPki = pkgs.runCommand "kubernetes-vm-pki" { buildInputs = [ pkgs.openssl ]; } ''
        set -euo pipefail
        mkdir -p $out/etcd

        openssl genrsa -out $out/ca.key 2048
        openssl req -x509 -new -nodes -key $out/ca.key -subj "/CN=kubernetes-ca" \
          -days 3650 -out $out/ca.crt
        openssl genrsa -out $out/etcd/ca.key 2048
        openssl req -x509 -new -nodes -key $out/etcd/ca.key -subj "/CN=etcd-ca" \
          -days 3650 -out $out/etcd/ca.crt
        openssl genrsa -out $out/sa.key 2048
        openssl rsa -in $out/sa.key -pubout -out $out/sa.pub

        cat > $out/openssl-san.cnf <<'CNF'
    [req]
    distinguished_name = req
    req_extensions = v3_req
    prompt = no
    [req_distinguished_name]
    [v3_req]
    subjectAltName = @alt
    [alt]
    IP.1 = 192.168.1.1
    IP.2 = 127.0.0.1
    DNS.1 = kubernetes
    DNS.2 = localhost
    CNF

        # Apiserver (server)
        openssl genrsa -out $out/apiserver.key 2048
        openssl req -new -key $out/apiserver.key -subj "/CN=kube-apiserver" \
          -out $out/apiserver.csr -config $out/openssl-san.cnf
        openssl x509 -req -in $out/apiserver.csr -CA $out/ca.crt -CAkey $out/ca.key \
          -CAcreateserial -out $out/apiserver.crt -days 365 \
          -extensions v3_req -extfile $out/openssl-san.cnf

        # Apiserver -> kubelet (client)
        openssl genrsa -out $out/apiserver-kubelet-client.key 2048
        openssl req -new -key $out/apiserver-kubelet-client.key \
          -subj "/CN=kube-apiserver-kubelet-client/O=system:masters" \
          -out $out/apiserver-kubelet-client.csr
        openssl x509 -req -in $out/apiserver-kubelet-client.csr -CA $out/ca.crt \
          -CAkey $out/ca.key -CAcreateserial -out $out/apiserver-kubelet-client.crt -days 365

        # Apiserver -> etcd (client)
        openssl genrsa -out $out/apiserver-etcd-client.key 2048
        openssl req -new -key $out/apiserver-etcd-client.key \
          -subj "/CN=apiserver-etcd-client" -out $out/apiserver-etcd-client.csr
        openssl x509 -req -in $out/apiserver-etcd-client.csr -CA $out/etcd/ca.crt \
          -CAkey $out/etcd/ca.key -CAcreateserial -out $out/apiserver-etcd-client.crt -days 365

        # etcd server + peer
        for name in server peer; do
          openssl genrsa -out $out/etcd/$name.key 2048
          openssl req -new -key $out/etcd/$name.key -subj "/CN=etcd-$name" \
            -out $out/etcd/$name.csr -config $out/openssl-san.cnf
          openssl x509 -req -in $out/etcd/$name.csr -CA $out/etcd/ca.crt \
            -CAkey $out/etcd/ca.key -CAcreateserial -out $out/etcd/$name.crt -days 365 \
            -extensions v3_req -extfile $out/openssl-san.cnf
        done

        # admin + kubelet kubeconfigs (cluster CA as trust root)
        b64ca=$(base64 -w0 $out/ca.crt)
        openssl genrsa -out $out/admin.key 2048
        openssl req -new -key $out/admin.key -subj "/CN=kubernetes-admin/O=system:masters" \
          -out $out/admin.csr
        openssl x509 -req -in $out/admin.csr -CA $out/ca.crt -CAkey $out/ca.key \
          -CAcreateserial -out $out/admin.crt -days 365
        b64admin=$(base64 -w0 $out/admin.crt)
        b64adminkey=$(base64 -w0 $out/admin.key)
        cat > $out/admin.conf <<EOF
    apiVersion: v1
    kind: Config
    clusters:
    - cluster:
        certificate-authority-data: $b64ca
        server: https://192.168.1.1:6443
      name: lab
    contexts:
    - context:
        cluster: lab
        user: admin
      name: lab
    current-context: lab
    users:
    - name: admin
      user:
        client-certificate-data: $b64admin
        client-key-data: $b64adminkey
    EOF

        openssl genrsa -out $out/kubelet.key 2048
        openssl req -new -key $out/kubelet.key \
          -subj "/CN=system:node:k8s/O=system:nodes" -out $out/kubelet.csr
        openssl x509 -req -in $out/kubelet.csr -CA $out/ca.crt -CAkey $out/ca.key \
          -CAcreateserial -out $out/kubelet.crt -days 30
        b64k=$(base64 -w0 $out/kubelet.crt)
        b64kk=$(base64 -w0 $out/kubelet.key)
        cat > $out/kubelet.conf <<EOF
    apiVersion: v1
    kind: Config
    clusters:
    - cluster:
        certificate-authority-data: $b64ca
        server: https://192.168.1.1:6443
      name: lab
    contexts:
    - context:
        cluster: lab
        user: kubelet
      name: lab
    current-context: lab
    users:
    - name: kubelet
      user:
        client-certificate-data: $b64k
        client-key-data: $b64kk
    EOF
  '';
in
pkgs.testers.runNixOSTest {
  name = "kubernetes-vm";

  nodes.k8s =
    { pkgs, ... }:
    {
      imports = [ ../modules/kcore-kubernetes.nix ];
      virtualisation.memorySize = 6144;
      virtualisation.cores = 2;

      environment.systemPackages = [
        pkgs.etcd
        pkgs.kubectl
        pkgs.openssl
      ];

      # Agent-style secret dir: keys + kubeconfigs, not world-readable etc text.
      system.activationScripts.kcore-k8s-pki.text = ''
        mkdir -p /var/lib/kcore/kubernetes/lab/pki/etcd
        cp -a ${k8sPki}/. /var/lib/kcore/kubernetes/lab/pki/
        chmod -R go-rwx /var/lib/kcore/kubernetes/lab/pki
      '';

      kcore.kubernetes = {
        enable = true;
        role = "control-plane";
        advertiseAddress = "192.168.1.1";
        apiServerEndpoint = "192.168.1.1:6443";
        scheduleOnControlPlane = true;
        pkiKeyDir = "/var/lib/kcore/kubernetes/lab/pki";
        caCrt = builtins.readFile "${k8sPki}/ca.crt";
        saPub = builtins.readFile "${k8sPki}/sa.pub";
        apiserverCrt = builtins.readFile "${k8sPki}/apiserver.crt";
        apiserverKubeletClientCrt = builtins.readFile "${k8sPki}/apiserver-kubelet-client.crt";
        apiserverEtcdClientCrt = builtins.readFile "${k8sPki}/apiserver-etcd-client.crt";
        etcdCaCrt = builtins.readFile "${k8sPki}/etcd/ca.crt";
        etcdServerCrt = builtins.readFile "${k8sPki}/etcd/server.crt";
        etcdPeerCrt = builtins.readFile "${k8sPki}/etcd/peer.crt";
      };
    };

  testScript = ''
    k8s.wait_for_unit("multi-user.target")
    k8s.succeed("test -f /var/lib/kcore/kubernetes/lab/pki/ca.key")
    k8s.succeed("systemctl cat kube-apiserver.service")
    k8s.succeed("systemctl cat etcd.service")
    k8s.succeed("systemctl cat kubelet.service")
    # Bootstrap: etcd must become healthy with the generated PKI.
    k8s.wait_until_succeeds(
      "etcdctl --endpoints=https://127.0.0.1:2379 "
      "--cacert=/etc/kubernetes/pki/etcd/ca.crt "
      "--cert=/etc/kubernetes/pki/apiserver-etcd-client.crt "
      "--key=/etc/kubernetes/pki/apiserver-etcd-client.key "
      "endpoint health",
      timeout=180,
    )
    k8s.wait_for_unit("kube-apiserver.service")
    # Local etcd snapshot path used by day-two backup.
    k8s.succeed("mkdir -p /var/lib/kcore/kubernetes-backups/lab")
    k8s.succeed(
      "etcdctl --endpoints=https://127.0.0.1:2379 "
      "--cacert=/etc/kubernetes/pki/etcd/ca.crt "
      "--cert=/etc/kubernetes/pki/apiserver-etcd-client.crt "
      "--key=/etc/kubernetes/pki/apiserver-etcd-client.key "
      "snapshot save /var/lib/kcore/kubernetes-backups/lab/manual.db"
    )
    k8s.succeed("test -s /var/lib/kcore/kubernetes-backups/lab/manual.db")
    k8s.succeed(
      "ETCDCTL_API=3 etcdctl --write-out=table snapshot status "
      "/var/lib/kcore/kubernetes-backups/lab/manual.db"
    )
  '';
}
