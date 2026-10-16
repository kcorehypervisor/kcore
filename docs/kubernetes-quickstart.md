# Kubernetes on kcore - Quick Start

**Status:** Implementation in progress  
**Full plan:** See [kubernetes-ha-calico-implementation.md](./kubernetes-ha-calico-implementation.md)

## What This Provides

- **Production-ready Kubernetes** running directly on kcoreOS bare metal
- **High Availability:** 3 control plane nodes with stacked etcd
- **Calico CNI** with VXLAN encapsulation and NetworkPolicy support
- **Fully automated:** Zero manual steps after YAML declaration

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                  Management Network (192.168.40.0/24)           │
└─────────────────────────────────────────────────────────────────┘
                                  │
                    ┌─────────────┴──────────────┐
                    │  VIP: 192.168.40.200:6443  │
                    │  (keepalived managed)      │
                    └─────────────┬──────────────┘
                                  │
        ┌─────────────────────────┼─────────────────────────┐
        │                         │                         │
    ┌───▼────┐               ┌────▼───┐               ┌────▼───┐
    │ CP-1   │               │ CP-2   │               │ CP-3   │
    │ .105   │◄──────etcd────┤ .107   │◄──────etcd────┤ .119   │
    │        │               │        │               │        │
    │ • api  │               │ • api  │               │ • api  │
    │ • etcd │               │ • etcd │               │ • etcd │
    │ • sched│               │ • sched│               │ • sched│
    │ • c-mgr│               │ • c-mgr│               │ • c-mgr│
    └────────┘               └────────┘               └────────┘

        ┌────────────────────────────────────────────────┐
        │              Worker Nodes                      │
        └────────────────────────────────────────────────┘
        │                    │                    │
    ┌───▼────┐          ┌────▼───┐          ┌────▼───┐
    │ W-1    │          │ W-2    │          │ W-3    │
    │ .103   │          │ .104   │          │ .106   │
    │        │          │        │          │        │
    │ kubelet│          │ kubelet│          │ kubelet│
    └────────┘          └────────┘          └────────┘

┌─────────────────────────────────────────────────────────────────┐
│           Pod Network (10.244.0.0/16 - Calico VXLAN)            │
│                   Service CIDR (10.96.0.0/12)                   │
└─────────────────────────────────────────────────────────────────┘
```

## Network Details

| Network | CIDR | Purpose |
|---------|------|---------|
| Management | 192.168.40.0/24 | kcore control, Kubernetes API |
| Ceph | 192.168.47.0/24 | Existing, untouched |
| Pods | 10.244.0.0/16 | Calico overlay (VXLAN) |
| Services | 10.96.0.0/12 | ClusterIP range |
| API VIP | 192.168.40.200 | Highly available API endpoint |

## Example Cluster Declaration

Create `lab-cluster.yaml`:

```yaml
kind: KubernetesCluster
metadata:
  name: lab-ha
  generation: 1
spec:
  clusterCidr: 10.244.0.0/16
  serviceCidr: 10.96.0.0/12
  clusterDns: 10.96.0.10
  apiServerEndpoint: 192.168.40.200:6443
  cni: calico
  
  calicoConfig:
    mode: vxlan
    mtu: 1450
    ipipMode: Never
    vxlanMode: Always
    natOutgoing: true
  
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

## Operator Workflow

### 1. Plan (Dry-Run)

```bash
kctl kubernetes plan -f lab-cluster.yaml
```

This will:
- Check all nodes are reachable
- Verify interfaces exist
- Verify addresses are configured
- Check ports are available (6443, 2379, 2380, 10250)
- Check CIDRs don't overlap
- Dry-build Nix configuration
- Show what will be deployed

**Sample output:**
```
✅ Plan OK

Action: create

Node plans:
  node-105 - ✅ (control-plane, bootstrap)
  node-107 - ✅ (control-plane, join)
  node-119 - ✅ (control-plane, join)
  node-103 - ✅ (worker)
  worker-2 - ✅ (worker)
  worker-3 - ✅ (worker)

Nix preview (first control plane):
{ ... }: {
  kcore.kubernetes = {
    enable = true;
    role = "control-plane";
    advertiseAddress = "192.168.40.105";
    ...
  };
}
```

### 2. Apply (Create Cluster)

```bash
kctl kubernetes apply -f lab-cluster.yaml
```

**What happens:**
1. Bootstrap control plane on node-105 (generates PKI, starts etcd, kube-apiserver)
2. Wait for API server ready
3. Join node-107 and node-119 to control plane (add to etcd cluster)
4. Wait for etcd quorum (3/3 members)
5. Join all 3 workers
6. Install Calico via systemd service
7. Wait for all nodes Ready

**Duration:** ~10-15 minutes (includes image pulls)

### 3. Verify

```bash
# Get cluster status
kctl kubernetes get lab-ha

# Check health
kctl kubernetes health lab-ha

# Access via kubectl (admin kubeconfig on control planes)
ssh node-105 'kubectl get nodes'
```

**Expected output:**
```
NAME        STATUS   ROLES           AGE   VERSION
node-105    Ready    control-plane   5m    v1.28.x
node-107    Ready    control-plane   4m    v1.28.x
node-119    Ready    control-plane   4m    v1.28.x
node-103    Ready    worker          3m    v1.28.x
worker-2    Ready    worker          3m    v1.28.x
worker-3    Ready    worker          3m    v1.28.x
```

### 4. Deploy Test Workload

```bash
ssh node-105
export KUBECONFIG=/var/lib/kubernetes/admin.conf

kubectl create deployment nginx --image=nginx --replicas=6
kubectl get pods -o wide

# Expected: 2 pods per worker node
```

### 5. Test HA Failover

```bash
# On node-107, stop API server
ssh node-107 'sudo systemctl stop kube-apiserver'

# VIP should move to node-105 or node-119 within 5 seconds
# kubectl commands should still work via VIP

# Verify
kubectl --kubeconfig=... --server=https://192.168.40.200:6443 get nodes

# Restore
ssh node-107 'sudo systemctl start kube-apiserver'
```

## Common Operations

### Scale Cluster (Add Worker)

The worker must already be an approved kcore node. `kctl kubernetes scale` joins it. Applying a manifest whose membership differs from the cluster scales that difference. A node added after the first create uses `initial-cluster-state=existing` and does not bootstrap etcd again. Workers do not join etcd.

```bash
kctl kubernetes scale lab --add-workers node-108:192.168.40.108
kctl kubernetes scale lab --remove node-108
```

`--remove` cordons and drains the node, then deletes it from the API. A control-plane node leaves the etcd member list first. The command refuses a removal that would drop etcd below quorum, and it refuses to remove the last control-plane node.

### Drain Node for Maintenance

A host generation update cordons and drains a Kubernetes member before the new NixOS generation is activated, one member at a time, then uncordons it after the node is Ready on that generation. A drain that does not finish within 300s does not activate the generation. Nodes that are not members of a cluster are updated without a cordon. `drainVms` is only the virtual-machine flag.

```bash
kctl update cluster apply -f cluster-update.yaml
```

### Update Cluster (e.g., Change MTU)

Edit `lab-cluster.yaml`:
```yaml
  calicoConfig:
    mtu: 1400  # Changed from 1450
```

Then:
```bash
kctl kubernetes plan -f lab-cluster.yaml   # Shows "update" action
kctl kubernetes apply -f lab-cluster.yaml  # Rolling update
```

### Delete Cluster

```bash
kctl kubernetes delete lab-ha --confirm
```

This renders `enable = false` in Nix, stops all Kubernetes services, and removes the cluster record.

## Troubleshooting

### Check Service Status

```bash
ssh node-105 'systemctl status kube-apiserver etcd kubelet'
ssh node-103 'systemctl status kubelet'
```

### Check Logs

```bash
# API server logs
ssh node-105 'journalctl -u kube-apiserver -f'

# etcd logs
ssh node-105 'journalctl -u etcd -f'

# kubelet logs
ssh node-103 'journalctl -u kubelet -f'

# Calico logs
ssh node-105 'kubectl logs -n calico-system -l k8s-app=calico-node'
```

### Check etcd Health

```bash
ssh node-105
export ETCDCTL_API=3
etcdctl --endpoints=https://192.168.40.105:2379 \
  --cacert=/var/lib/kubernetes/ca.crt \
  --cert=/var/lib/kubernetes/etcd-client.crt \
  --key=/var/lib/kubernetes/etcd-client.key \
  member list

# Should show 3 members
```

### Check Calico Status

```bash
ssh node-105
kubectl get pods -n calico-system
kubectl get installation default -o yaml
kubectl get ippools -o yaml
```

### Port Not Free

**Error:** "Port 6443 already in use"

**Fix:** Something else is using the port. Check:
```bash
ssh node-105 'sudo ss -tulpn | grep 6443'
```

### CIDR Overlap

**Error:** "Pod CIDR overlaps with existing network"

**Fix:** Change `clusterCidr` in manifest to avoid 192.168.x.x ranges.

### Node Not Ready

**Error:** Node shows NotReady in `kubectl get nodes`

**Check:**
```bash
# Kubelet status
ssh <node> 'systemctl status kubelet'

# Calico node running?
kubectl get pods -n calico-system -o wide | grep <node>

# CNI config present?
ssh <node> 'ls -la /etc/cni/net.d/'
```

## Pre-Deployment Checklist

Before running `kctl kubernetes apply`:

- [ ] All 6 nodes are approved in `kctl get nodes`
- [ ] Static IP addresses confirmed on management network
- [ ] VIP address (192.168.40.200) is not in use
- [ ] All NICs have MTU ≥ 1500
- [ ] No existing Kubernetes running on these nodes
- [ ] All nodes have at least 4 CPU cores, 8 GB RAM
- [ ] OS disk has at least 50 GB free (for etcd + container images)
- [ ] Nodes can reach each other on management network
- [ ] Nodes can reach internet for image pulls (or configure registry mirror)
- [ ] VMs on these nodes have been drained (if needed)
- [ ] Operator has confirmed node assignments

## Performance Considerations

### Recommended Resources

**Control Plane Nodes:**
- CPU: 4+ cores
- RAM: 8+ GB
- Disk: 100+ GB (etcd data + container images)

**Worker Nodes:**
- CPU: 8+ cores
- RAM: 16+ GB
- Disk: 100+ GB (container images, ephemeral storage)

### Networking

- **Management NIC:** 1 Gbps minimum (API traffic)
- **MTU:** 1500 on physical NICs (VXLAN subtracts 50 bytes)
- **Latency:** <5ms between control plane nodes for etcd

### etcd Performance

- **Disk:** SSD strongly recommended (etcd is latency-sensitive)
- **IOPS:** 100+ sequential write IOPS
- **Latency:** <10ms fsync

## Security Notes

- **PKI:** NixOS easyCerts generates CA and certificates on bootstrap node
- **Service Accounts:** Kubernetes RBAC enabled by default
- **NetworkPolicy:** Calico enforces policies (start with deny-all for sensitive namespaces)
- **API Access:** Only accessible on management network (192.168.40.0/24)
- **Pod Security:** Consider Pod Security Standards (restricted/baseline/privileged)

## Integration with kcore

### Running VMs Alongside Kubernetes

- kcore VMs continue using `ch-vm` and cloud-hypervisor
- Kubernetes pods run in separate network namespace
- Both can coexist on the same node
- Resource limits should account for both (e.g., reserve 50% for VMs)

### When NOT to Run Kubernetes on a Node

- **kcore controller node (node-107):** Keep controller isolated unless explicitly needed
- **High-density VM hosts:** Kubernetes overhead reduces VM capacity
- **Ceph OSD-only nodes:** Leave dedicated to storage

### Scheduling Preference

Tag worker nodes for specific workloads:
```bash
kubectl label nodes node-103 workload=compute-heavy
kubectl label nodes worker-2 workload=storage
```

Then use node selectors in pod specs:
```yaml
nodeSelector:
  workload: compute-heavy
```

## Next Steps After Deployment

1. **Install CSI driver** for persistent volumes
   - Ceph RBD (if SAN cluster exists)
   - Local hostPath provisioner

2. **Install Ingress controller**
   - nginx-ingress
   - traefik

3. **Setup monitoring**
   - Prometheus operator
   - Grafana
   - kube-state-metrics

4. **Setup logging**
   - Loki
   - Promtail

5. **Configure backups**
   - etcd snapshots (to kcore controller or S3)
   - Velero for application backups

---

**Need Help?**
- Full implementation: [kubernetes-ha-calico-implementation.md](./kubernetes-ha-calico-implementation.md)
- Security model: [security-k8s-vs-kcore.md](./security-k8s-vs-kcore.md)
- Bare-metal context: [bare-metal-kubernetes.md](./bare-metal-kubernetes.md)
