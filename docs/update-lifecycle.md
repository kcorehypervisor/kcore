# Update Lifecycle: kcore, NixOS, and State

Status: plan. Nothing here is implemented yet unless it says so.

[cluster-updates.md](cluster-updates.md) describes how one release reaches
the nodes today. This document plans what an operator should experience over
years of releases. In that time kcore moves, NixOS moves underneath it, and
the data on the nodes has to survive both.

## Three things move, at different speeds

| Layer | Examples | Rolled back by a NixOS generation? |
|---|---|---|
| kcore code | controller, node agent, `kctl`, dashboard | Yes |
| Base OS | nixpkgs: kernel, systemd, QEMU and Cloud Hypervisor, Ceph, glibc | Yes, the binaries |
| State | controller SQLite schema, Ceph on-disk format and `require-osd-release`, agent files, VM disks, `system.stateVersion` defaults | **No** |

A NixOS rollback swaps the first two. It leaves `/var` as the new version
wrote it. The NixOS paper says so, and so does every system surveyed below.
The plan is built around that third row.

## Where kcore stands

Measured on `main` at 44f19ef:

- **The base OS is a frozen `nixos-unstable` snapshot.** `flake.nix` follows
  `nixos-unstable`, and `flake.lock` freezes it at `b40629e` from 2026-03-18.
  It reports NixOS 26.05 "Yarara", kernel 6.18.18, and Cloud Hypervisor
  51.1. It is not the `nixos-26.05` branch, so it gets no backported security
  fixes. Only a new kcore release moves it.
- **Ceph comes from a second nixpkgs.** `modules/kcore-ceph.nix` fetches
  nixpkgs `e7439b6` with `builtins.fetchTarball` because the base pin cannot
  evaluate its own Ceph. That pin gives Ceph 20.2.4. It is outside
  `flake.lock`, `lib.kcore`, and the SBOM's view of inputs. A Ceph major
  version can change in a commit that never touches the lock.
- **Nodes cannot choose nixpkgs.** The host flake has only the kcore input,
  and an `updateKind: nixpkgs` update is refused for it. This is good: one
  tag names one closure.
- **The controller schema has up and down migrations.** `Database::migrate_down`
  refuses a step marked `irreversible`, and `SetClusterVersion` refuses a tag
  whose controller cannot read the database (`MIN_COMPATIBLE_SCHEMA_READER`).
- **Rollback is per node and tested on hardware.** A failed switch or an
  unhealthy boot returns to the previous generation (the v0.6.x Dell runs).
- **Missing pieces:**
  - no written controller-to-agent version skew rule;
  - no update graph, so any tag can be set over any other tag;
  - no pre-flight check separate from the update itself;
  - no Ceph-aware ordering or `ok-to-stop` gate;
  - no comparison of an updated node with its peers;
  - no record of which state changes a release makes.
- **Rollout strategy:** one node at a time, which is the only strategy the
  Community release train allows.

## What others do

### Cyberus Linux (formerly CTRL-OS)

Cyberus renamed CTRL-OS to Cyberus Linux in August 2026. It is a downstream
of NixOS with five years of support per release.

- **Cadence:** a new LTS every two years (24.05, 26.05, and 28.05 planned).
  Each release first follows upstream `release-YY.05` while upstream supports
  it (26.05 until 2026-12-31), then is maintained by Cyberus alone until 2031.
  ([announcement](https://www.cyberus-technology.de/post/announcing-ctrl-os-26-05-fast-security-patches-and-long-term-stability),
  [vulnerability management](https://www.cyberus-technology.de/post/the-cyberus-linux-approach-to-vulnerability-management))
- **Backports:** patch versions by default, minor versions rarely, and a new
  major version only as a separate attribute. Module interfaces stay stable
  within a release. Urgent CVEs ship within 24 hours on their own branch,
  before upstream's batched mass rebuild.
  ([open beta](https://discourse.nixos.org/t/ctrl-os-open-beta-nixos-lts-with-5-year-support/68754))
- **CVE scope from an SBOM:** they promise coverage for the closure that is
  actually shipped, tracked in Dependency-Track. A public tracker shows each
  issue as New, Plausible, In Progress, or Resolved.
  ([security tracker](https://security.cyberus-linux.com/))
- **Updates:** today a normal rebuild against a pinned channel tarball with
  their own binary cache. Since September 2026 they also offer image-based A/B
  updates with `systemd-sysupdate`, `systemd-repart`, and dm-verity, built in
  CI with no `nix-daemon` on the device. Delta updates and A/B boot fallback
  are on their 2027 roadmap.
  ([image-based systems](https://discourse.nixos.org/t/cyberus-linux-september-update-image-based-systems/80347))
- **Cloud Hypervisor:** Cyberus leads its live migration work. Since v54,
  Cloud Hypervisor promises live migration from the two previous releases
  (n-2), never to an older one.
  ([v54](https://www.cloudhypervisor.org/blog/cloud-hypervisor-v54.0-released/))

### Immutable and appliance systems

| System | Idea worth taking | Source |
|---|---|---|
| Fedora CoreOS | An update graph served with the releases. **Barrier** releases every node must pass through, and **dead-end** releases that cannot update further. Stable, testing, and next streams; run a few percent on testing. | [Cincinnati](https://coreos.github.io/zincati/development/cincinnati/protocol/) |
| Fedora CoreOS | Reboot only in maintenance windows, or under a fleet lock with N slots. | [Zincati](https://coreos.github.io/zincati/usage/auto-updates/) |
| Talos and Omni | Only adjacent minor versions are a tested path. OS and Kubernetes upgrades are separate. "Lock node" holds machines back as a canary split. | [upgrading](https://docs.siderolabs.com/talos/v1.14/configure-your-talos-cluster/lifecycle-management/upgrading-talos) |
| Bottlerocket | TUF-signed update metadata with expiry. Settings migrations have `forward` and `backward`; the updater refuses a target without both. | [updater](https://github.com/bottlerocket-os/bottlerocket/blob/develop/sources/updater/README.md) |
| ChromeOS and Android | Mark a slot good only after a successful boot. Rollback indexes stop a boot below a security floor. | [A/B](https://source.android.com/docs/core/ota/ab/ab_faqs) |
| systemd | Boot counting in the entry name, with `boot-complete.target` as the success gate. | [boot assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/) |
| VMware vLCM | One desired image per cluster. Compliance check, then remediation pre-check, then remediation. A local depot for offline sites. | [vLCM](https://www.vmware.com/docs/what-is-the-difference-between-vlcm-desired-image-vs-vlcm-baseline) |
| Nutanix LCM | Read-only inventory, pre-checks that run on their own, and the next node waits for data resiliency to be back to normal. | [LCM FAQ](https://www.nutanix.com/products/life-cycle-manager/lcm-faq) |
| Proxmox VE | `pve8to9 --full` checklist, run before, during, and after. Migrate VMs from older nodes to newer, never back. | [upgrade 8 to 9](https://pve.proxmox.com/wiki/Upgrade_from_8_to_9) |
| Harvester | Pre-check lists VMs that cannot live-migrate. Optionally stops them and restores them after their node is done. | [upgrades](https://documentation.suse.com/cloudnative/virtualization/latest/en/upgrades/upgrades.html) |
| Kubernetes | A written skew policy. Control plane first, kubelets up to three minors behind and never ahead. kubeadm refuses to skip a minor version. | [skew policy](https://kubernetes.io/releases/version-skew-policy/) |
| Ceph (cephadm) | mgr, then mon, then OSD. Restart a daemon only when `ok-to-stop` agrees. Set `require-osd-release` only at the end. | [cephadm upgrade](https://docs.ceph.com/en/latest/cephadm/upgrade/) |
| Flatcar | Yearly LTS streams with 18 months of support, overlapping by 6 months, so the jump is planned. | [channels](https://www.flatcar.org/docs/latest/setup/releases/switching-channels/) |

### Research

- **Upgrade failures are mostly about data, not code.** Zhang et al. (SOSP
  2021) studied 123 upgrade failures in 8 distributed systems. 63% came from
  old and new versions misreading each other's stored data or network
  messages. 28% took the whole cluster down, and HA did not help because the
  bug was deterministic. Over 80% need only two consecutive versions and at
  most three nodes. About 70% are caught by the project's existing tests run
  as an upgrade.
  ([paper](https://cs.purdue.edu/homes/yonglezh/pub/upgrade-sosp21.pdf))
  UpFuzz (NSDI 2026) fuzzes the state the old version writes and the new one
  reads. ([paper](https://www.cs.purdue.edu/homes/pfonseca/papers/nsdi26-upfuzz.pdf))
- **Judge a canary against a control group.** Google's Canary Analysis
  Service compares the canary with unchanged peers. It answers PASS, FAIL, or
  NONE, and NONE stops the rollout.
  ([ACM Queue](https://queue.acm.org/detail.cfm?id=3194655))
  Azure's Gandalf links faults to rollouts across time and place, including
  regressions that appear days later.
  ([NSDI 2020](https://www.usenix.org/system/files/nsdi20-paper-li.pdf))
- **Keep configuration changes apart from binary changes.** Holistic
  Configuration Management at Facebook (SOSP 2015) treats each as a separately
  canaried, separately revertible change.
  ([paper](https://sigops.org/s/conferences/sosp/2015/current/2015-Monterey/008-tang-online.pdf))
  Meta's Conveyor (OSDI 2023) relies on automatic guardrails and lets
  operators block releases known to be bad.
  ([paper](https://www.usenix.org/system/files/osdi23-grubic.pdf))
- **Signatures alone do not stop freeze and rollback attacks.** Cappos et al.
  (CCS 2008) show this; TUF (CCS 2010) and Uptane (2016) answer it with signed,
  expiring, versioned metadata.
  ([TUF paper](https://dl.acm.org/doi/pdf/10.1145/1866307.1866315),
  [Uptane](https://uptane.org/papers/kuppusamy_escar_16.pdf))
- **Generations do not cover state.** NixOS (ICFP 2008, JFP 2010) names the
  limit itself: mutable state outside the store is not rolled back.
  ([paper](https://edolstra.github.io/pubs/nixos-jfp-final.pdf))
  Reproducibility in nixpkgs rose to 91% bit for bit (MSR 2025), so nodes that
  compile from source can still differ.
  ([paper](https://arxiv.org/abs/2501.15919))
- **Cheaper host updates.** VM-preserving host updates at Azure (EuroSys 2021)
  keep guest memory across a kexec, with a blackout of seconds.
  ([paper](https://doi.org/10.1145/3447786.3456232))

## The plan

### 1. Release lines follow NixOS stable releases

- Each kcore minor line is built on one NixOS stable branch, and
  `flake.nix` follows that branch, not `nixos-unstable`.
- **The first line is on `nixos-26.11`.** Releases stay on the current
  snapshot until 26.11 ships (late November 2026). The first base release then
  moves `flake.nix` to `nixos-26.11`. There is no interim move to
  `nixos-26.05`.
- A patch release in a line may move nixpkgs only within its branch. That
  brings backported security fixes and keeps NixOS module options stable.
- **A base release** is the only kind that crosses NixOS releases. It
  changes nothing else in kcore, so a failure points at the base. It is a
  barrier in the update graph (section 2).
- Ceph moves into `flake.lock` as a named input (`nixpkgs-ceph`) until the base
  pin evaluates its own Ceph. Then it is removed. Either way `lib.kcore`
  reports the Ceph version, and a Ceph major version change is a base
  release.
- NixOS supports a stable branch for about seven months, which leaves about
  one month of overlap between branches. Each later base release (27.05,
  27.11) is planned for that month.
- **A longer-lived line** (Cyberus Linux 26.05 until 2031, or a kcore LTS line)
  is decided later. It would belong to the subscriber channel, because it
  needs a binary cache, which the Community release must not configure.

### 2. An update graph in the signed release metadata

`lib.kcore` and `kcore-X.Y.Z-release.json` gain:

| Field | Meaning |
|---|---|
| `nixosRelease` | `26.05`, `26.11`, and so on. |
| `cephRelease` | `squid`, `tentacle`, and so on. |
| `upgradeFrom` | The oldest version that may update straight to this one, by default two minor versions back (section 5). Older nodes need an intermediate release. |
| `barrier` | Every cluster must stop here on the way past. Set on base releases. |
| `downgradeTo` | The oldest version this one can go back to. Absent after a one-way step. |
| `blocked` | Set in a later signed release to retire a bad version; `SetClusterVersion` refuses it. |
| `stateChanges` | Which one-way steps the release can run (section 4). |

`SetClusterVersion` computes the path from the cluster's version to the
target. It then refuses, or offers the chain (for example v0.7.4, then v0.8.0,
then v0.8.2). `kctl update version set --dry-run` prints that path.

TUF-style protection is a later step. The metadata gains an expiry and a
monotonic version, and the controller refuses an older or expired document.
The Sigstore signature over `SHA256SUMS` already covers the file today.

### 3. A pre-flight check you can run at any time

`kctl update precheck vX.Y.Z` changes nothing and runs the same checks that
`SetClusterVersion` runs before starting:

- The update graph allows the path.
- The target evaluates for every node's `node.nix`, which catches removed
  NixOS options and packages that `stateVersion` depends on.
- The closure diff for each node: kernel, systemd, Cloud Hypervisor, and Ceph
  versions, and whether the node needs a reboot.
- Free space in `/nix` and `/boot`, and enough generations kept to roll back.
- Ceph is `HEALTH_OK`, and `ceph osd ok-to-stop` passes for each node's OSDs.
- Controller replication is healthy and there is a quorum.
- Each VM is checked for live migration (passthrough, vGPU, local disks), and
  receiving nodes have headroom for it. VMs that cannot move are listed, with
  what will happen to them.

### 4. One-way steps are explicit and come last

Some changes cannot be undone by any rollback:

- Ceph `require-osd-release` and on-disk format changes;
- controller schema steps marked `irreversible`;
- data migrations such as a PostgreSQL major version.

`system.stateVersion` stays at its install value. Never bumping it is the rule.

A release lists these steps under `stateChanges`. The rollout runs them in a
separate **finalize** phase:

- It starts only after every node is confirmed on the new version and the
  cluster has soaked.
- It needs explicit operator approval.
- `kctl update version get` shows "rolled out, not finalized" until then.

Before finalize, setting the previous tag is a supported downgrade. After it,
the update graph drops the downgrade edge.

Two existing pieces are the start of this. The controller schema's
`irreversible` flag is the first example, and the expand-then-contract style
of migration is the pattern.

### 5. Version skew is written down and tested

- **The window is n-2, as in Cloud Hypervisor.** A controller on minor
  version N works with agents on N, N-1, and N-2. A cluster can update
  straight from N-2 to N unless a barrier release lies between them.
- The controller updates first. Agents are never ahead of the controller.
- Controller replication between HA peers on N and N-2 is part of the same
  window, because a rollout leaves followers and the leader on different
  versions for a while.
- Cloud Hypervisor live migration goes from older to newer, within its own
  n-2 promise. kcore never migrates a VM from a newer node to an older one.
- Proto changes are checked in CI with `buf breaking` against `main`.
- A NixOS VM test boots three nodes on release N-2, and another on N-1, and
  updates them to N one by one, so every mixed state in the window is
  exercised. It runs on persisted
  controller state, and it covers a node joining on the new version. The SOSP
  study says this one test would catch most upgrade bugs.

### 6. Ordering and gates know about Ceph and HA

- **Order:** controller followers, then the leader, then Ceph mon and mgr
  hosts, then OSD-only hosts.
- **Before each node:** `ok-to-stop` and `noout`.
- **After each node:** wait for `HEALTH_OK` and for placement groups to
  return to active+clean, the way Nutanix waits for data resiliency, before
  moving to the next node.
- **VMs that cannot migrate:** stopped and restored on their node after it
  confirms, but only when the operator opts in. Otherwise the precheck
  blocks.

### 7. Canary first, then one node at a time

Every rollout starts with a canary:

1. **The controllers judge the canary from the old version.** The canary is
   the first node in the rollout order that does not run a controller; only
   a cluster where every node runs one uses a controller. Whatever order
   section 6 settles on applies to the nodes after the canary. `--canary` or
   a node label (`kcore.io/update-canary`) picks a specific worker.
2. **The canary soaks.** The default is 24 hours for a base release and one
   hour for a patch release. `--soak` changes it, and
   `kctl update cluster promote` ends it early.
3. **The canary is compared with its peers** during the soak (below). A FAIL
   rolls the canary back and stops. A NONE pauses for the operator.
4. **The rest of the nodes then update one at a time.** `maxUnavailable`
   stays 1. `batch` and `per-dc` stay refused.

The comparison covers heartbeats, the running version, guest state, the
agent's failed controller calls, and Ceph OSD commit latency (section 8). A
canary that rebooted into the update is rebooted into the previous
generation after a FAIL.

### 8. Health means "no worse than the peers"

- The current per-node health check (units active for 20 seconds, at most two
  restarts) stays as the gate for confirming a node.
- After confirmation, the controller compares the updated node with nodes
  still on the old version: VM start success, agent RPC error rate, Ceph OSD
  latency. A worse node pauses the rollout. When there is no clear answer,
  the rollout also pauses.
- For seven days after a rollout, new alerts on the updated nodes are linked
  to the release in `kctl update version get`. An operator can mark the
  release bad, which sets `blocked` for this cluster.

### 9. Windows, holds, and disruption tiers

- **Build outside the window, reboot inside it.** `kctl update window set`
  gives a weekly reboot window. Prepare and switch run at any time; boot
  activations wait for the window. `kctl update hold --until` defers updates.
- **Pick the cheapest tier the closure diff allows.** Restart the agent only;
  switch with VMs left running (today's switch path); or reboot. A later
  research item is kexec with Cloud Hypervisor snapshot and restore into RAM,
  following the Azure paper.

### 10. Security updates have a clock

- Patch releases in a line are cut when the stable branch carries a security
  fix that touches the shipped closure. The release SBOM decides that.
- An urgent CVE can ship as a small overlay in the kcore flake, removed once
  the stable branch has the fix, as Cyberus does.
- Each release publishes the CVEs it fixes and which ones are open against
  its SBOM. The controller shows when the running version is affected.

## Sequencing

The order below puts the cheapest risk reduction first:

1. **Measure:**
   - the mixed-version VM test;
   - `buf breaking` in CI;
   - a hardware test of a nixpkgs bump on the Dell, including the failover
     case.
2. **Make the base visible:**
   - move Ceph's nixpkgs into `flake.lock`;
   - add `nixosRelease` and `cephRelease` to `lib.kcore`;
   - show both in `kctl update version get`.
3. **Move to a stable branch:** the first base release switches `flake.nix`
   to `nixos-26.11` once it ships. Steps 1 and 2 land before it, so that
   release is the first one tested by them.
4. **Precheck:** `kctl update precheck` and the Ceph gates.
5. **Update graph:** `upgradeFrom`, `barrier`, `downgradeTo`, and `blocked` in
   the release metadata, enforced by `SetClusterVersion`.
6. **Finalize phase** for the steps in `stateChanges`.
7. **Canary with soak and peer comparison, windows, and holds.**
8. **TUF-style metadata freshness and the CVE feed.**

## Decisions

Decided on 2026-11-04:

- **NixOS branch:** wait for `nixos-26.11` and make it the first base
  release.
- **Canary:** every rollout starts with a canary and a soak (section 7).
- **Skew window:** n-2, as in Cloud Hypervisor (section 5).

Open:

- **Long-term line:** whether the later private channel offers a long-lived
  line, built on Cyberus Linux or maintained by kcore. Decided later.
