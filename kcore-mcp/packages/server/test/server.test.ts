import assert from "node:assert/strict";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { createServer } from "../src/server.js";

test("the server lists tools and refuses to apply a VM that still has open questions", async () => {
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  const server = createServer();
  const client = new Client({ name: "kcore-test", version: "0.0.0" });
  await server.connect(serverTransport);
  await client.connect(clientTransport);

  const tools = await client.listTools();
  const names = tools.tools.map((tool) => tool.name);
  for (const name of ["kcore_advise", "kcore_plan", "kcore_apply", "kcore_read", "kcore_delete", "kcore_operation", "kcore_catalog"]) {
    assert.ok(names.includes(name), name);
  }

  const advice = await client.callTool({ name: "kcore_advise", arguments: { kind: "vm", spec: {} } });
  const adviceText = JSON.stringify(advice);
  assert.match(adviceText, /Ask the operator/);

  const apply = await client.callTool({
    name: "kcore_apply",
    arguments: { kind: "vm", spec: {}, confirm: true },
  });
  const applyText = JSON.stringify(apply);
  assert.match(applyText, /applied\\": false/);
  assert.doesNotMatch(applyText, /kctl was not found/);

  await client.close();
  await server.close();
});
