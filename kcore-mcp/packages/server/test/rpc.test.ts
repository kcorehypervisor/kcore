import assert from "node:assert/strict";
import test from "node:test";
import { InputError } from "../src/names.js";
import { requireRpc, rpcCatalog } from "../src/rpc.js";

test("the RPC catalog includes implemented controller methods and omits stubs and streams", () => {
  const methods = rpcCatalog();
  const names = new Set(methods.map((item) => `${item.service}/${item.method}`));
  assert.equal(names.has("controller/listVms"), true);
  assert.equal(names.has("controller/createVm"), true);
  assert.equal(names.has("controller/listNodes"), true);
  assert.equal(names.has("controller/getPkiStatus"), true);
  assert.equal(names.has("controller-admin/getReplicationStatus"), true);
  assert.equal(names.has("node-compute/listImages"), true);
  assert.equal(names.has("node-admin/listDisks"), true);
  assert.equal(names.has("node-info/getNodeInfo"), true);
  assert.equal(names.has("node-compute/pullImage"), false);
  assert.equal(names.has("node-compute/createVm"), false);
  assert.equal(names.has("controller/attachVmConsole"), false);
  assert.equal(names.has("node-admin/attachVmConsole"), false);
  assert.equal(names.has("node-admin/uploadImageStream"), false);
  const listVms = methods.find((item) => item.service === "controller" && item.method === "listVms");
  assert.equal(listVms?.readOnly, true);
  const createVm = methods.find((item) => item.service === "controller" && item.method === "createVm");
  assert.equal(createVm?.readOnly, false);
});

test("an omitted stub is rejected before any dial", () => {
  assert.throws(() => requireRpc("node-compute", "pullImage"), InputError);
  assert.throws(() => requireRpc("controller", "attachVmConsole"), InputError);
});
