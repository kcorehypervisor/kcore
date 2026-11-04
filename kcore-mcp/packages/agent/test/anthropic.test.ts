import assert from "node:assert/strict";
import test from "node:test";
import { Anthropic, toAnthropicMessages } from "../src/providers/anthropic.js";

function sse(events: unknown[]): Response {
  return new Response(events.map((e) => `event: x\ndata: ${JSON.stringify(e)}\n\n`).join(""), { headers: { "content-type": "text/event-stream" } });
}

test("streams text and a tool call, and sends the Messages API shape", async () => {
  let sent: Record<string, unknown> = {};
  let headers: Record<string, string> = {};
  const provider = new Anthropic({
    apiKey: "sk-ant",
    fetch: async (_url, init) => {
      sent = JSON.parse(String(init?.body));
      headers = init?.headers as Record<string, string>;
      return sse([
        { type: "message_start", message: { usage: { input_tokens: 11 } } },
        { type: "content_block_start", index: 0, content_block: { type: "text" } },
        { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "Hi" } },
        { type: "content_block_stop", index: 0 },
        { type: "content_block_start", index: 1, content_block: { type: "tool_use", id: "tu1", name: "kcore_read" } },
        { type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: '{"a"' } },
        { type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: ":1}" } },
        { type: "content_block_stop", index: 1 },
        { type: "message_delta", usage: { output_tokens: 7 } },
        { type: "message_stop" },
      ]);
    },
  });
  const events = [];
  for await (const e of provider.stream({
    model: "claude-x",
    messages: [{ role: "system", content: "sys" }, { role: "user", content: "hello" }],
    tools: [{ name: "kcore_read", description: "d", parameters: { type: "object", properties: {} } }],
  })) {
    events.push(e);
  }
  assert.equal(headers["x-api-key"], "sk-ant");
  assert.equal(headers["anthropic-version"], "2023-06-01");
  assert.equal(sent.system, "sys");
  assert.equal(sent.stream, true);
  assert.ok(Number(sent.max_tokens) > 0);
  assert.deepEqual((sent.tools as Array<Record<string, unknown>>)[0].input_schema, { type: "object", properties: {} });
  assert.deepEqual(events, [
    { type: "text", delta: "Hi" },
    { type: "tool_call", call: { id: "tu1", name: "kcore_read", arguments: '{"a":1}' } },
    { type: "done", usage: { inputTokens: 11, outputTokens: 7 } },
  ]);
});

test("tool results from one turn are grouped into a single user message", () => {
  const out = toAnthropicMessages([
    { role: "user", content: "go" },
    {
      role: "assistant",
      content: "ok",
      tool_calls: [
        { id: "a", type: "function", function: { name: "t", arguments: '{"x":1}' } },
        { id: "b", type: "function", function: { name: "t", arguments: "not json" } },
      ],
    },
    { role: "tool", tool_call_id: "a", content: "one" },
    { role: "tool", tool_call_id: "b", content: "two" },
  ]);
  assert.equal(out.length, 3);
  assert.deepEqual(out[1].content, [
    { type: "text", text: "ok" },
    { type: "tool_use", id: "a", name: "t", input: { x: 1 } },
    { type: "tool_use", id: "b", name: "t", input: {} },
  ]);
  assert.deepEqual(out[2], {
    role: "user",
    content: [
      { type: "tool_result", tool_use_id: "a", content: "one" },
      { type: "tool_result", tool_use_id: "b", content: "two" },
    ],
  });
});

test("an HTTP error surfaces the status", async () => {
  const provider = new Anthropic({ apiKey: "k", fetch: async () => new Response("bad key", { status: 401 }) });
  await assert.rejects(async () => {
    for await (const _ of provider.stream({ model: "m", messages: [{ role: "user", content: "x" }], tools: [] })) void _;
  }, /401/);
});
