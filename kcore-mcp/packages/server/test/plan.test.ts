import assert from "node:assert/strict";
import test from "node:test";
import { findKind } from "../src/catalog.js";
import { InputError } from "../src/names.js";
import { installNodeArgs, renderApply } from "../src/manifest.js";
import { diffSpec } from "../src/plan.js";

const sha = "ab".repeat(32);

test("VM manifests match the kctl YAML shape", () => {
  const kind = findKind("vm");
  assert.ok(kind);
  const rendered = renderApply(kind, {
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
  assert.match(rendered.yaml ?? "", /kind: VM/);
  assert.match(rendered.yaml ?? "", /name: web-01/);
  assert.match(rendered.yaml ?? "", /storageBackend: zfs/);
  assert.match(rendered.yaml ?? "", /image: https:\/\/example.com\/debian.qcow2/);
  assert.deepEqual(rendered.args.slice(0, 2), ["apply", "-f"]);
});

test("a URL image without https is refused", () => {
  const kind = findKind("vm");
  assert.ok(kind);
  assert.throws(
    () =>
      renderApply(kind, {
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
      renderApply(kind, {
        name: "bad name",
        type: "nat",
        externalIp: "203.0.113.10",
        gatewayIp: "10.0.0.1",
      }),
    InputError,
  );
});

test("volume create uses kctl flags", () => {
  const kind = findKind("volume");
  assert.ok(kind);
  const rendered = renderApply(kind, { name: "data", sizeBytes: "10737418240", encrypt: "yes" });
  assert.deepEqual(rendered.args, ["create", "volume", "data", "--size-bytes", "10737418240", "--encrypt"]);
});

test("node install args name the disk and the controller", () => {
  const built = installNodeArgs({
    node: "10.0.0.21:9091",
    osDisk: "/dev/sda",
    joinController: "10.0.0.10:9090",
    dataDisk: "/dev/nvme0n1",
  });
  assert.equal(built.node, "10.0.0.21:9091");
  assert.ok(built.args.includes("/dev/sda"));
  assert.ok(built.args.includes("/dev/nvme0n1"));
  assert.ok(built.args.includes("10.0.0.10:9090"));
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

test("cluster update plan uses the update subcommand", () => {
  const kind = findKind("cluster-update");
  assert.ok(kind);
  const rendered = renderApply(kind, { name: "rel" }, "kind: ClusterUpdate\nmetadata:\n  name: rel\nspec: {}\n");
  assert.deepEqual(rendered.args.slice(0, 4), ["update", "cluster", "apply", "-f"]);
});
