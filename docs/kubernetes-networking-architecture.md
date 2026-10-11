# Kubernetes Networking Architecture for kcore (Cilium-Native)

## Network Stack Overview

```
                Management Network (192.168.40.0/24)
                           ↓
                  ┌────────────────┐
                  │ VIP: .200:6443 │  ← keepalived (API HA)
                  └────────┬───────┘
                           │
        ┌──────────────────┼──────────────────┐
        │                  │                  │
    ┌───▼───┐         ┌────▼───┐        ┌────▼───┐
    │ CP-1  │◄────────┤ CP-2   │◄───────┤ CP-3   │  ← 3 Control Planes
    │ +etcd │  etcd   │ +etcd  │  etcd  │ +etcd  │
    └───────┘         └────────┘        └────────┘
    
    ┌───────┐         ┌────────┐        ┌────────┐
    │ W-1   │         │ W-2    │        │ W-3    │  ← 3+ Workers
    └───┬───┘         └────┬───┘        └────┬───┘
        │                  │                  │
        └──────────────────┴──────────────────┘
                           │
        ┌──────────────────▼──────────────────────┐
        │    Cilium eBPF Pod Network              │
        │    - Pod CIDR: 10.244.0.0/16            │
        │    - Service CIDR: 10.96.0.0/12         │
        │    - Native routing (no VXLAN overhead) │
        │    - Hubble observability               │
        └──────────────────┬──────────────────────┘
                           │
        ┌──────────────────▼──────────────────────┐
        │    Cilium LoadBalancer (L2 Mode)        │
        │    - Assigns real IPs to Services       │
        │    - ARP/NDP announcements              │
        │    - No external router needed          │
        └─────────────────────────────────────────┘
```

## 1. VIP + keepalived (Kubernetes API HA)

**Purpose:** High availability for Kubernetes API server on control planes.

**How it works:**
- **VIP:** Floating IP (e.g., 192.168.40.200) managed by keepalived
- **VRRP:** Virtual Router Redundancy Protocol for failover
- **Priorities:** Bootstrap = MASTER (100), others = BACKUP (50)
- **Failover:** <5 seconds if API server dies
- **Health checks:** Monitors port 6443 on each control plane

**Configuration:**
```nix
services.keepalived = {
  enable = true;
  vrrpInstances.kube_apiserver = {
    interface = "enp6s0";
    state = if isBootstrap then "MASTER" else "BACKUP";
    priority = if isBootstrap then 100 else 50;
    virtualRouterId = 51;
    virtualIps = [{ addr = "192.168.40.200"; }];
  };
};
```

**Who uses it:** `kubectl`, kubelet on workers, all external clients

---

## 2. Cilium CNI (eBPF-Based Networking)

### Why Cilium?

- ✅ **eBPF-based:** Kernel-level datapath, no iptables overhead
- ✅ **Better performance:** 10x faster than traditional CNI
- ✅ **Built-in LoadBalancer:** No MetalLB needed
- ✅ **Hubble:** Deep network observability
- ✅ **NetworkPolicy:** Advanced security policies
- ✅ **Gateway API:** Future-proof ingress (out of scope for now)
- ✅ **No external dependencies:** Works standalone

### Cilium Modes

#### Native Routing (Default, Recommended)

**No encapsulation overhead.** Cilium programs kernel routes directly.

```yaml
cni: cilium
ciliumConfig:
  mode: native-routing
  mtu: 1500  # Full MTU, no overhead
  ipam: kubernetes
  tunnel: disabled
  autoDirectNodeRoutes: true
```

**How it works:**
- Each node gets a pod CIDR subnet (e.g., node-1: 10.244.0.0/24, node-2: 10.244.1.0/24)
- Cilium programs kernel routes: `10.244.1.0/24 via 192.168.40.104`
- Pods communicate directly through management network
- **No VXLAN, no IP-in-IP, no tunnels**

**Requirements:**
- ✅ Nodes on same L2 network (same subnet) - **kcore already has this**
- ✅ No datacenter router config needed
- ✅ Works out of the box

#### Tunnel Mode (Alternative)

If native routing doesn't work (rare), Cilium can fall back to VXLAN or Geneve tunneling.

```yaml
ciliumConfig:
  mode: tunnel
  tunnel: vxlan  # or geneve
  mtu: 1450
```

**When to use:** Only if native routing is blocked by network policy. **Default to native routing.**

---

## 3. Cilium LoadBalancer (Replaces MetalLB)

### What It Does

Assigns **real IP addresses** from your network to Kubernetes Services of type `LoadBalancer`.

**Without LoadBalancer:**
```yaml
apiVersion: v1
kind: Service
metadata:
  name: my-app
spec:
  type: ClusterIP
  clusterIP: 10.96.5.10  # Only reachable inside cluster ❌
```

**With Cilium LoadBalancer:**
```yaml
apiVersion: v1
kind: Service
metadata:
  name: my-app
spec:
  type: LoadBalancer
  loadBalancerIP: 192.168.40.220  # Real IP, reachable from network ✅
  ports:
    - port: 80
      targetPort: 8080
```

### Cilium LB L2 Mode (No BGP, No External Routers)

**How it works:**
- **ARP announcements:** Cilium answers ARP requests for LoadBalancer IPs
- **Leader election:** One node (per IP) answers ARP
- **Failover:** If that node dies, another node takes over (<5s)
- **No router config:** Works on any L2 network

**Configuration:**
```yaml
apiVersion: "cilium.io/v2alpha1"
kind: CiliumLoadBalancerIPPool
metadata:
  name: default-pool
spec:
  cidrs:
    - cidr: "192.168.40.220/29"  # 8 IPs: .220-.227
  serviceSelector:
    matchLabels: {}  # All LoadBalancer services
---
apiVersion: "cilium.io/v2alpha1"
kind: CiliumL2AnnouncementPolicy
metadata:
  name: default-l2-policy
spec:
  interfaces:
    - enp6s0  # Management NIC
  externalIPs: true
  loadBalancerIPs: true
```

**Result:**
- Service gets `192.168.40.220` as EXTERNAL-IP
- Accessible from entire `192.168.40.0/24` network
- No MetalLB, no external load balancer needed

### Cilium BGP Control Plane (Future, Optional)

**NOT datacenter BGP peering** - this is **Cilium-internal BGP** for advanced setups:
- BGP between Cilium nodes for multi-cluster
- BGP for service IP advertisement within Kubernetes
- **Does NOT require external routers**

**Recommendation:** Start with L2 mode (Phase 1-11). Add BGP control plane in Phase 12+ if needed.

---

## 4. Ingress / API Gateway (Out of Scope for Phase 1-11)

### Options for Future (Phase 12+)

#### Option 1: Cilium Ingress (Built-in Envoy)

Cilium includes an Envoy-based ingress controller.

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: my-app
spec:
  ingressClassName: cilium
  rules:
    - host: app.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: my-app
                port:
                  number: 80
```

**Enable with:**
```
helm install cilium cilium/cilium \
  --set ingressController.enabled=true
```

#### Option 2: Envoy Gateway (Gateway API)

More advanced than Ingress, supports L4 + L7 routing.

```yaml
apiVersion: gateway.networking.k8s.io/v1beta1
kind: Gateway
metadata:
  name: eg
spec:
  gatewayClassName: eg
  listeners:
    - name: http
      protocol: HTTP
      port: 80
```

**Install Envoy Gateway:**
```bash
kubectl apply -f https://github.com/envoyproxy/gateway/releases/latest/download/install.yaml
```

### Decision: **Out of Scope**

For Phase 1-11:
- ✅ Services get LoadBalancer IPs via Cilium LB
- ✅ Access apps directly: `http://192.168.40.220`
- ❌ No Ingress controller
- ❌ No API Gateway
- ❌ No hostname routing

**Phase 12+:** Add Cilium Ingress or Envoy Gateway for HTTP/HTTPS routing.

---

## 5. Updated Cluster Spec (Cilium-Native)

```yaml
kind: KubernetesCluster
metadata:
  name: lab-ha
spec:
  clusterCidr: 10.244.0.0/16
  serviceCidr: 10.96.0.0/12
  clusterDns: 10.96.0.10
  apiServerEndpoint: 192.168.40.200:6443
  
  # Cilium CNI (not Calico)
  cni: cilium
  ciliumConfig:
    mode: native-routing  # No tunnels, direct routing
    mtu: 1500             # Full MTU
    ipam: kubernetes      # Kubernetes-managed IPAM
    tunnel: disabled
    autoDirectNodeRoutes: true
    hubble:
      enabled: true       # Observability
      relay:
        enabled: true
      ui:
        enabled: true
    
    # Cilium LoadBalancer L2
    loadBalancer:
      mode: l2
      ipPools:
        - name: default-pool
          cidr: 192.168.40.220/29  # 8 IPs (.220-.227)
      l2Announcements:
        - interface: enp6s0
  
  controlPlane:
    nodes:
      - nodeId: node-105
        advertiseAddress: 192.168.40.105
        iface: enp6s0
        initialCluster: true
      - nodeId: node-107
        advertiseAddress: 192.168.40.107
        iface: enp6s0
        joinExisting: true
      - nodeId: node-119
        advertiseAddress: 192.168.40.119
        iface: enp6s0
        joinExisting: true
  
  workers:
    nodes:
      - nodeId: node-103
        advertiseAddress: 192.168.40.103
        iface: enp6s0
      - nodeId: worker-2
        advertiseAddress: 192.168.40.104
        iface: enp6s0
      - nodeId: worker-3
        advertiseAddress: 192.168.40.106
        iface: enp6s0
  
  highAvailability:
    enabled: true
    vipAddress: 192.168.40.200
    vipInterface: enp6s0
    method: keepalived
```

---

## 6. Cilium Installation (Phase 4, Task 4.7)

### Via Helm (Recommended)

```bash
# Add Cilium Helm repo
helm repo add cilium https://helm.cilium.io/
helm repo update

# Install Cilium with native routing + L2 LoadBalancer
helm install cilium cilium/cilium \
  --namespace kube-system \
  --set ipam.mode=kubernetes \
  --set tunnel=disabled \
  --set autoDirectNodeRoutes=true \
  --set ipv4NativeRoutingCIDR=10.244.0.0/16 \
  --set l2announcements.enabled=true \
  --set kubeProxyReplacement=true \
  --set k8sServiceHost=192.168.40.200 \
  --set k8sServicePort=6443 \
  --set hubble.enabled=true \
  --set hubble.relay.enabled=true \
  --set hubble.ui.enabled=true
```

### Configure LoadBalancer IP Pool

```bash
kubectl apply -f - <<EOF
apiVersion: "cilium.io/v2alpha1"
kind: CiliumLoadBalancerIPPool
metadata:
  name: default-pool
spec:
  cidrs:
    - cidr: "192.168.40.220/29"
---
apiVersion: "cilium.io/v2alpha1"
kind: CiliumL2AnnouncementPolicy
metadata:
  name: default-l2-policy
spec:
  interfaces:
    - enp6s0
  externalIPs: true
  loadBalancerIPs: true
EOF
```

### Verify

```bash
# Check Cilium pods
kubectl get pods -n kube-system -l k8s-app=cilium

# Check Cilium status
cilium status

# Check connectivity
cilium connectivity test

# Access Hubble UI
cilium hubble ui
```

---

## 7. Example: Deploy App with LoadBalancer

```bash
# Deploy nginx
kubectl create deployment nginx --image=nginx --replicas=3

# Expose with LoadBalancer
kubectl expose deployment nginx \
  --type=LoadBalancer \
  --port=80 \
  --target-port=80

# Check LoadBalancer IP
kubectl get svc nginx
# NAME    TYPE           CLUSTER-IP      EXTERNAL-IP      PORT(S)
# nginx   LoadBalancer   10.96.123.45    192.168.40.220   80:30123/TCP

# Access from anywhere on 192.168.40.0/24 network
curl http://192.168.40.220
```

**No Ingress needed!** App is directly accessible via LoadBalancer IP.

---

## 8. Hubble Observability (Included with Cilium)

### What You Get

- **Network flows:** See pod-to-pod communication
- **Latency metrics:** Request/response times
- **Security policies:** Which policies allowed/denied traffic
- **DNS queries:** What domains pods are resolving
- **HTTP/gRPC metrics:** L7 visibility

### Access Hubble UI

```bash
cilium hubble ui
# Opens browser to http://localhost:12000
```

### CLI Queries

```bash
# Watch all flows
hubble observe

# Filter by pod
hubble observe --pod default/nginx-abc123

# Filter by verdict (dropped packets)
hubble observe --verdict DROPPED

# HTTP requests only
hubble observe --protocol http
```

---

## 9. Component Comparison

| Component | Traditional Stack | Cilium-Native Stack |
|-----------|------------------|---------------------|
| **CNI** | Calico (iptables) | **Cilium (eBPF)** ✅ |
| **Routing** | VXLAN encapsulation | **Native routing** ✅ |
| **LoadBalancer** | MetalLB | **Cilium LB L2** ✅ |
| **Ingress** | nginx-ingress | **Out of scope** (Phase 12+: Cilium Ingress) |
| **Observability** | Separate tools | **Hubble (built-in)** ✅ |
| **NetworkPolicy** | Calico | **Cilium (eBPF)** ✅ |
| **Service Mesh** | Istio/Linkerd | **Cilium (future)** |

---

## 10. Performance Benefits

### eBPF vs iptables

| Metric | iptables (Calico) | eBPF (Cilium) |
|--------|------------------|---------------|
| **Latency** | ~5-10µs | **~1µs** |
| **Throughput** | ~10 Gbps | **~40 Gbps** |
| **CPU overhead** | 20% | **2%** |
| **Rules scale** | Linear (O(n)) | **Constant (O(1))** |

### Native Routing vs VXLAN

| Metric | VXLAN | Native Routing |
|--------|-------|----------------|
| **MTU** | 1450 (overhead) | **1500 (full)** |
| **CPU** | +5% (encap/decap) | **0% (direct)** |
| **Latency** | +0.5ms | **0ms** |

---

## 11. Recommended Architecture (Phase 1-11)

```
Management Network: 192.168.40.0/24
Pod CIDR: 10.244.0.0/16 (Cilium native routing)
Service CIDR: 10.96.0.0/12
LoadBalancer Pool: 192.168.40.220-227 (Cilium L2)

Components:
✅ VIP + keepalived (API HA)
✅ Cilium CNI (eBPF, native routing)
✅ Cilium LoadBalancer (L2 mode, no external router)
✅ Hubble (observability)
❌ No Ingress controller (Phase 12+)
❌ No API Gateway (Phase 12+)
❌ No MetalLB (replaced by Cilium LB)
❌ No datacenter BGP (not needed)
```

---

## 12. Future Enhancements (Phase 12+)

- [ ] Cilium Ingress Controller (Envoy-based)
- [ ] Envoy Gateway (Gateway API)
- [ ] Cilium Service Mesh
- [ ] WebAssembly filters in Envoy
- [ ] Cilium BGP Control Plane (multi-cluster)
- [ ] Tetragon (runtime security)
- [ ] Multi-cluster with ClusterMesh

---

**Summary:**
- **CNI:** Cilium (eBPF, native routing)
- **LoadBalancer:** Cilium L2 (no MetalLB, no routers)
- **API HA:** keepalived VIP
- **Ingress:** Out of scope (Phase 12+: Cilium Ingress or Envoy Gateway)
- **No external dependencies:** Works standalone on kcore
