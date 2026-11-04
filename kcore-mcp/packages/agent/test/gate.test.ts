import assert from "node:assert/strict";
import test from "node:test";
import { gate, modelFacingTool } from "../src/gate.js";
import { isApproval, validateForm } from "../src/forms.js";

test("the model cannot pick the controller or TLS settings", () => {
  const out = gate("kcore_read", { kind: "vm", connection: { controller: "evil:9090", insecure: true, config: "/tmp/x", node: "10.0.0.5:9091" } });
  assert.deepEqual(out.arguments.connection, { node: "10.0.0.5:9091" });
  const none = gate("kcore_read", { kind: "vm", connection: { controller: "evil:9090" } });
  assert.equal("connection" in none.arguments, false);
});

test("apply, delete and operation always go to the server with confirm false", () => {
  for (const name of ["kcore_apply", "kcore_delete", "kcore_operation"]) {
    const out = gate(name, { confirm: true, kind: "vm", name: "a", action: "set-vm-state", spec: {} });
    assert.equal(out.arguments.confirm, false, name);
    assert.equal(out.hostApproval, undefined, name);
  }
});

test("node install and mutating RPCs need a host approval, reads do not", () => {
  const install = gate("kcore_operation", { action: "install-node", spec: { acknowledge: "wipe the selected disks", disks: ["/dev/sda"] }, confirm: true });
  assert.ok(install.hostApproval);
  assert.equal(install.arguments.confirm, false);
  assert.equal(install.approvedArguments.confirm, true);
  assert.equal(JSON.stringify(install.hostApproval?.detail).includes("wipe the selected disks"), false);

  const write = gate("kcore_rpc", { service: "controller", method: "createVm", request: {}, confirm: true });
  assert.ok(write.hostApproval);
  assert.equal(write.arguments.confirm, false);

  const read = gate("kcore_rpc", { service: "controller", method: "listNodes", confirm: true });
  assert.equal(read.hostApproval, undefined);
  assert.equal("confirm" in read.arguments, false);
});

test("tools shown to the model hide confirm and connection overrides", () => {
  const tool = modelFacingTool({
    name: "kcore_apply",
    inputSchema: {
      type: "object",
      properties: { confirm: { type: "boolean" }, connection: { type: "object", properties: { controller: { type: "string" }, node: { type: "string" } } }, kind: { type: "string" } },
      required: ["confirm", "kind"],
    },
  });
  const params = tool.parameters as { properties: Record<string, { properties?: Record<string, unknown> }>; required: string[] };
  assert.equal("confirm" in params.properties, false);
  assert.deepEqual(Object.keys(params.properties.connection.properties ?? {}), ["node"]);
  assert.deepEqual(params.required, ["kind"]);
});

test("form validation enforces required, enum, and number rules", () => {
  const schema = {
    type: "object",
    properties: { name: { type: "string", title: "Name" }, size: { type: "number", minimum: 1 }, net: { type: "string", enum: ["a", "b"] } },
    required: ["name"],
  };
  assert.equal(validateForm(schema, {}).ok, false);
  assert.equal(validateForm(schema, { name: "x", net: "c" }).ok, false);
  assert.equal(validateForm(schema, { name: "x", size: 0 }).ok, false);
  assert.equal(validateForm(schema, { name: "x", extra: 1 }).ok, false);
  const ok = validateForm(schema, { name: "x", size: "4", net: "b" });
  assert.deepEqual(ok, { ok: true, content: { name: "x", size: 4, net: "b" } });
  assert.equal(isApproval({ properties: { confirm: { enum: ["yes", "no"] } } }), true);
  assert.equal(isApproval({ properties: { confirm: { enum: ["yes", "no"] }, other: {} } }), false);
});

test("labelled choices validate against their values", () => {
  const schema = { properties: { net: { type: "string", enum: ["lan", "dmz"], enumNames: ["lan (10.0.0.1)", "dmz"] }, node: { type: "string", oneOf: [{ const: "n1", title: "n1 (host-a)" }] } } };
  assert.equal(validateForm(schema, { net: "lan" }).ok, true);
  assert.equal(validateForm(schema, { net: "lan (10.0.0.1)" }).ok, false);
  assert.equal(validateForm(schema, { node: "n1" }).ok, true);
  assert.equal(validateForm(schema, { node: "n2" }).ok, false);
});
