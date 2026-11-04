import assert from "node:assert/strict";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { ElicitRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import type { Question } from "../src/advise.js";
import { resolveQuestions } from "../src/options.js";
import { createServer } from "../src/server.js";

const base: Question = { id: "network", prompt: "Which network?", why: "Pick one.", required: true };

test("a restricted question becomes a pick list with labels", async () => {
  const [out] = await resolveQuestions([{ ...base, source: "networks", restrict: true }], async () => [
    { value: "lan", label: "lan (10.0.0.1)" },
    { value: "dmz" },
    { value: "lan" },
  ]);
  assert.deepEqual(out.choices, ["lan", "dmz"]);
  assert.deepEqual(out.choiceLabels, ["lan (10.0.0.1)", "dmz"]);
});

test("an unrestricted question keeps free text and lists what exists", async () => {
  const [out] = await resolveQuestions([{ ...base, id: "auth", source: "ssh-keys" }], async () => [{ value: "alice" }, { value: "bob" }]);
  assert.equal(out.choices, undefined);
  assert.match(out.why, /Existing stored ssh keys: alice, bob\./);
});

test("an empty cluster says so, and an unreadable one changes nothing", async () => {
  const [empty] = await resolveQuestions([{ ...base, source: "networks", restrict: true }], async () => []);
  assert.equal(empty.choices, undefined);
  assert.match(empty.why, /No networks exist yet\./);
  const [failed] = await resolveQuestions([{ ...base, source: "networks", restrict: true }], async () => {
    throw new Error("controller down");
  });
  assert.deepEqual(failed, { ...base, source: "networks", restrict: true });
});

test("each source is read once per ask", async () => {
  let reads = 0;
  await resolveQuestions(
    [
      { ...base, source: "networks", restrict: true },
      { ...base, id: "other", source: "networks", restrict: true },
    ],
    async () => {
      reads += 1;
      return [{ value: "a" }];
    },
  );
  assert.equal(reads, 1);
});

test("the VM form the server asks for has a network pick list, a typed vCPU field, and key hints", async () => {
  const seen: Array<{ message: string; requestedSchema: { properties: Record<string, Record<string, unknown>> } }> = [];
  const server = createServer({
    loadOptions: async (_connection, source) => {
      if (source === "networks") return [{ value: "lan", label: "lan (10.0.0.1)" }, { value: "dmz" }];
      if (source === "ssh-keys") return [{ value: "alice" }];
      return [];
    },
  });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: "t", version: "0" }, { capabilities: { elicitation: {} } });
  client.setRequestHandler(ElicitRequestSchema, async (request) => {
    seen.push(request.params as never);
    return { action: "cancel" as const };
  });
  await server.connect(serverTransport);
  await client.connect(clientTransport);
  await client.callTool({ name: "kcore_plan", arguments: { kind: "vm", acceptDefaults: false, spec: {} } });
  const properties = seen[0].requestedSchema.properties;
  assert.deepEqual(properties.network.enum, ["lan", "dmz"]);
  assert.deepEqual(properties.network.enumNames, ["lan (10.0.0.1)", "dmz"]);
  assert.match(String(properties.auth.description), /Existing stored ssh keys: alice\./);
  assert.equal(properties.auth.enum, undefined);
  await client.close();
  await server.close();
});

test("integer questions go out as integer fields", async () => {
  const seen: Array<{ requestedSchema: { properties: Record<string, Record<string, unknown>> } }> = [];
  const server = createServer({ loadOptions: async () => [] });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: "t", version: "0" }, { capabilities: { elicitation: {} } });
  client.setRequestHandler(ElicitRequestSchema, async (request) => {
    seen.push(request.params as never);
    return { action: "cancel" as const };
  });
  await server.connect(serverTransport);
  await client.connect(clientTransport);
  await client.callTool({ name: "kcore_plan", arguments: { kind: "snapshot-policy", spec: { name: "p", target: "vm", vm: "a", schedule: "@daily" } } });
  const keep = seen[0].requestedSchema.properties.keep;
  assert.equal(keep.type, "integer");
  assert.equal(keep.minimum, 1);
  await client.close();
  await server.close();
});
