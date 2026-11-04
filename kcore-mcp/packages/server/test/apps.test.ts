import assert from "node:assert/strict";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { MCP_APP_MIME, PLAN_APP_HTML, PLAN_APP_URI } from "../src/apps.js";
import { createServer } from "../src/server.js";

async function connect() {
  const server = createServer({ loadOptions: async () => [] });
  const [a, b] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: "t", version: "0" }, { capabilities: {} });
  await server.connect(b);
  await client.connect(a);
  return { client, close: async () => { await client.close(); await server.close(); } };
}

test("kcore_plan points at the plan app and the app can be read", async () => {
  const { client, close } = await connect();
  const tools = await client.listTools();
  const plan = tools.tools.find((t) => t.name === "kcore_plan");
  assert.deepEqual((plan?._meta as { ui?: { resourceUri?: string } }).ui, { resourceUri: PLAN_APP_URI });
  for (const tool of tools.tools.filter((t) => t.name !== "kcore_plan")) assert.equal((tool._meta as { ui?: unknown } | undefined)?.ui, undefined, tool.name);

  const resources = await client.listResources();
  assert.ok(resources.resources.some((r) => r.uri === PLAN_APP_URI && r.mimeType === MCP_APP_MIME));
  const read = await client.readResource({ uri: PLAN_APP_URI });
  assert.equal(read.contents[0].mimeType, MCP_APP_MIME);
  assert.equal((read.contents[0] as { text: string }).text, PLAN_APP_HTML);
  await close();
});

test("the plan app talks only to its host and loads nothing", () => {
  assert.doesNotMatch(PLAN_APP_HTML, /https?:\/\//);
  assert.doesNotMatch(PLAN_APP_HTML, /<script[^>]*\ssrc=|<link|<img|<iframe|fetch\(|XMLHttpRequest|WebSocket|innerHTML|eval\(/);
  assert.match(PLAN_APP_HTML, /ui\/initialize/);
  assert.match(PLAN_APP_HTML, /ui\/notifications\/initialized/);
  assert.match(PLAN_APP_HTML, /2026-01-26/);
});
