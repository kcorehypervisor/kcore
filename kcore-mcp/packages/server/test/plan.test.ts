import assert from "node:assert/strict";
import test from "node:test";
import { findKind } from "../src/catalog.js";
import { InputError } from "../src/names.js";
import { buildApply, buildOperation } from "../src/requests.js";
import { diffSpec } from "../src/plan.js";

const sha = "ab".repeat(32);

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
