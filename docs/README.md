# Kubernetes on kcore - Documentation

This directory contains the complete implementation plan for running Kubernetes directly on kcoreOS bare-metal nodes.

## 📋 Quick Navigation

### For Operators
- **Start here:** [kubernetes-quickstart.md](./kubernetes-quickstart.md) - How to deploy and manage
- **Volumes:** [kubernetes-volumes.md](./kubernetes-volumes.md) - Local-path claims, and a CSI the operator installs
- **Examples:** [kubernetes-cluster-examples.yaml](./kubernetes-cluster-examples.yaml) - Sample cluster specs

### For Developers
- **Master plan:** [kubernetes-ha-calico-implementation.md](./kubernetes-ha-calico-implementation.md) - Complete technical design
- **Networking:** [kubernetes-networking-architecture.md](./kubernetes-networking-architecture.md) - VIP, BGP, ingress, load balancing
- **Roadmap:** [IMPLEMENTATION_ROADMAP.md](./IMPLEMENTATION_ROADMAP.md) - Phase-based execution (11 phases, agent-driven)
- **Proto (controller):** [proto-snippets-controller.proto](./proto-snippets-controller.proto) - gRPC API definitions
- **Proto (node):** [proto-snippets-node.proto](./proto-snippets-node.proto) - Node agent API

### Background
- **Original plan:** [bare-metal-kubernetes.md](./bare-metal-kubernetes.md) - Single-node design (superseded)
- **Security:** [security-k8s-vs-kcore.md](./security-k8s-vs-kcore.md) - Security model comparison

### Other bare-metal services
- **ClickHouse design:** [bare-metal-clickhouse.md](./bare-metal-clickhouse.md) - Shards, replicas, Keeper, rolling operations, test plan
- **ClickHouse proto:** [proto-snippets-clickhouse.proto](./proto-snippets-clickhouse.proto) - Controller and node RPCs (compiles standalone)
- **Ingress design:** [ingress-waf-load-balancer-plan.md](./ingress-waf-load-balancer-plan.md) - Caddy HTTPS load balancer with Coraza WAF and Let's Encrypt, fronting VMs, containers, and Kubernetes services

## 🎯 What This Implements

**Production-ready Kubernetes on kcoreOS with:**
- ✅ 3 control plane nodes (stacked etcd)
- ✅ 3+ worker nodes
- ✅ Calico CNI (VXLAN mode, NetworkPolicy support)
- ✅ High Availability (VIP with keepalived)
- ✅ Full gRPC automation (zero manual steps)
- ✅ Scaling support (add/remove nodes)
- ✅ Health monitoring

## 🏗️ Architecture

```
┌─────────────────────────────────────────────┐
│     Management Network (192.168.40.0/24)    │
└─────────────────────────────────────────────┘
                     │
        ┌────────────┴────────────┐
        │  VIP: 192.168.40.200    │  ← Highly Available API
        └────────────┬────────────┘
                     │
    ┌────────────────┼────────────────┐
    │                │                │
┌───▼────┐      ┌────▼───┐      ┌────▼───┐
│ CP-1   │◄─────┤ CP-2   │◄─────┤ CP-3   │  ← 3 Control Planes
│ + etcd │ etcd │ + etcd │ etcd │ + etcd │     (stacked etcd)
└────────┘      └────────┘      └────────┘

┌────────┐      ┌────────┐      ┌────────┐
│Worker-1│      │Worker-2│      │Worker-3│  ← 3+ Workers
└────────┘      └────────┘      └────────┘

┌─────────────────────────────────────────────┐
│   Pod Network: 10.244.0.0/16 (Calico VXLAN) │
│   Service Network: 10.96.0.0/12             │
└─────────────────────────────────────────────┘
```

## 🚀 Quick Start

### 1. Create Cluster Spec

Create `my-cluster.yaml`:

```yaml
kind: KubernetesCluster
metadata:
  name: lab-ha
spec:
  clusterCidr: 10.244.0.0/16
  serviceCidr: 10.96.0.0/12
  apiServerEndpoint: 192.168.40.200:6443
  cni: calico
  
  calicoConfig:
    mode: vxlan
    mtu: 1450
  
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
      # ... more workers
  
  highAvailability:
    enabled: true
    vipAddress: 192.168.40.200
    vipInterface: enp6s0
```

### 2. Plan (Dry-Run)

```bash
kctl kubernetes plan -f my-cluster.yaml
```

Reviews:
- All nodes reachable
- Interfaces exist
- Ports available
- No CIDR overlaps
- Nix config builds

### 3. Apply (Create)

```bash
kctl kubernetes apply -f my-cluster.yaml
```

Executes:
1. Bootstrap first control plane
2. Join additional control planes
3. Wait for etcd quorum
4. Join workers
5. Install Calico
6. Verify all nodes Ready

Duration: ~10-15 minutes

### 4. Verify

```bash
kctl kubernetes get lab-ha
kctl kubernetes health lab-ha

# Or via kubectl from any control plane
ssh node-105 'kubectl get nodes'
```

## 📦 What Gets Installed

### On Control Plane Nodes
- **kube-apiserver** - Kubernetes API (port 6443)
- **etcd** - Distributed key-value store (ports 2379, 2380)
- **kube-scheduler** - Pod placement
- **kube-controller-manager** - Controllers
- **kubelet** - Node agent (port 10250)
- **keepalived** - VIP management
- **calico-node** - CNI dataplane

### On Worker Nodes
- **kubelet** - Node agent
- **calico-node** - CNI dataplane

### Network Components
- **Calico** - CNI with VXLAN encapsulation
- **CoreDNS** - Cluster DNS (10.96.0.10)
- **kube-proxy** - Service load balancing

## 🔧 Operations

### Scale Cluster

Edit spec, add node to `workers.nodes`, then:
```bash
kctl kubernetes apply -f my-cluster.yaml
```

### Drain Node

```bash
ssh node-105 'kubectl drain worker-3 --ignore-daemonsets'
# ... maintenance ...
ssh node-105 'kubectl uncordon worker-3'
```

### Update Config

Edit spec (e.g., change MTU), then:
```bash
kctl kubernetes plan -f my-cluster.yaml   # Review changes
kctl kubernetes apply -f my-cluster.yaml  # Rolling update
```

### Delete Cluster

```bash
kctl kubernetes delete lab-ha --confirm
```

Stops all Kubernetes services and removes cluster record.

## 🛠️ Implementation Status

- [x] Design complete ([kubernetes-ha-calico-implementation.md](./kubernetes-ha-calico-implementation.md))
- [x] Networking architecture ([kubernetes-networking-architecture.md](./kubernetes-networking-architecture.md))
- [ ] Phase 1: Proto definitions, NixOS module, database schema
- [ ] Phase 2: Node agent (apply config, status, drain)
- [ ] Phase 3: Controller planning (preflights, validation)
- [ ] Phase 4: Controller creation (bootstrap, join, Calico)
- [ ] Phase 5: Controller operations (update, scale, delete, health)
- [ ] Phase 6: CLI commands (plan, apply, get, delete, health)
- [ ] Phase 7: MCP integration (advise, plan, apply)
- [ ] Phase 8: Lab deployment (6-node HA cluster)
- [ ] Phase 9: Ingress & load balancing (MetalLB, nginx-ingress)
- [ ] Phase 10: Validation & testing (failover, scaling, load tests)
- [ ] Phase 11: Documentation & operator training

**Current:** Design ✅ | **Next:** Phase 1 (Foundation)

**Execution:** Agent-driven phases, no calendar time estimates. See [IMPLEMENTATION_ROADMAP.md](./IMPLEMENTATION_ROADMAP.md).

## 📚 Document Descriptions

| Document | What It Contains |
|----------|-----------------|
| [kubernetes-quickstart.md](./kubernetes-quickstart.md) | Operator guide: architecture, workflows, troubleshooting |
| [kubernetes-ha-calico-implementation.md](./kubernetes-ha-calico-implementation.md) | Complete technical design: API, Rust code, Nix module |
| [kubernetes-networking-architecture.md](./kubernetes-networking-architecture.md) | **VIP/keepalived, BGP, ingress, load balancing explained** |
| [kubernetes-cluster-examples.yaml](./kubernetes-cluster-examples.yaml) | Sample YAML specs: dev, HA, large, BGP, multi-NIC |
| [IMPLEMENTATION_ROADMAP.md](./IMPLEMENTATION_ROADMAP.md) | **Phase-based plan** (11 phases, agent-driven execution) |
| [proto-snippets-controller.proto](./proto-snippets-controller.proto) | gRPC API for controller (copy to proto/controller.proto) |
| [proto-snippets-node.proto](./proto-snippets-node.proto) | gRPC API for node agent (copy to proto/node.proto) |
| [bare-metal-kubernetes.md](./bare-metal-kubernetes.md) | Original single-node plan (superseded by HA design) |
| [security-k8s-vs-kcore.md](./security-k8s-vs-kcore.md) | Kubernetes vs kcore security model comparison |

## 🎓 Key Concepts

### Why Bare Metal?

kcore runs Kubernetes **directly on the hypervisor nodes** using NixOS services. This is the same pattern as Ceph (`kcore-ceph.nix`). Guest VMs continue using cloud-hypervisor. Both can coexist.

**Benefits:**
- No nested virtualization overhead
- Direct hardware access for GPU workloads
- Simpler resource management
- Single operator for VMs + containers

### Why Calico?

- **Production-grade** CNI used by major deployments
- **NetworkPolicy enforcement** built-in
- **VXLAN mode** compatible with kcore's existing overlay
- **BGP-capable** for future datacenter integration
- **Well-maintained** with active community

### Why 3 Control Planes?

- **etcd quorum:** 3 members = tolerates 1 failure (2/3 quorum)
- **API availability:** VIP fails over in < 5 seconds
- **Production best practice:** Odd numbers (3, 5, 7) for etcd

### Why Stacked etcd?

- **Simpler architecture:** etcd runs on control plane nodes
- **Fewer machines:** No separate etcd cluster
- **Good for most deployments:** < 100 nodes

Alternative (future): External etcd on dedicated nodes.

## 🔒 Security Considerations

### Certificate Management
- Bootstrap node generates CA via NixOS `easyCerts`
- Additional nodes get certificates from bootstrap
- Follows Kubernetes PKI best practices

### Network Isolation
- Kubernetes API only on management network (192.168.40.0/24)
- Calico VXLAN isolated from kcore VM VXLAN (different bridges)
- Firewall rules per node role

### Pod Security
- RBAC enabled by default
- Service accounts auto-created
- NetworkPolicy enforcement ready
- Consider Pod Security Standards for sensitive workloads

### Host Access
- Pods share host kernel
- Privileged pods = root on kcoreOS
- Operator controls node membership

See [security-k8s-vs-kcore.md](./security-k8s-vs-kcore.md) for full comparison.

## 🧪 Testing Approach

### Unit Tests
- Nix code generation
- CIDR validation
- String escaping
- etcd initial-cluster builder

### Integration Tests
- Controller → node agent (apply config)
- Node preflights
- Full lifecycle (plan → create → scale → delete)

### System Tests (Lab)
- 3-node control plane bootstrap
- 3-worker join
- Failover scenarios
- Scaling operations
- NetworkPolicy enforcement
- Load testing

## 🚨 Pre-Deployment Checklist

Before running `kctl kubernetes apply`:

- [ ] All nodes approved in `kctl get nodes`
- [ ] Static IPs configured on 192.168.40.0/24
- [ ] VIP address (e.g., 192.168.40.200) not in use
- [ ] NICs have MTU ≥ 1500
- [ ] No existing Kubernetes on these nodes
- [ ] Nodes have sufficient resources (4 CPU, 8 GB RAM minimum)
- [ ] OS disks have 50+ GB free
- [ ] Internet access for image pulls (or registry mirror configured)
- [ ] VMs drained from nodes (if needed)
- [ ] Operator confirmed node assignments

## 🆘 Troubleshooting

### Check Service Status
```bash
ssh node-105 'systemctl status kube-apiserver etcd kubelet'
```

### View Logs
```bash
ssh node-105 'journalctl -u kube-apiserver -f'
ssh node-105 'journalctl -u etcd -f'
```

### Check Calico
```bash
ssh node-105 'kubectl get pods -n calico-system'
ssh node-105 'kubectl logs -n calico-system -l k8s-app=calico-node'
```

### Check etcd Health
```bash
ssh node-105 'etcdctl member list'
ssh node-105 'etcdctl endpoint health'
```

### Common Issues

**"Port 6443 already in use"**
→ Check: `sudo ss -tulpn | grep 6443`

**"Pod CIDR overlaps"**
→ Change `clusterCidr` in spec to avoid 192.168.x.x

**"Node NotReady"**
→ Check: `systemctl status kubelet` and Calico pod logs

See [kubernetes-quickstart.md](./kubernetes-quickstart.md) for more.

## 📈 Performance Recommendations

### Control Plane
- **CPU:** 4+ cores
- **RAM:** 8+ GB
- **Disk:** 100+ GB SSD (etcd is latency-sensitive)

### Workers
- **CPU:** 8+ cores
- **RAM:** 16+ GB
- **Disk:** 100+ GB

### Network
- **Management:** 1 Gbps minimum
- **MTU:** 1500 on physical NICs
- **Latency:** < 5ms between control planes

## 🔮 Future Enhancements

- [ ] Ceph RBD CSI driver (persistent volumes)
- [ ] Ingress controller (nginx/traefik)
- [ ] Monitoring (Prometheus operator)
- [ ] Logging (Loki)
- [ ] Cert-manager
- [ ] Calico BGP mode
- [ ] Cilium alternative CNI
- [ ] GPU scheduling via device plugin
- [ ] Multi-cluster federation

## 🤝 Contributing

This implementation follows the established kcore patterns:

1. **Proto-first:** Define gRPC API before implementation
2. **NixOS modules:** Declarative host configuration
3. **Controller reconciliation:** Desired state → actual state
4. **Node agent execution:** Apply changes, verify, report
5. **Tests:** Unit, integration, system

See [kubernetes-ha-calico-implementation.md](./kubernetes-ha-calico-implementation.md) for code structure.

## 📞 Support

- **Technical questions:** Review the implementation plan
- **Operator questions:** See the quickstart guide
- **Issues:** File in kcore repository
- **Blockers:** Escalate to team lead

---

**Last Updated:** 2026-10-11  
**Status:** Design phase complete, implementation pending  
**Next Step:** Begin Week 1 (Proto & Module)
