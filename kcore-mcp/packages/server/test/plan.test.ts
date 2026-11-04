import assert from "node:assert/strict";
import test from "node:test";
import { findKind } from "../src/catalog.js";
import { InputError } from "../src/names.js";
import { buildApply, buildDelete, buildOperation, buildPlanCall, buildRead, soakSeconds } from "../src/requests.js";
import { diffSpec } from "../src/plan.js";

const sha = "ab".repeat(32);

test("a PostgreSQL apply is a CreatePostgresql RPC", () => {
  const kind = findKind("postgresql");
  assert.ok(kind);
  const [call] = buildApply(kind, { name: "app", package: "postgresql_16" });
  assert.equal(call.method, "createPostgresql");
  assert.equal(call.request.database, "app");
  assert.equal(call.request.package, "postgresql_16");
  assert.equal(call.request.port, 5432);
});

test("a VM apply is a CreateVm RPC", () => {
  const kind = findKind("vm");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    name: "web-01",
    imageUrl: "https://example.com/debian.qcow2",
    imageSha256: sha,
    network: "default",
    storageBackend: "zfs",
    storageSizeBytes: "40G",
    cpu: "2",
    memoryBytes: "4G",
    sshKeys: ["deploy"],
  });
  assert.equal(call.method, "createVm");
  assert.equal(call.request.storageBackend, "STORAGE_BACKEND_TYPE_ZFS");
  assert.equal(call.request.imageUrl, "https://example.com/debian.qcow2");
  const vm = call.request.spec as { name: string; memoryBytes: string };
  assert.equal(vm.name, "web-01");
  assert.equal(vm.memoryBytes, String(4 * 1024 ** 3));
});

test("a URL image without https is refused", () => {
  const kind = findKind("vm");
  assert.ok(kind);
  assert.throws(
    () =>
      buildApply(kind, {
        name: "web-01",
        imageUrl: "http://example.com/debian.qcow2",
        imageSha256: sha,
        network: "default",
        storageBackend: "filesystem",
        storageSizeBytes: "40G",
        sshKeys: ["deploy"],
      }),
    InputError,
  );
});

test("unsafe resource names are refused", () => {
  const kind = findKind("network");
  assert.ok(kind);
  assert.throws(
    () =>
      buildApply(kind, {
        name: "bad name",
        type: "nat",
        externalIp: "203.0.113.10",
        gatewayIp: "10.0.0.1",
      }),
    InputError,
  );
});

test("volume create is a CreateVolume RPC", () => {
  const kind = findKind("volume");
  assert.ok(kind);
  const [call] = buildApply(kind, { name: "data", sizeBytes: "10737418240", encrypt: "yes" });
  assert.equal(call.method, "createVolume");
  assert.equal(call.request.encrypt, true);
  assert.equal(call.request.sizeBytes, "10737418240");
});

test("node install names the disk and asks the controller for a bootstrap certificate", () => {
  const call = buildOperation("install-node", {
    node: "10.0.0.21:9091",
    osDisk: "/dev/sda",
    joinController: "10.0.0.10:9090",
    dataDisk: "/dev/nvme0n1",
  });
  assert.equal(call.target, "node");
  assert.equal(call.method, "installToDisk");
  assert.equal(call.address, "10.0.0.21:9091");
  assert.equal(call.request.osDisk, "/dev/sda");
  assert.deepEqual(call.request.dataDisks, ["/dev/nvme0n1"]);
  assert.equal(call.request.controller, "10.0.0.10:9090");
});

test("mutable VM fields update and immutable fields replace", () => {
  const kind = findKind("vm");
  assert.ok(kind);
  const current = { cpu: 2, memoryBytes: "2G", network: "default", storageBackend: "filesystem" };
  const update = diffSpec(kind, { ...current, cpu: 4 }, current);
  assert.equal(update.action, "update");
  const replace = diffSpec(kind, { ...current, network: "other" }, current);
  assert.equal(replace.action, "replace");
});

test("a VM apply carries vGPU slices and PCI devices", () => {
  const kind = findKind("vm");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    name: "vgpu-01",
    imageUrl: "https://example.com/ubuntu.img",
    imageSha256: sha,
    network: "default",
    storageBackend: "filesystem",
    storageSizeBytes: "40G",
    sshKeys: ["deploy"],
    vgpus: ["A2-4C"],
    pciDevices: ["0000:61:00.1"],
  });
  const vm = call.request.spec as { vgpus: string[]; pciDevices: string[] };
  assert.deepEqual(vm.vgpus, ["A2-4C"]);
  assert.deepEqual(vm.pciDevices, ["0000:61:00.1"]);
});

test("a vGPU host apply stages the manager", () => {
  const kind = findKind("node-vgpu");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    nodeId: "node-193",
    hostCards: ["0000:61:00.0"],
    managerUrl: "http://192.168.40.10:18083/virtual-gpu-manager-8_v8.2/NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run",
    managerSha256: sha,
    managerFile: "NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run",
  });
  assert.equal(call.method, "configureNodeVgpu");
  assert.equal(call.request.enabled, true);
  assert.equal(call.request.vendor, "nvidia");
  const [amd] = buildApply(kind, { nodeId: "node-7", vendor: "AMD", hostCards: ["0000:c3:00.0"], numVfs: 4 });
  assert.equal(amd.request.vendor, "amd");
  assert.equal(amd.request.manager, undefined);
  assert.throws(() => buildApply(kind, { nodeId: "node-7", vendor: "intel", hostCards: ["0000:03:00.0"] }), /numVfs/);
  const [mixed] = buildApply(kind, { nodeId: "node-7", vendor: "amd", hostCards: ["0000:c3:00.0=4", "0000:c4:00.0=6"] });
  assert.deepEqual(mixed.request.hostCards, ["0000:c3:00.0=4", "0000:c4:00.0=6"]);
  assert.throws(() => buildApply(kind, { nodeId: "node-7", vendor: "amd", hostCards: ["0000:c3:00.0=4", "0000:c4:00.0"] }), /numVfs/);
  const migrate = buildOperation("migrate-vm", { name: "vgpu-01", targetNode: "node-193", vgpus: ["A2-4C"] });
  assert.deepEqual(migrate.request.vgpus, ["A2-4C"]);
  const reboot = buildOperation("reboot-node", { nodeId: "node-193" });
  assert.equal(reboot.method, "rebootNode");
});

test("a cluster update is a CreateClusterUpdate RPC", () => {
  const kind = findKind("cluster-update");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    name: "rel",
    version: "0.3.0",
    flakeRef: "github:kcorehypervisor/kcore/v0.3.0",
  });
  assert.equal(call.method, "createClusterUpdate");
});

test("a SlurmCluster apply is a CreateSlurmCluster RPC", () => {
  const kind = findKind("slurm-cluster");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    name: "hpc-a",
    controller: { nodeId: "node-1", address: "10.0.0.1", iface: "enp6s0" },
    compute: [
      { nodeId: "node-2", address: "10.0.0.2", iface: "enp6s0", partitions: ["batch"], gres: "none" },
    ],
    partitions: [{ name: "batch", default: true }],
  });
  assert.equal(call.method, "createSlurmCluster");
  assert.equal((call.request as { name: string }).name, "hpc-a");
});

test("a SlurmCluster plan refuses missing answers", () => {
  const kind = findKind("slurm-cluster");
  assert.ok(kind);
  assert.throws(() => buildApply(kind, { name: "hpc-a" }), /controller is required/);
});

test("a complete SlurmCluster spec is accepted", () => {
  const kind = findKind("slurm-cluster");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    name: "hpc-a",
    controller: { nodeId: "node-1", address: "10.0.0.1", iface: "enp6s0" },
    compute: [
      { nodeId: "node-2", address: "10.0.0.2", iface: "enp6s0", partitions: ["batch"], gres: "none" },
    ],
    partitions: [{ name: "batch", default: true }],
    users: [{ name: "alice", uid: 2001, sshKeys: ["ssh-ed25519 AAAA"] }],
    storage: { mode: "nfs", exports: ["/home", "/scratch"] },
  });
  const spec = (call.request as { spec: Record<string, unknown> }).spec;
  assert.equal(spec.clusterName, "hpc-a");
  assert.equal((spec.compute as unknown[]).length, 1);
});

test("Slurm day-2 operations map to their RPCs", () => {
  const drain = buildOperation("slurm-drain-node", { name: "hpc-a", nodeId: "node-108", reason: "maintenance" });
  assert.equal(drain.method, "drainSlurmNode");
  assert.deepEqual(drain.request, { name: "hpc-a", nodeId: "node-108", reason: "maintenance" });
  const resume = buildOperation("slurm-resume-node", { name: "hpc-a", nodeId: "node-108" });
  assert.equal(resume.method, "resumeSlurmNode");
  const rotate = buildOperation("slurm-rotate-munge-key", { name: "hpc-a" });
  assert.equal(rotate.method, "rotateSlurmMungeKey");
  assert.throws(() => buildOperation("slurm-drain-node", { name: "hpc-a" }), /node id is required/);
  const kind = findKind("slurm-cluster");
  assert.ok(kind);
  assert.equal(buildDelete(kind, "hpc-a").method, "deleteSlurmCluster");
});

import { serviceClient } from "../src/grpc.js";
import type { RpcCall } from "../src/requests.js";

type MethodDefinition = {
  requestSerialize: (value: unknown) => Buffer;
  requestDeserialize: (value: Buffer) => Record<string, unknown>;
};

function wire(call: RpcCall): Record<string, unknown> {
  const service = serviceClient("controller").service as unknown as Record<string, MethodDefinition>;
  const definition = service[call.method[0].toUpperCase() + call.method.slice(1)];
  assert.ok(definition, `controller has no ${call.method}`);
  return definition.requestDeserialize(definition.requestSerialize(call.request));
}

test("a Kubernetes cluster apply carries the spec the controller reads", () => {
  const kind = findKind("kubernetes-cluster");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    name: "prod",
    clusterCidr: "10.244.0.0/16",
    serviceCidr: "10.96.0.0/12",
    apiServerEndpoint: "192.168.40.200:6443",
    controlPlaneNodes: [
      { nodeId: "node-105", advertiseAddress: "192.168.40.105", iface: "enp6s0", initialCluster: true },
      { nodeId: "node-107", advertiseAddress: "192.168.40.107" },
    ],
    cilium: { loadBalancerIpPools: ["192.168.40.220/29"] },
    highAvailability: { enabled: true, vipAddress: "192.168.40.200", vipInterface: "enp6s0" },
  });
  assert.equal(call.method, "createKubernetesCluster");
  const decoded = wire(call) as { name: string; confirm: boolean; spec: Record<string, any> };
  assert.equal(decoded.name, "prod");
  assert.equal(decoded.confirm, true);
  assert.equal(decoded.spec.clusterDns, "10.96.0.10");
  assert.equal(decoded.spec.cni, "cilium");
  assert.equal(decoded.spec.controlPlaneNodes.length, 2);
  assert.equal(decoded.spec.controlPlaneNodes[0].initialCluster, true);
  assert.equal(decoded.spec.controlPlaneNodes[1].joinExisting, true);
  assert.equal(decoded.spec.controlPlaneNodes[1].role, "control-plane");
  assert.deepEqual(decoded.spec.ciliumConfig.loadBalancerIpPools, ["192.168.40.220/29"]);
  assert.equal(decoded.spec.highAvailability.vipAddress, "192.168.40.200");
  assert.equal(buildPlanCall(kind, { name: "prod", clusterCidr: "a", serviceCidr: "b", apiServerEndpoint: "c:1", controlPlaneNodes: [{ nodeId: "n", advertiseAddress: "1.2.3.4" }] }).method, "planKubernetesCluster");
  assert.throws(() => buildApply(kind, { name: "prod", clusterCidr: "a", serviceCidr: "b", apiServerEndpoint: "c:1" }), /controlPlaneNodes/);
  assert.deepEqual(buildDelete(kind, "prod").request, { name: "prod", confirm: true, force: false });
});

test("object users are created through kctl, and deleted here", () => {
  const kind = findKind("object-user");
  assert.ok(kind);
  assert.throws(() => buildApply(kind, { name: "app", store: "s3" }), /kctl create object-user/);
  const call = buildOperation("delete-object-user", { name: "app", store: "s3" });
  assert.equal(call.method, "deleteObjectUser");
  assert.deepEqual(wire(call), { name: "app", store: "s3" });
});

test("an operator apply creates it and grants each role", () => {
  const kind = findKind("operator");
  assert.ok(kind);
  const calls = buildApply(kind, { name: "alice", roles: ["vm-admin", "read-only"] });
  assert.deepEqual(calls.map((call) => call.method), ["createOperator", "grantOperatorRole", "grantOperatorRole"]);
  assert.equal(wire(calls[1]).role, "OPERATOR_ROLE_KIND_VM_ADMIN");
  assert.throws(() => buildApply(kind, { name: "alice", roles: ["root"] }), /role must be/);
  assert.equal(buildOperation("revoke-operator-role", { name: "alice", role: "vm-admin" }).method, "revokeOperatorRole");
});

test("nodes are readable and deletable but not appliable", () => {
  const kind = findKind("node");
  assert.ok(kind);
  assert.equal(buildRead(kind).method, "listNodes");
  assert.deepEqual(buildRead(kind, "node-1").request, { nodeId: "node-1" });
  assert.equal(buildDelete(kind, "node-1").method, "deleteNode");
  assert.throws(() => buildApply(kind, { name: "node-1" }), /read-only/);
});

test("volume operations map to their RPCs and keep the destructive guards", () => {
  const resize = buildOperation("resize-volume", { name: "data", sizeBytes: "20G", growFilesystem: true });
  assert.equal(resize.method, "resizeVolume");
  const decoded = wire(resize);
  assert.equal(decoded.sizeBytes, String(20 * 1024 ** 3));
  assert.equal(decoded.growFilesystem, true);
  assert.equal(decoded.allowShrink, false);
  assert.deepEqual(wire(buildOperation("attach-volume", { name: "data", vm: "web" })), { name: "data", vm: "web" });
  assert.equal(buildOperation("detach-volume", { name: "data" }).method, "detachVolume");
  assert.deepEqual(wire(buildOperation("restore-volume", { volume: "data", snapshot: "nightly" })), { volume: "data", snapshot: "nightly" });
  assert.equal(buildOperation("flatten-volume", { name: "data" }).method, "flattenVolume");
  assert.equal(wire(buildOperation("format-volume", { name: "data" })).fsType, "ext4");
  assert.throws(() => buildOperation("format-volume", { name: "data", fsType: "ntfs" }), /ext4 or xfs/);
  assert.equal(wire(buildOperation("mount-volume", { name: "data", mountPoint: "/mnt/kcore/data", persistent: true })).persistent, true);
  assert.throws(() => buildOperation("mount-volume", { name: "data", mountPoint: "/etc" }), /mount point/);
  assert.throws(() => buildOperation("mount-volume", { name: "data", mountPoint: "/mnt/kcore/../../etc" }), /mount point/);
  assert.equal(buildOperation("unmount-volume", { name: "data" }).method, "unmountVolume");
  assert.throws(() => buildOperation("encrypt-volume", { name: "data" }), /destroyData/);
  assert.equal(wire(buildOperation("encrypt-volume", { name: "data", destroyData: true })).destroyData, true);
});

test("certificate rotation, update reboot, and route WAF mode are operations", () => {
  assert.deepEqual(wire(buildOperation("rotate-node-certs", { nodeId: "node-1" })), { nodeId: "node-1", allNodes: false });
  assert.deepEqual(wire(buildOperation("rotate-node-certs", { allNodes: true })), { nodeId: "", allNodes: true });
  assert.throws(() => buildOperation("rotate-node-certs", {}), /nodeId or allNodes/);
  assert.throws(() => buildOperation("rotate-node-certs", { nodeId: "node-1", allNodes: true }), /nodeId or allNodes/);
  assert.deepEqual(wire(buildOperation("reboot-update-node", { name: "rel", nodeId: "node-1" })), { name: "rel", nodeId: "node-1" });
  assert.deepEqual(wire(buildOperation("set-route-waf-mode", { name: "shop", mode: "block" })), { name: "shop", mode: "block" });
  assert.throws(() => buildOperation("set-route-waf-mode", { name: "shop", mode: "strict" }), /detect, block, or off/);
  assert.equal(buildOperation("cancel-vm-operation", { id: "op-1" }).method, "cancelVmOperation");
});

test("a cluster update takes the shape of the release YAML", () => {
  const kind = findKind("cluster-update");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    name: "release-0-3-0",
    target: { version: "0.3.0", flakeRef: "github:kcorehypervisor/kcore", flakeRev: "a".repeat(40), nixpkgsRev: "b".repeat(40) },
    selector: { allNodes: true },
    strategy: { strategyType: "one-at-a-time", maxUnavailable: 1, batchSize: 1 },
    drainVms: false,
    activation: { mode: "auto" },
    approvalPolicy: "manual",
    updateKind: "kcore",
    schemaVersion: 3,
    testedOsPins: [{ nixpkgsRev: "b".repeat(40), cephRev: "c", cephSha256: "d", kubernetesVersion: "1.33" }],
  });
  const decoded = wire(call) as { spec: Record<string, any> };
  assert.equal(decoded.spec.selector.allNodes, true);
  assert.equal(decoded.spec.target.flakeRev, "a".repeat(40));
  assert.equal(decoded.spec.target.nixpkgsRev, "b".repeat(40));
  assert.equal(decoded.spec.strategy.type, "CLUSTER_UPDATE_STRATEGY_ONE_AT_A_TIME");
  assert.equal(decoded.spec.activationMode, "CLUSTER_UPDATE_ACTIVATION_AUTO");
  assert.equal(decoded.spec.approvalPolicy, "CLUSTER_UPDATE_APPROVAL_MANUAL");
  assert.equal(decoded.spec.drainVms, false);
  assert.equal(decoded.spec.updateKind, "kcore");
  assert.equal(decoded.spec.schemaVersion, 3);
  assert.equal(decoded.spec.testedOsPins[0].kubernetesVersion, "1.33");
  assert.throws(() => buildApply(kind, { name: "x", version: "1", flakeRef: "f", strategy: "batch" }), /one-at-a-time or canary/);
  assert.throws(
    () => buildApply(kind, { name: "x", version: "1", flakeRef: "f", strategy: { strategyType: "one-at-a-time", canaryNode: "n" } }),
    /need strategyType canary/,
  );
  assert.throws(() => buildApply(kind, { name: "x", version: "1", flakeRef: "f", approvalPolicy: "yolo" }), /approvalPolicy/);
});

test("a canary cluster update carries its soak and node", () => {
  const kind = findKind("cluster-update");
  assert.ok(kind);
  const [call] = buildApply(kind, {
    name: "release-0-7-0",
    target: { version: "0.7.0", flakeRef: "github:kcorehypervisor/kcore" },
    selector: { allNodes: true },
    strategy: { strategyType: "canary", maxUnavailable: 1, canarySoakSeconds: 1800, canaryNode: "node-b" },
  });
  const decoded = wire(call) as { spec: Record<string, any> };
  assert.equal(decoded.spec.strategy.type, "CLUSTER_UPDATE_STRATEGY_CANARY");
  assert.equal(decoded.spec.strategy.canarySoakSeconds, 1800);
  assert.equal(decoded.spec.strategy.canaryNodeId, "node-b");
});

test("set-version and promote-update reach the release RPCs", () => {
  const set = buildOperation("set-version", { version: "0.7.0", soak: "4h", canaryNode: "node-b", drainVms: true });
  assert.equal(set.method, "setClusterVersion");
  assert.deepEqual(wire(set), {
    version: "v0.7.0",
    flakeRef: "",
    drainVms: true,
    dryRun: false,
    canarySoakSeconds: 14400,
    noCanary: false,
    canaryNodeId: "node-b",
  });
  const plain = buildOperation("set-version", { version: "v0.7.0", noCanary: true, dryRun: true });
  const decoded = wire(plain);
  assert.equal(decoded.version, "v0.7.0");
  assert.equal(decoded.dryRun, true);
  assert.equal(decoded.noCanary, true);
  assert.equal(decoded.canarySoakSeconds, 0);
  assert.throws(() => buildOperation("set-version", { version: "0.7.0", noCanary: true, soak: 60 }), /noCanary/);
  assert.throws(() => buildOperation("set-version", { version: "0.7.0", soak: "8d" }), /between/);
  assert.throws(() => buildOperation("set-version", { version: "0.7.0", soak: "soon" }), /duration/);
  assert.equal(soakSeconds(90), 90);
  assert.equal(soakSeconds("45m"), 2700);
  assert.equal(soakSeconds("1d"), 86400);
  const promote = buildOperation("promote-update", { name: "version-v0-7-0" });
  assert.equal(promote.method, "promoteClusterUpdate");
  assert.equal(wire(promote).name, "version-v0-7-0");
});
