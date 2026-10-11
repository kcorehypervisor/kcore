# Kubernetes on kcore - Implementation Roadmap

**Created:** 2026-10-11  
**Working Tree:** `/mnt/data/dev/kcore/kcore-rust-k8s`  
**Target:** Production HA Kubernetes with Calico CNI  
**Execution Model:** Agent-driven phases - complete tasks as fast as possible, no calendar time estimates

## Document Index

| Document | Purpose |
|----------|---------|
| [kubernetes-ha-calico-implementation.md](./kubernetes-ha-calico-implementation.md) | **Master implementation plan** - Complete technical design |
| [kubernetes-networking-architecture.md](./kubernetes-networking-architecture.md) | **Network architecture** - VIP, BGP, ingress, load balancing |
| [kubernetes-quickstart.md](./kubernetes-quickstart.md) | **Operator guide** - How to deploy and manage clusters |
| [kubernetes-cluster-examples.yaml](./kubernetes-cluster-examples.yaml) | **Sample specs** - Dev, HA, large, BGP configurations |
| [proto-snippets-controller.proto](./proto-snippets-controller.proto) | **gRPC API (controller)** - Copy into `proto/controller.proto` |
| [proto-snippets-node.proto](./proto-snippets-node.proto) | **gRPC API (node)** - Copy into `proto/node.proto` |

## What Changed from Original Plan

### Original (bare-metal-kubernetes.md)
- ❌ Single control plane node
- ❌ Flannel CNI
- ❌ easyCerts only
- ❌ No HA
- ❌ No ingress/load balancing

### New (kubernetes-ha-calico-implementation.md)
- ✅ **3 control planes** (stacked etcd)
- ✅ **Cilium CNI** (eBPF, native routing)
- ✅ **High Availability** (VIP with keepalived)
- ✅ **Cilium LoadBalancer** (L2 mode, no MetalLB)
- ✅ **3 workers** minimum
- ✅ **Full automation** (gRPC API)
- ✅ **Scaling support**

## Implementation Phases

Complete each phase sequentially. Each phase has multiple tasks. No fixed calendar time - agents work as fast as possible.

---

## Phase 1: Foundation

**Goal:** Proto definitions compile, NixOS module evaluates, database schema ready

### Task 1.1: gRPC Protocol Definitions
- [ ] Add messages to `proto/controller.proto` (copy from `proto-snippets-controller.proto`)
- [ ] Add messages to `proto/node.proto` (copy from `proto-snippets-node.proto`)
- [ ] Run `buf generate`
- [ ] Verify: `cargo check` passes on all crates

### Task 1.2: Database Schema
- [ ] Create migration: `kubernetes_clusters` table
  - Columns: name (PK), spec_json, generation, created_at
- [ ] Create migration: `kubernetes_cluster_status` table
  - Columns: name (FK), phase, message, ready_control_planes, ready_workers, last_updated
- [ ] Run `sqlx migrate run`
- [ ] Verify: SQLite schema includes new tables

### Task 1.3: NixOS Module
- [ ] Create `modules/kcore-kubernetes.nix`
  - Support control-plane and worker roles
  - Calico configuration (VXLAN mode, BGP mode)
  - keepalived integration for VIP
  - etcd stacked configuration
  - Firewall rules per role
- [ ] Create `tests/kubernetes-module-eval.nix`
- [ ] Verify: `nix build .#checks.x86_64-linux.kubernetes-module-eval` succeeds

### Task 1.4: Nix Code Generation
- [ ] Implement `generate_kubernetes_nix()` in `crates/controller/src/nix_codegen.rs`
- [ ] Unit tests for Nix escaping (reuse `nix_escape` from Ceph)
- [ ] CIDR validation helpers (overlap detection)
- [ ] etcd initial-cluster string builder
- [ ] Verify: `cargo test -p kcore-controller -- kubernetes::nix` passes

**Phase 1 Exit Criteria:**
- ✅ Proto compiles, Rust types generated
- ✅ SQLite migrations applied
- ✅ NixOS module evaluates without errors
- ✅ Nix code generation tests pass

---

## Phase 2: Node Agent

**Goal:** Node agent can receive config via gRPC, write Nix, and rebuild NixOS

### Task 2.1: ApplyKubernetesConfig RPC
- [ ] Implement `ApplyKubernetesConfig` in `crates/node-agent/src/grpc/admin.rs`
  - Validate request fields
  - Render Nix from request using `generate_kubernetes_nix()`
  - Write to `/etc/nixos/kcore-kubernetes.nix`
  - Copy `modules/kcore-kubernetes.nix` from embedded binary
  - Write kubeconfig to `/var/lib/kubernetes/admin.conf` (if provided)
  - Acquire apply lock (prevent concurrent VM/Ceph/K8s applies)
  - Run `nixos-rebuild test`
  - Run `nixos-rebuild switch`
  - Verify services active (kubelet, apiserver if control-plane, etcd if control-plane)
  - Return success/failure

### Task 2.2: GetKubernetesStatus RPC
- [ ] Implement `GetKubernetesStatus` in node agent
  - Check if Kubernetes enabled in NixOS config
  - Query systemctl for service status (kubelet, apiserver, etcd, calico-node)
  - Query kubectl for node status (if kubectl available)
  - Count running pods on this node
  - Return structured status

### Task 2.3: DrainKubernetesNode RPC
- [ ] Implement `DrainKubernetesNode` in node agent
  - Run `kubectl drain <this-node>` with options from request
  - Return drain status (completed/partial/failed)

### Task 2.4: Preflight Checks
- [ ] Check interface exists (`ip link show <iface>`)
- [ ] Check address on interface (`ip addr show <iface>`)
- [ ] Check interface not DHCP-managed (parse NetworkManager/systemd-networkd config)
- [ ] Check ports available (6443, 2379, 2380, 10250 for control-plane; 10250 for worker)
  - Use `ss -tulpn | grep <port>`
- [ ] Check CIDR non-overlapping with host networks
  - Parse `ip route` and compare with requested pod/service CIDRs

### Task 2.5: Tests
- [ ] Unit tests: Nix rendering with various roles and configs
- [ ] Integration tests: Mock controller calling ApplyKubernetesConfig
- [ ] Verify: `cargo test -p kcore-node-agent -- kubernetes` passes

**Phase 2 Exit Criteria:**
- ✅ Node agent can apply Kubernetes config
- ✅ Node agent can report status
- ✅ Node agent can drain itself
- ✅ Preflight checks implemented
- ✅ All node agent tests pass

---

## Phase 3: Controller Reconciler (Part 1: Planning)

**Goal:** Controller can validate cluster spec and run preflights on all nodes

### Task 3.1: PlanKubernetesCluster RPC
- [ ] Implement in `crates/controller/src/kubernetes_cluster_reconciler.rs`
- [ ] Validate cluster spec:
  - At least 1 control plane
  - Control plane count is odd (1, 3, 5, 7)
  - All node IDs exist in controller DB
  - All nodes are approved
  - CIDRs are valid and non-overlapping
  - VIP address not in use (if HA enabled)
- [ ] Call preflight on each node in parallel (use tokio::spawn)
  - Control planes: check ports 6443, 2379, 2380, 10250
  - Workers: check port 10250
  - All: check interface, address, CIDRs
- [ ] Collect results into `NodePlanResult` list
- [ ] Generate Nix preview for first control plane
- [ ] Determine action: "create", "update", "replace", "unchanged"
- [ ] Return plan response with blocking issues, warnings, node plans

### Task 3.2: Validation Helpers
- [ ] `check_cidr_overlap()` - compare pod/service CIDRs with management/Ceph networks
- [ ] `check_vip_free()` - ping VIP address, return false if responds
- [ ] `build_etcd_initial_cluster()` - format "node1=https://ip1:2380,node2=..."
- [ ] Unit tests for each helper

### Task 3.3: Tests
- [ ] Mock node clients returning preflight success/failure
- [ ] Test plan with missing nodes
- [ ] Test plan with CIDR overlaps
- [ ] Test plan with VIP in use
- [ ] Test plan with all preflights passing
- [ ] Verify: `cargo test -p kcore-controller -- kubernetes::plan` passes

**Phase 3 Exit Criteria:**
- ✅ PlanKubernetesCluster validates specs
- ✅ Preflight checks run in parallel
- ✅ Plan returns actionable results
- ✅ All planning tests pass

---

## Phase 4: Controller Reconciler (Part 2: Creation)

**Goal:** Controller can bootstrap a complete HA cluster

### Task 4.1: CreateKubernetesCluster - Bootstrap
- [ ] Implement `CreateKubernetesCluster` in reconciler
- [ ] Refuse if `confirm` is false or plan has blocking issues
- [ ] Find bootstrap node (first control plane with `initial_cluster: true`)
- [ ] Generate bootstrap config:
  - `role: control-plane`
  - `initial_cluster: true`
  - `easyCerts: true` (NixOS generates PKI)
  - etcd initial-cluster string with all control planes
  - VIP config (if HA enabled)
- [ ] Call `ApplyKubernetesConfig` on bootstrap node via gRPC
- [ ] Poll until nixos-rebuild completes (check apply status)

### Task 4.2: CreateKubernetesCluster - Wait for API
- [ ] Poll `https://<bootstrap-ip>:6443/healthz` until returns 200 OK
- [ ] Timeout: 5 minutes
- [ ] If timeout, return error and set cluster phase to "failed"

### Task 4.3: CreateKubernetesCluster - Retrieve Kubeconfig
- [ ] Call node agent `GetFile` RPC to read `/var/lib/kubernetes/admin.conf` from bootstrap node
- [ ] Store kubeconfig in controller memory (needed for joining other nodes)

### Task 4.4: CreateKubernetesCluster - Join Control Planes
- [ ] For each additional control plane (`joinExisting: true`):
  - Generate join config (same as bootstrap but `initial_cluster: false`, include kubeconfig)
  - Call `ApplyKubernetesConfig` on node
  - Wait for rebuild completion
  - Wait for `kubectl get nodes` to show node Ready (use kubeconfig)
- [ ] Sequential joins (one at a time to avoid etcd race conditions)

### Task 4.5: CreateKubernetesCluster - Wait for etcd Quorum
- [ ] Use kubeconfig to run `kubectl exec` into etcd pod
- [ ] Run `etcdctl member list` until expected member count reached
- [ ] Verify all members healthy
- [ ] Timeout: 5 minutes

### Task 4.6: CreateKubernetesCluster - Join Workers
- [ ] For each worker:
  - Generate worker config (role: worker, include kubeconfig, no etcd)
  - Call `ApplyKubernetesConfig` on node
  - Wait for rebuild completion
- [ ] Parallel worker joins (workers don't affect etcd)

### Task 4.7: CreateKubernetesCluster - Wait for Calico
- [ ] Poll `kubectl get pods -n calico-system` until all Running
- [ ] Wait for `kubectl get nodes` to show all nodes Ready
- [ ] Timeout: 10 minutes (includes image pulls)

### Task 4.8: CreateKubernetesCluster - Persist
- [ ] Insert row into `kubernetes_clusters` table (spec JSON, generation = 1)
- [ ] Insert row into `kubernetes_cluster_status` table (phase = "ready", counts)
- [ ] Return `KubernetesCluster` response

### Task 4.9: Tests
- [ ] Mock node clients for bootstrap, join, worker scenarios
- [ ] Test bootstrap failure (API server doesn't start)
- [ ] Test etcd quorum loss during join
- [ ] Test worker join failure
- [ ] Verify: `cargo test -p kcore-controller -- kubernetes::create` passes

**Phase 4 Exit Criteria:**
- ✅ Can bootstrap first control plane
- ✅ Can join additional control planes
- ✅ Can join workers
- ✅ etcd quorum validated
- ✅ Calico readiness validated
- ✅ Cluster persisted to DB
- ✅ All creation tests pass

---

## Phase 5: Controller Reconciler (Part 3: Operations)

**Goal:** Update, scale, delete, and monitor clusters

### Task 5.1: UpdateKubernetesCluster
- [ ] Detect changes in spec (diff old vs new)
- [ ] Refuse immutable changes (clusterCidr, serviceCidr)
- [ ] Allow mutable changes (MTU, bgpPeers, node list)
- [ ] Rolling update:
  - Update control planes one at a time (maintain quorum)
  - Update workers in batches (drain, update, uncordon)
- [ ] Bump generation
- [ ] Return updated cluster

### Task 5.2: ScaleKubernetesCluster
- [ ] Add nodes: same as join workers/control-planes
- [ ] Remove nodes:
  - Drain node first (call `DrainKubernetesNode`)
  - Run `kubectl delete node <name>`
  - Render `enable: false` in Nix, apply to node
  - Remove from cluster record
- [ ] Return updated cluster

### Task 5.3: DeleteKubernetesCluster
- [ ] For each node in cluster:
  - Render `enable: false` in Nix
  - Call `ApplyKubernetesConfig`
  - Wait for Kubernetes services to stop
- [ ] If force=true, skip unreachable nodes
- [ ] Delete rows from DB
- [ ] Return success

### Task 5.4: GetKubernetesClusterHealth
- [ ] For each node:
  - Call `GetKubernetesStatus` via gRPC
  - Collect kubelet/apiserver/etcd status
- [ ] Use kubeconfig to query:
  - `kubectl get nodes` (Ready status)
  - `kubectl get pods -A` (count Running)
  - `etcdctl member list` (quorum health)
  - `curl https://VIP:6443/healthz` (API reachable)
- [ ] Aggregate into overall health: "healthy", "degraded", "critical"
- [ ] Return health response

### Task 5.5: Tests
- [ ] Test update with mutable changes
- [ ] Test update refusing immutable changes
- [ ] Test scale up (add worker)
- [ ] Test scale down (remove worker)
- [ ] Test delete with unreachable node (force)
- [ ] Test health with degraded node
- [ ] Verify: `cargo test -p kcore-controller -- kubernetes::operations` passes

**Phase 5 Exit Criteria:**
- ✅ Can update clusters (rolling updates)
- ✅ Can scale clusters (add/remove nodes)
- ✅ Can delete clusters
- ✅ Can query cluster health
- ✅ All operation tests pass

---

## Phase 6: CLI

**Goal:** Operator can manage clusters via `kctl kubernetes` commands

### Task 6.1: YAML Parsing
- [ ] Add `serde` deserializers for `KubernetesCluster`, `KubernetesClusterSpec`
- [ ] Implement `load_kubernetes_spec(path: &str) -> Result<KubernetesClusterSpec>`
- [ ] Validate required fields
- [ ] Tests for valid and invalid YAML

### Task 6.2: kctl kubernetes plan
- [ ] Implement in `crates/kctl/src/commands/kubernetes.rs`
- [ ] Load spec from YAML file
- [ ] Call controller `PlanKubernetesCluster` RPC
- [ ] Format output:
  - Action (create/update/unchanged)
  - Blocking issues (red)
  - Warnings (yellow)
  - Node plan results (✅/❌ per node)
  - Nix preview (if requested)
- [ ] Exit code 1 if blocking issues exist

### Task 6.3: kctl kubernetes apply
- [ ] Load spec from YAML
- [ ] Run plan first (auto-preflight)
- [ ] Refuse if blocking issues
- [ ] Prompt for confirmation: "This will rebuild NixOS on X nodes. Continue? (yes/no)"
- [ ] Call controller `CreateKubernetesCluster` or `UpdateKubernetesCluster`
- [ ] Stream progress (if controller supports streaming)
- [ ] Display success or failure

### Task 6.4: kctl kubernetes get
- [ ] Call controller `GetKubernetesCluster` RPC
- [ ] Format output as YAML or table
- [ ] Support `--output yaml|json|table`

### Task 6.5: kctl kubernetes list
- [ ] Call controller `ListKubernetesClusters` RPC
- [ ] Display table: Name, Phase, Generation, Control Planes, Workers

### Task 6.6: kctl kubernetes delete
- [ ] Require `--confirm` flag
- [ ] Call controller `DeleteKubernetesCluster` RPC
- [ ] Display success

### Task 6.7: kctl kubernetes health
- [ ] Call controller `GetKubernetesClusterHealth` RPC
- [ ] Display overall health
- [ ] Display per-node status (Ready, kubelet active, etcd active, etc.)
- [ ] Color-code: green=healthy, yellow=degraded, red=critical

### Task 6.8: Tests
- [ ] Test plan with sample YAML
- [ ] Test apply with mock controller
- [ ] Test error handling (file not found, invalid YAML, gRPC failure)
- [ ] Verify: `cargo test -p kctl -- kubernetes::cli` passes

**Phase 6 Exit Criteria:**
- ✅ All kctl commands implemented
- ✅ YAML parsing works
- ✅ Human-readable output
- ✅ All CLI tests pass

---

## Phase 7: MCP Integration

**Goal:** Cursor agents can manage Kubernetes clusters via MCP

### Task 7.1: MCP Catalog Entry
- [ ] Add `kubernetes-cluster` kind to `kcore-mcp/packages/server/src/catalog.ts`
- [ ] Register advise, plan, apply, delete operations

### Task 7.2: MCP Advise
- [ ] Questions:
  - Which control plane node IDs? (list, minimum 1, recommend 3)
  - Which worker node IDs? (list)
  - Control plane advertise addresses? (one per node)
  - Worker advertise addresses? (one per node)
  - Interface name? (e.g., enp6s0)
  - Pod CIDR? (default 10.244.0.0/16)
  - Service CIDR? (default 10.96.0.0/12)
  - Enable HA? (yes/no, default yes if 3+ control planes)
  - VIP address? (if HA enabled)
  - CNI mode? (vxlan or bgp, default vxlan)
- [ ] Return `KubernetesClusterSpec` from answers

### Task 7.3: MCP Plan
- [ ] Call controller `PlanKubernetesCluster` RPC
- [ ] Format response for MCP client:
  - Show blocking issues prominently
  - Show node plan results
  - Show Nix preview (collapsed by default)

### Task 7.4: MCP Apply
- [ ] Check `confirm: true` in request
- [ ] Call controller `CreateKubernetesCluster` RPC
- [ ] Return success or error

### Task 7.5: MCP Delete
- [ ] Require `confirm: true`
- [ ] Call controller `DeleteKubernetesCluster` RPC

### Task 7.6: Tests
- [ ] Test advise flow (answer questions → spec generated)
- [ ] Test plan flow (spec → plan result)
- [ ] Test apply flow (spec → cluster created)
- [ ] Run `pnpm test` in kcore-mcp

**Phase 7 Exit Criteria:**
- ✅ MCP catalog includes kubernetes-cluster
- ✅ Advise generates valid specs
- ✅ Plan/apply/delete work via MCP
- ✅ All MCP tests pass

---

## Phase 8: Lab Deployment (Requires Operator Approval)

**Goal:** Deploy first production HA cluster on lab hardware

### Task 8.1: Pre-Deployment Checklist
- [ ] **BLOCKING:** Operator confirms 6 node assignments
- [ ] **BLOCKING:** Operator confirms static IPs on 192.168.40.0/24
- [ ] **BLOCKING:** Operator confirms VIP (192.168.40.200) is free
- [ ] **BLOCKING:** Operator drains VMs from these 6 nodes
- [ ] **BLOCKING:** Operator approves running nixos-rebuild on production nodes
- [ ] Backup controller database: `kctl backup-cluster`
- [ ] Document rollback plan

### Task 8.2: Create Cluster Spec
- [ ] Create `lab-ha.yaml` with:
  - 3 control planes: node-105, node-107, node-119
  - 3 workers: node-103, worker-2, worker-3
  - VIP: 192.168.40.200
  - Calico VXLAN mode
- [ ] Review spec with operator

### Task 8.3: Plan
- [ ] Run `kctl kubernetes plan -f lab-ha.yaml`
- [ ] Review plan output:
  - All nodes preflight passed?
  - No blocking issues?
  - Nix preview looks correct?
- [ ] Get operator approval to proceed

### Task 8.4: Apply
- [ ] Run `kctl kubernetes apply -f lab-ha.yaml`
- [ ] Monitor controller logs: `journalctl -u kcore-controller -f`
- [ ] Expected duration: 10-15 minutes
- [ ] Watch for errors

### Task 8.5: Verify Cluster
- [ ] Run `kctl kubernetes health lab-ha`
- [ ] SSH to control plane: `ssh node-105`
- [ ] Check nodes: `kubectl get nodes`
  - Expected: 6 nodes, all Ready
- [ ] Check pods: `kubectl get pods -A`
  - Expected: kube-system, calico-system, all Running
- [ ] Check etcd: `etcdctl member list`
  - Expected: 3 members, all healthy

### Task 8.6: Test VIP Failover
- [ ] Stop API server on node-105: `ssh node-105 'sudo systemctl stop kube-apiserver'`
- [ ] Test kubectl from external: `kubectl --kubeconfig=... --server=https://192.168.40.200:6443 get nodes`
  - Expected: Still works (VIP moved to node-107 or node-119)
- [ ] Restart API server: `ssh node-105 'sudo systemctl start kube-apiserver'`
- [ ] Verify VIP returns or stays on backup

### Task 8.7: Deploy Test Workload
- [ ] Create deployment: `kubectl create deployment nginx --image=nginx --replicas=6`
- [ ] Wait for all pods Running: `kubectl get pods -o wide`
  - Expected: 2 pods per worker
- [ ] Test scaling: `kubectl scale deployment nginx --replicas=12`
  - Expected: 4 pods per worker

### Task 8.8: Document Results
- [ ] Record node IDs and IPs used
- [ ] Record VIP address
- [ ] Screenshot `kubectl get nodes` output
- [ ] Note any issues encountered
- [ ] Update operator runbook

**Phase 8 Exit Criteria:**
- ✅ 6-node cluster deployed
- ✅ All nodes Ready
- ✅ VIP failover tested
- ✅ Test workload deployed
- ✅ Results documented

---

## Phase 9: Cilium LoadBalancer Configuration

**Goal:** Services can get real IPs from network (no external load balancer needed)

### Task 9.1: Configure Cilium LoadBalancer IP Pool
- [ ] Create `CiliumLoadBalancerIPPool` resource
  - CIDR: `192.168.40.220/29` (8 IPs: .220-.227)
  - Reserve IPs outside DHCP range
- [ ] Verify pool created: `kubectl get ciliumloadbalancerippool`

### Task 9.2: Configure L2 Announcement Policy
- [ ] Create `CiliumL2AnnouncementPolicy` resource
  - Interface: `enp6s0` (management NIC)
  - Enable for LoadBalancer IPs
- [ ] Verify: `kubectl get ciliumnodeconfig`

### Task 9.3: Test LoadBalancer Service
- [ ] Deploy test app:
  ```bash
  kubectl create deployment echo --image=hashicorp/http-echo --port=5678 -- -text="Hello from Kubernetes"
  kubectl expose deployment echo --type=LoadBalancer --port=80 --target-port=5678
  ```
- [ ] Wait for EXTERNAL-IP assignment: `kubectl get svc echo`
  - Expected: EXTERNAL-IP = 192.168.40.220 (or next available)
- [ ] Test from external host:
  ```bash
  curl http://192.168.40.220
  ```
  - Expected: "Hello from Kubernetes"

### Task 9.4: Verify Cilium L2 Announcements
- [ ] Check which node is announcing IP:
  ```bash
  kubectl get ciliumnodeconfig -o yaml | grep -A5 loadBalancerIPs
  ```
- [ ] Check ARP table on external host:
  ```bash
  arp -a | grep 192.168.40.220
  ```
  - Should show MAC address of one Kubernetes node

### Task 9.5: Test LoadBalancer Failover
- [ ] Note which node is currently announcing 192.168.40.220
- [ ] Stop Cilium on that node:
  ```bash
  ssh <node> 'sudo systemctl stop cilium'
  ```
- [ ] Verify another node takes over (<5s):
  ```bash
  arp -a | grep 192.168.40.220
  ```
- [ ] Verify service still accessible:
  ```bash
  curl http://192.168.40.220
  ```
- [ ] Restart Cilium:
  ```bash
  ssh <node> 'sudo systemctl start cilium'
  ```

### Task 9.6: Update Cluster Spec
- [ ] Document LoadBalancer IP pool in `lab-ha.yaml`:
  ```yaml
  ciliumConfig:
    loadBalancer:
      mode: l2
      ipPools:
        - name: default-pool
          cidr: 192.168.40.220/29
      l2Announcements:
        - interface: enp6s0
  ```

**Phase 9 Exit Criteria:**
- ✅ LoadBalancer IP pool configured
- ✅ L2 announcements working
- ✅ Services get real IPs from network
- ✅ External hosts can reach LoadBalancer IPs
- ✅ Failover tested (<5s)
- ✅ Documented in cluster spec

---

## Phase 10: Validation & Testing

**Goal:** Cluster passes all production readiness tests

### Task 10.1: Failover Testing - Control Plane
- [ ] Test 1: Stop kube-apiserver on node-105
  - Verify: VIP moves to another control plane
  - Verify: kubectl commands still work via VIP
  - Verify: New pods can be scheduled
  - Restart kube-apiserver
- [ ] Test 2: Stop etcd on node-107
  - Verify: Cluster still works (2/3 quorum)
  - Verify: Can create/delete pods
  - Restart etcd
- [ ] Test 3: Hard reboot node-119 (power cycle)
  - Verify: Cluster stays operational
  - Verify: Node rejoins after reboot

### Task 10.2: Failover Testing - Workers
- [ ] Test 1: Drain worker-3
  - Run `kubectl drain worker-3 --ignore-daemonsets`
  - Verify: Pods moved to other workers
  - Run `kubectl uncordon worker-3`
  - Verify: Pods can be scheduled on worker-3 again
- [ ] Test 2: Stop kubelet on node-103
  - Verify: Pods marked NotReady after 90s
  - Verify: Pods rescheduled to other workers
  - Restart kubelet

### Task 10.3: Scaling Tests
- [ ] Test 1: Add worker-4
  - Edit `lab-ha.yaml`, add worker-4 to workers.nodes
  - Run `kctl kubernetes apply -f lab-ha.yaml`
  - Verify: worker-4 joins cluster, shows Ready
- [ ] Test 2: Remove worker-4
  - Edit `lab-ha.yaml`, remove worker-4
  - Run `kctl kubernetes apply -f lab-ha.yaml`
  - Verify: worker-4 drained and removed from cluster
- [ ] Test 3: Scale deployment
  - `kubectl scale deployment nginx --replicas=50`
  - Verify: Pods spread across all workers
  - Verify: No scheduling failures

### Task 10.4: NetworkPolicy Tests
- [ ] Create deny-all policy in test namespace
- [ ] Deploy app, verify it cannot reach other apps
- [ ] Add allow policy, verify connectivity restored
- [ ] Verify: Calico enforces policies correctly

### Task 10.5: Load Testing
- [ ] Deploy load generator (e.g., hey or ab)
- [ ] Run 10k requests against nginx deployment
- [ ] Monitor:
  - CPU usage on workers
  - Network throughput
  - Pod restart count (should be 0)
  - Calico node performance
- [ ] Verify: No crashes, no OOM kills, no network issues

### Task 10.6: Rolling Update Test
- [ ] Change MTU in `lab-ha.yaml` from 1450 to 1400
- [ ] Run `kctl kubernetes plan -f lab-ha.yaml`
  - Verify: Shows "update" action
- [ ] Run `kctl kubernetes apply -f lab-ha.yaml`
- [ ] Monitor: Control planes updated one at a time
- [ ] Monitor: Workers updated in batches
- [ ] Verify: No downtime for applications during update

### Task 10.7: Disaster Recovery Test
- [ ] Stop all 3 control planes simultaneously
- [ ] Wait 5 minutes
- [ ] Start all 3 control planes
- [ ] Verify: etcd quorum restored
- [ ] Verify: Cluster operational

**Phase 10 Exit Criteria:**
- ✅ Control plane failover works
- ✅ Worker failover works
- ✅ Scaling up/down works
- ✅ NetworkPolicy enforcement works
- ✅ Load testing passes
- ✅ Rolling updates work without downtime
- ✅ Disaster recovery works

---

## Phase 11: Documentation & Handoff

**Goal:** Operator can manage cluster without developer assistance

### Task 11.1: Update Main README
- [ ] Add Kubernetes section to `/mnt/data/dev/kcore/kcore-rust-k8s/README.md`
- [ ] Link to all documentation
- [ ] Describe integration with kcore

### Task 11.2: Operator Runbook
- [ ] Common operations:
  - How to create cluster
  - How to scale cluster
  - How to drain node for maintenance
  - How to update cluster configuration
  - How to delete cluster
- [ ] Troubleshooting:
  - Node NotReady
  - Pod stuck Pending
  - API server unreachable
  - etcd quorum loss
  - Calico issues
  - Ingress not working
- [ ] Emergency procedures:
  - How to force-delete cluster
  - How to recover from etcd disaster
  - How to rollback nixos-rebuild

### Task 11.3: Architecture Diagrams
- [ ] Network architecture (VIP, Calico, MetalLB, BGP)
- [ ] Control plane HA (etcd, keepalived)
- [ ] Ingress flow (external → MetalLB → nginx → pod)
- [ ] Service mesh options (Cilium, Envoy)

### Task 11.4: Video Walkthrough (Optional)
- [ ] Record cluster creation from scratch
- [ ] Demonstrate failover
- [ ] Demonstrate scaling
- [ ] Demonstrate application deployment with ingress

### Task 11.5: Operator Training
- [ ] Walk through quickstart guide
- [ ] Demonstrate all kctl commands
- [ ] Review troubleshooting procedures
- [ ] Answer operator questions

**Phase 11 Exit Criteria:**
- ✅ All documentation complete
- ✅ Operator trained
- ✅ Operator can perform common tasks without help
- ✅ Troubleshooting runbook validated

---

## Key Milestones

| Milestone | Phase | Gate |
|-----------|-------|------|
| ✅ **Proto & Module Eval** | 1 | `nix build .#checks.x86_64-linux.kubernetes-module-eval` |
| ✅ **Node Agent Apply** | 2 | `cargo test -p kcore-node-agent -- kubernetes` |
| ✅ **Controller Planning** | 3 | Integration test: plan returns valid results |
| ✅ **Cluster Creation** | 4 | Integration test: bootstrap → join → ready |
| ✅ **Cluster Operations** | 5 | Can update/scale/delete clusters |
| ✅ **CLI Functional** | 6 | Can plan/apply YAML spec |
| ✅ **MCP Integrated** | 7 | Cursor agent can create cluster |
| ✅ **Lab Deployed** | 8 | 6-node HA cluster running |
| ✅ **Ingress Working** | 9 | External traffic reaches apps |
| ✅ **Validated** | 10 | All failover/scaling/load tests pass |
| ✅ **Production Ready** | 11 | Operator trained, docs complete |

## Dependencies & Blockers

### Code Dependencies (All Available)
- ✅ NixOS kubernetes module (in nixpkgs)
- ✅ Calico (pulled from upstream)
- ✅ keepalived (in nixpkgs)
- ✅ MetalLB (Kubernetes manifests)
- ✅ nginx-ingress (Kubernetes manifests)
- ✅ Existing Ceph pattern (to copy)
- ✅ `nix_escape()` function (exists)

### Infrastructure Dependencies (Operator Approval Required)
- ⚠️ **6 kcore nodes approved and ready**
- ⚠️ **Static IPs assigned on 192.168.40.0/24**
- ⚠️ **VIP 192.168.40.200 not in use**
- ⚠️ **No critical VMs on these 6 nodes** (or drained)
- ⚠️ **Internet access for image pulls** (or registry mirror)
- ⚠️ **10 IPs reserved for MetalLB** (192.168.40.220-230)

### Operator Decisions Required (Before Phase 8)
1. **Which 6 nodes?** (3 control planes + 3 workers)
2. **Include node-107 (controller)?** (Recommendation: No initially)
3. **Confirm static IPs?**
4. **Confirm VIP address?**
5. **Confirm MetalLB IP pool range?**
6. **Approval to run nixos-rebuild on production nodes?**

## Risk Mitigation

| Risk | Severity | Mitigation |
|------|----------|-----------|
| nixos-rebuild breaks VMs | High | Apply lock serializes operations; operator drains nodes first |
| etcd quorum loss | High | Sequential control plane joins; never drop below 2/3 |
| VIP failover delay | Medium | keepalived 5s detection; API clients auto-retry |
| Calico/kcore VXLAN conflict | Medium | Different bridges, firewall isolation |
| MetalLB IP conflict | Medium | Reserve IPs outside DHCP range; test before production |
| Ingress controller crash | Low | Deployed as Kubernetes Deployment with multiple replicas |
| BGP misconfiguration | Medium | Start with VXLAN; add BGP only after stable |

## Success Criteria

- [ ] `kubectl get nodes` shows all 6 nodes Ready
- [ ] All system pods Running (api, etcd, kubelet, calico, coredns, metallb, nginx-ingress)
- [ ] Test deployment spreads across workers
- [ ] VIP failover works (< 5s)
- [ ] etcd survives 1-node failure
- [ ] NetworkPolicy enforcement works
- [ ] External HTTP/HTTPS traffic reaches apps via ingress
- [ ] `kctl kubernetes health` reports "healthy"
- [ ] Can scale cluster (add/remove nodes)
- [ ] Rolling updates work without downtime
- [ ] Load testing passes (10k+ requests)
- [ ] Documentation complete
- [ ] Operator trained

## Post-Production Enhancements

### Phase 12+: Advanced Features (Future)
- [ ] Ceph RBD CSI driver (persistent volumes)
- [ ] Monitoring (Prometheus operator, Grafana)
- [ ] Logging (Loki + Promtail)
- [ ] Cert-manager (automatic TLS)
- [ ] Calico BGP mode (peer with datacenter routers)
- [ ] Cilium CNI (eBPF-based alternative)
- [ ] Cilium Ingress (replace nginx-ingress)
- [ ] WebAssembly filters (custom logic in Envoy)
- [ ] GPU scheduling (device plugin for AI workloads)
- [ ] Multi-cluster federation
- [ ] GitOps (ArgoCD, Flux)
- [ ] Service mesh (Istio, Linkerd)

## Command Reference

### Development
```bash
# Proto
buf generate

# Tests
cargo test -p kcore-controller -- kubernetes
cargo test -p kcore-node-agent -- kubernetes
cargo test -p kctl -- kubernetes

# Nix module
nix build .#checks.x86_64-linux.kubernetes-module-eval

# Run services
cargo run -p kcore-controller
cargo run -p kcore-node-agent
```

### Deployment
```bash
# Plan
kctl kubernetes plan -f lab-ha.yaml

# Create
kctl kubernetes apply -f lab-ha.yaml

# Status
kctl kubernetes get lab-ha
kctl kubernetes health lab-ha

# Scale
# (edit lab-ha.yaml to add/remove nodes)
kctl kubernetes apply -f lab-ha.yaml

# Delete
kctl kubernetes delete lab-ha --confirm
```

### Debugging
```bash
# Controller logs
journalctl -u kcore-controller -f

# Node agent logs
ssh node-105 'journalctl -u kcore-node-agent -f'

# Kubernetes services
ssh node-105 'systemctl status kube-apiserver etcd kubelet'

# etcd health
ssh node-105 'etcdctl member list'
ssh node-105 'etcdctl endpoint health'

# kubectl (from control plane)
ssh node-105 'kubectl get nodes'
ssh node-105 'kubectl get pods -A'
ssh node-105 'kubectl logs -n calico-system -l k8s-app=calico-node'

# Ingress
kubectl get svc -n ingress-nginx
kubectl logs -n ingress-nginx -l app.kubernetes.io/component=controller

# MetalLB
kubectl get ipaddresspools -n metallb-system
kubectl logs -n metallb-system -l app=metallb
```

## Next Steps

1. **Review roadmap** with team/operator
2. **Get infrastructure approvals** (6 nodes, IPs, VIP, MetalLB pool)
3. **Begin Phase 1** (proto + module)
4. **Complete each phase** before moving to next
5. **Operator checkpoint** before Phase 8 (lab deployment)

---

**Execution:** Phases are completed by agents as fast as possible. No calendar time estimates. Each phase must be complete before starting the next.

**Documentation:** See [kubernetes-networking-architecture.md](./kubernetes-networking-architecture.md) for VIP, BGP, and ingress details.
