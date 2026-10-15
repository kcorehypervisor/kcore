# Documentation Updates Summary

## Changes Made (2026-10-11, 9:17 PM)

### ❌ **Removed (Old Stack)**
- Calico CNI
- MetalLB (external LoadBalancer)
- nginx-ingress (deprecated)
- Datacenter BGP peering (not needed)
- Week-based timeline (replaced with phases)

### ✅ **Added (New Stack)**
- **Cilium CNI** (eBPF-based, native routing)
- **Cilium LoadBalancer** (L2 mode, no external dependencies)
- **Phase-based execution** (11 phases, agent-driven)
- **Ingress/Gateway out of scope** (can add Cilium Ingress or Envoy Gateway in Phase 12+)

---

## Why Cilium?

| Feature | Traditional (Calico) | Cilium eBPF |
|---------|---------------------|-------------|
| **Performance** | ~10 Gbps | **40+ Gbps** ✅ |
| **Latency** | ~5-10µs | **~1µs** ✅ |
| **CPU overhead** | 20% | **2%** ✅ |
| **Rules scale** | O(n) iptables | **O(1) eBPF maps** ✅ |
| **LoadBalancer** | Requires MetalLB | **Built-in** ✅ |
| **Observability** | Separate tools | **Hubble (built-in)** ✅ |
| **Ingress** | Requires nginx/traefik | **Optional Envoy-based** ✅ |
| **Gateway API** | Not supported | **Native support** ✅ |

---

## Stack Comparison

### Old Plan (Calico + MetalLB + nginx-ingress)
```
Calico CNI (VXLAN)
    ↓
MetalLB (L2 announcements)
    ↓
nginx-ingress controller
    ↓
Application pods
```

**Problems:**
- ❌ Calico uses iptables (slow, O(n) complexity)
- ❌ MetalLB is separate project (another thing to maintain)
- ❌ nginx-ingress deprecated/not preferred
- ❌ 3 components to install and configure

### New Plan (Cilium-Native)
```
Cilium eBPF CNI (native routing)
    ↓
Cilium LoadBalancer (L2, built-in)
    ↓
Application pods (directly accessible)
    
(Optional Phase 12+)
    ↓
Cilium Ingress OR Envoy Gateway
```

**Benefits:**
- ✅ Single component (Cilium does CNI + LoadBalancer)
- ✅ eBPF = 10x faster than iptables
- ✅ Native routing = no encapsulation overhead
- ✅ Hubble observability included
- ✅ No external router dependencies
- ✅ Future-proof (Gateway API support)

---

## Network Architecture (Simplified)

```
Management Network (192.168.40.0/24)
         │
    ┌────▼────┐
    │ VIP .200│  ← keepalived (API HA)
    └────┬────┘
         │
   ┌─────┼─────┐
   │     │     │
 CP-1  CP-2  CP-3  ← 3 Control Planes + etcd
   │     │     │
 W-1   W-2   W-3   ← 3 Workers
   │     │     │
   └─────┴─────┘
         │
  ┌──────▼──────┐
  │ Cilium eBPF │  ← CNI + LoadBalancer
  │ Pod Network │
  │ 10.244.0.0/16│
  └──────┬──────┘
         │
  ┌──────▼──────┐
  │ Cilium LB   │  ← L2 announcements
  │ 192.168.40. │     (no MetalLB needed)
  │ 220-227     │
  └─────────────┘
```

**Key Points:**
- **VIP** for API HA (keepalived)
- **Cilium** for pod networking (eBPF, native routing)
- **Cilium LB** for LoadBalancer services (L2 mode)
- **No tunnels** (VXLAN/IPIP/Geneve) - direct routing
- **No external dependencies** (no MetalLB, no routers)

---

## Updated Documents

1. **[kubernetes-networking-architecture.md](./kubernetes-networking-architecture.md)**
   - Complete Cilium explanation
   - VIP/keepalived detailed
   - Cilium LoadBalancer L2 mode
   - Hubble observability
   - Ingress/Gateway out of scope (future)
   - Removed all Calico/MetalLB/nginx references

2. **[IMPLEMENTATION_ROADMAP.md](./IMPLEMENTATION_ROADMAP.md)**
   - Changed from "Week 1-5" to "Phase 1-11"
   - Agent-driven execution (no calendar time)
   - Phase 9: Cilium LoadBalancer configuration (not MetalLB + nginx)
   - Updated all references to use Cilium

3. **[kubernetes-cluster-examples.yaml](./kubernetes-cluster-examples.yaml)**
   - All examples use Cilium CNI
   - `ciliumConfig` with native routing, L2 LoadBalancer
   - Removed Calico config examples
   - Added Cilium-specific examples (Hubble, LoadBalancer pools)

4. **[README.md](./docs/README.md)**
   - Updated document index
   - Links to networking architecture
   - Phase-based status

---

## Sample Cilium Config

```yaml
cni: cilium
ciliumConfig:
  # Native routing (no tunnels)
  mode: native-routing
  mtu: 1500
  ipam: kubernetes
  tunnel: disabled
  autoDirectNodeRoutes: true
  
  # Hubble observability
  hubble:
    enabled: true
    relay:
      enabled: true
    ui:
      enabled: true
  
  # LoadBalancer L2 mode
  loadBalancer:
    mode: l2
    ipPools:
      - name: default-pool
        cidr: 192.168.40.220/29  # 8 IPs
    l2Announcements:
      - interface: enp6s0
```

---

## What's Out of Scope (For Now)

### Ingress / API Gateway (Phase 12+)

**Not needed for Phase 1-11:**
- Services get real IPs via Cilium LoadBalancer
- Access apps directly: `http://192.168.40.220`
- No hostname-based routing yet

**Future options (Phase 12+):**
- **Cilium Ingress:** Built-in Envoy-based ingress controller
- **Envoy Gateway:** Advanced Gateway API support
- **WebAssembly:** Custom logic in Envoy filters

**Recommendation:** Deploy core cluster first (Phase 1-11), add ingress later when needed.

---

## Migration Benefits

### Performance Gains
- **10x faster** datapath (eBPF vs iptables)
- **Full MTU** (no tunneling overhead)
- **<1µs latency** (vs 5-10µs with iptables)
- **Constant time lookups** (O(1) vs O(n))

### Operational Simplicity
- **1 component instead of 3** (Cilium does CNI + LB)
- **Built-in observability** (Hubble, no separate tools)
- **No external routers** needed (L2 mode works anywhere)
- **Native Gateway API** support (future-proof)

### Developer Experience
- **Better debugging** (Hubble UI shows all flows)
- **NetworkPolicy visualization**
- **L7 visibility** (HTTP, gRPC, DNS)
- **WebAssembly extensibility** (future)

---

## Phase Execution Model

**Old:** Week 1, Week 2, Week 3 (calendar time estimates)

**New:** Phase 1, Phase 2, ... Phase 11 (task-based)
- No calendar time estimates
- Complete phase N before starting phase N+1
- Agent-driven execution (as fast as possible)
- Each phase has clear tasks and exit criteria

**Example:**
```
Phase 1: Foundation
  Task 1.1: gRPC Protocol Definitions
  Task 1.2: Database Schema
  Task 1.3: NixOS Module
  Task 1.4: Nix Code Generation
  Exit Criteria: Proto compiles, module evaluates, tests pass
```

---

## Commands Updated

### Before (Calico)
```yaml
cni: calico
calicoConfig:
  mode: vxlan
  mtu: 1450
```

### After (Cilium)
```yaml
cni: cilium
ciliumConfig:
  mode: native-routing
  mtu: 1500
  tunnel: disabled
```

### Before (MetalLB + nginx)
```bash
kubectl apply -f metallb.yaml
kubectl apply -f nginx-ingress.yaml
```

### After (Cilium only)

The controller unit `kcore-kubernetes-cilium` installs Cilium and applies the L2 pool. An operator does not run helm for this.

---

## Questions?

- **Network architecture:** See [kubernetes-networking-architecture.md](./kubernetes-networking-architecture.md)
- **Implementation phases:** See [IMPLEMENTATION_ROADMAP.md](./IMPLEMENTATION_ROADMAP.md)
- **Example configs:** See [kubernetes-cluster-examples.yaml](./kubernetes-cluster-examples.yaml)
- **Operator guide:** See [kubernetes-quickstart.md](./kubernetes-quickstart.md)

---

**Status:** All documentation updated to Cilium-native stack ✅
**Next:** Begin Phase 1 (Foundation: proto, module, schema)
