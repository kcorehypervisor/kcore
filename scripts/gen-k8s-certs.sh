#!/usr/bin/env bash
set -euo pipefail

# Generate Kubernetes PKI certificates for bare-metal cluster.
# Usage: ./gen-k8s-certs.sh <cluster-name> <api-endpoint> <node-105-ip> <node-107-ip> <node-119-ip>
# Output directory: $KCORE_K8S_PKI_DIR, or ${XDG_STATE_HOME:-$HOME/.local/state}/kcore/k8s-pki.
# Do not point this at the git checkout.

CLUSTER_NAME="${1:-kcore-prod}"
API_ENDPOINT="${2:-192.168.40.200}"
NODE_105="${3:-192.168.40.105}"
NODE_107="${4:-192.168.40.107}"
NODE_119="${5:-192.168.40.119}"

OUT_DIR="${KCORE_K8S_PKI_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/kcore/k8s-pki}"
case "$OUT_DIR" in
  /*) ;;
  *)
    echo "Error: KCORE_K8S_PKI_DIR must be an absolute path (got $OUT_DIR)" >&2
    exit 1
    ;;
esac
mkdir -p "$OUT_DIR"
chmod 700 "$OUT_DIR"

echo "==> Generating Kubernetes PKI for cluster: $CLUSTER_NAME"
echo "    API endpoint: $API_ENDPOINT"
echo "    Nodes: $NODE_105, $NODE_107, $NODE_119"
echo

# Generate CA certificate and key
echo "==> Generating Kubernetes CA"
openssl genrsa -out "$OUT_DIR/ca.key" 2048
openssl req -x509 -new -nodes -key "$OUT_DIR/ca.key" -sha256 -days 3650 \
  -out "$OUT_DIR/ca.crt" \
  -subj "/CN=kubernetes-ca/O=kubernetes"

# Generate etcd CA
echo "==> Generating etcd CA"
openssl genrsa -out "$OUT_DIR/etcd-ca.key" 2048
openssl req -x509 -new -nodes -key "$OUT_DIR/etcd-ca.key" -sha256 -days 3650 \
  -out "$OUT_DIR/etcd-ca.crt" \
  -subj "/CN=etcd-ca/O=etcd"

# Generate service account key pair
echo "==> Generating service account keys"
openssl genrsa -out "$OUT_DIR/sa.key" 2048
openssl rsa -in "$OUT_DIR/sa.key" -pubout -out "$OUT_DIR/sa.pub"

# Generate API server certificate
echo "==> Generating API server certificate"
openssl genrsa -out "$OUT_DIR/apiserver.key" 2048
cat > "$OUT_DIR/apiserver.conf" <<EOF
[req]
req_extensions = v3_req
distinguished_name = req_distinguished_name
[req_distinguished_name]
[v3_req]
basicConstraints = CA:FALSE
keyUsage = nonRepudiation, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = @alt_names
[alt_names]
DNS.1 = kubernetes
DNS.2 = kubernetes.default
DNS.3 = kubernetes.default.svc
DNS.4 = kubernetes.default.svc.cluster.local
DNS.5 = node-105
DNS.6 = node-107
DNS.7 = node-119
IP.1 = 10.96.0.1
IP.2 = $API_ENDPOINT
IP.3 = $NODE_105
IP.4 = $NODE_107
IP.5 = $NODE_119
IP.6 = 127.0.0.1
EOF

openssl req -new -key "$OUT_DIR/apiserver.key" -out "$OUT_DIR/apiserver.csr" \
  -subj "/CN=kube-apiserver/O=kubernetes" \
  -config "$OUT_DIR/apiserver.conf"
openssl x509 -req -in "$OUT_DIR/apiserver.csr" -CA "$OUT_DIR/ca.crt" -CAkey "$OUT_DIR/ca.key" \
  -CAcreateserial -out "$OUT_DIR/apiserver.crt" -days 365 \
  -extensions v3_req -extfile "$OUT_DIR/apiserver.conf"

# Generate API server kubelet client certificate
echo "==> Generating API server kubelet client certificate"
openssl genrsa -out "$OUT_DIR/apiserver-kubelet-client.key" 2048
openssl req -new -key "$OUT_DIR/apiserver-kubelet-client.key" \
  -out "$OUT_DIR/apiserver-kubelet-client.csr" \
  -subj "/CN=kube-apiserver-kubelet-client/O=system:masters"
openssl x509 -req -in "$OUT_DIR/apiserver-kubelet-client.csr" \
  -CA "$OUT_DIR/ca.crt" -CAkey "$OUT_DIR/ca.key" -CAcreateserial \
  -out "$OUT_DIR/apiserver-kubelet-client.crt" -days 365

# Generate API server etcd client certificate
echo "==> Generating API server etcd client certificate"
openssl genrsa -out "$OUT_DIR/apiserver-etcd-client.key" 2048
openssl req -new -key "$OUT_DIR/apiserver-etcd-client.key" \
  -out "$OUT_DIR/apiserver-etcd-client.csr" \
  -subj "/CN=kube-apiserver-etcd-client/O=system:masters"
openssl x509 -req -in "$OUT_DIR/apiserver-etcd-client.csr" \
  -CA "$OUT_DIR/etcd-ca.crt" -CAkey "$OUT_DIR/etcd-ca.key" -CAcreateserial \
  -out "$OUT_DIR/apiserver-etcd-client.crt" -days 365

# Function to generate etcd server certificate for a node
gen_etcd_cert() {
  local node_name=$1
  local node_ip=$2
  
  echo "==> Generating etcd server certificate for $node_name"
  openssl genrsa -out "$OUT_DIR/etcd-server-$node_name.key" 2048
  
  cat > "$OUT_DIR/etcd-$node_name.conf" <<EOF
[req]
req_extensions = v3_req
distinguished_name = req_distinguished_name
[req_distinguished_name]
[v3_req]
basicConstraints = CA:FALSE
keyUsage = nonRepudiation, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth, clientAuth
subjectAltName = @alt_names
[alt_names]
DNS.1 = $node_name
DNS.2 = localhost
IP.1 = $node_ip
IP.2 = 127.0.0.1
EOF

  openssl req -new -key "$OUT_DIR/etcd-server-$node_name.key" \
    -out "$OUT_DIR/etcd-server-$node_name.csr" \
    -subj "/CN=$node_name" \
    -config "$OUT_DIR/etcd-$node_name.conf"
  openssl x509 -req -in "$OUT_DIR/etcd-server-$node_name.csr" \
    -CA "$OUT_DIR/etcd-ca.crt" -CAkey "$OUT_DIR/etcd-ca.key" -CAcreateserial \
    -out "$OUT_DIR/etcd-server-$node_name.crt" -days 365 \
    -extensions v3_req -extfile "$OUT_DIR/etcd-$node_name.conf"
  
  # etcd peer certificate (same as server for simplicity)
  cp "$OUT_DIR/etcd-server-$node_name.crt" "$OUT_DIR/etcd-peer-$node_name.crt"
  cp "$OUT_DIR/etcd-server-$node_name.key" "$OUT_DIR/etcd-peer-$node_name.key"
}

# Generate etcd certificates for all nodes
gen_etcd_cert "node-105" "$NODE_105"
gen_etcd_cert "node-107" "$NODE_107"
gen_etcd_cert "node-119" "$NODE_119"

echo
echo "==> Certificates generated in $OUT_DIR/"
echo
echo "Next steps:"
echo "1. Copy certificates to each node"
echo "2. Update NixOS configuration with certificate data"
echo "3. Run nixos-rebuild switch"
