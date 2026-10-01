import assert from "node:assert/strict";
import test from "node:test";
import { advise, inferKind, mergeAnswers } from "../src/advise.js";

const completeVm = {
  name: "web-01",
  imageUrl: "https://example.com/debian.qcow2",
  imageSha256: "a".repeat(64),
  network: "default",
  storageBackend: "filesystem",
  storageSizeBytes: "40G",
  sshPublicKeys: ["ssh-ed25519 AAAA operator@host"],
  cpu: "2",
  memoryBytes: "4G",
  desiredState: "running",
  targetNode: "node-a",
};

test("a blank VM request asks for the decisions that change the machine", () => {
  const advice = advise({ kind: "vm", spec: {} });
  assert.equal(advice.ready, false);
  const ids = advice.blocking.map((item) => item.id);
  assert.ok(ids.includes("name"));
  assert.ok(ids.includes("image"));
  assert.ok(ids.includes("network"));
  assert.ok(ids.includes("storageBackend"));
  assert.ok(ids.includes("storageSizeBytes"));
  assert.ok(ids.includes("auth"));
  assert.match(advice.askTheUser, /Ask the operator/);
});

test("a complete VM spec is ready to plan", () => {
  const advice = advise({ kind: "vm", spec: completeVm });
  assert.equal(advice.ready, true);
  assert.equal(advice.blocking.length, 0);
  assert.equal(advice.nextTool, "kcore_plan");
});

test("an ambiguous request asks which resource", () => {
  const advice = advise({ intent: "make something" });
  assert.equal(advice.kind, null);
  assert.equal(advice.blocking[0]?.id, "kind");
});

test("intent can select a single resource", () => {
  assert.equal(inferKind("create a virtual machine for the web tier"), "vm");
  assert.equal(inferKind("bootstrap the cluster"), "cluster");
});

test("password login is called out before apply", () => {
  const advice = advise({
    kind: "vm",
    spec: { ...completeVm, password: "secret", compliant: true },
  });
  assert.ok(advice.warnings.some((warning) => warning.includes("non-compliant")));
});

test("elicited answers fill image and ssh key fields", () => {
  const spec = mergeAnswers(
    {},
    {
      image: "https://example.com/debian.qcow2",
      auth: "ssh-ed25519 AAAA operator@host",
      memory: "4G",
    },
  );
  assert.equal(spec.imageUrl, "https://example.com/debian.qcow2");
  assert.deepEqual(spec.sshPublicKeys, ["ssh-ed25519 AAAA operator@host"]);
  assert.equal(spec.memoryBytes, "4G");
});

test("a VM manifest answers the image and network questions", () => {
  const advice = advise({
    kind: "vm",
    manifest: `
kind: VM
metadata:
  name: web-01
spec:
  storageBackend: filesystem
  storageSizeBytes: "40G"
  sshKeys: [deploy]
  nics:
    - network: default
  disks:
    - image: https://example.com/debian.qcow2
      sha256: ${"ab".repeat(32)}
      format: qcow2
  cpu: 2
  memoryBytes: "4G"
  desiredState: running
  targetNode: node-a
`,
  });
  assert.equal(advice.ready, true, advice.askTheUser);
});

test("cluster advice asks for the controller address", () => {
  const advice = advise({ kind: "cluster", spec: {} });
  assert.ok(advice.blocking.some((item) => item.id === "controller"));
});
