import assert from "node:assert/strict";
import test from "node:test";
import { createServer } from "../../server/src/server.js";
import { Session, type AgentEvent } from "../src/session.js";
import { call, scripted, waitFor } from "./support.js";

function newSession() {
  return new Session({ id: "s1", createServer });
}

const find = <T extends AgentEvent["type"]>(session: Session, type: T) =>
  session.history().find((e) => e.type === type) as Extract<AgentEvent, { type: T }> | undefined;

test("a read-only tool call runs and the model sees the result", async () => {
  const session = newSession();
  const provider = scripted((request, turn) =>
    turn === 0 ? [call("c1", "kcore_catalog", {}), { type: "done" }] : [{ type: "text", delta: "Here are the kinds." }, { type: "done" }],
  );
  await session.send("what can you manage?", provider, "m");
  const result = find(session, "tool_result");
  assert.ok(result?.ok);
  assert.match(result.text, /"id": "vm"/);
  assert.equal(find(session, "assistant")?.text, "Here are the kinds.");
  const second = provider.requests[1].messages.at(-1);
  assert.equal(second?.role, "tool");
  assert.ok(provider.requests[0].tools.some((t) => t.name === "kcore_apply"));
  await session.close();
});

test("a model that sets confirm true still gets an approval card, and No changes nothing", async () => {
  const session = newSession();
  const provider = scripted((_request, turn) =>
    turn === 0
      ? [call("c1", "kcore_delete", { kind: "vm", name: "web", confirm: true, connection: { controller: "evil.example:9090", insecure: true } }), { type: "done" }]
      : [{ type: "text", delta: "ok" }, { type: "done" }],
  );
  const done = session.send("delete web", provider, "m");
  const form = await waitFor(() => find(session, "form"));
  assert.equal(form.kind, "approval");
  assert.match(form.message, /Delete/);
  assert.deepEqual(session.answerForm(form.formId, "accept", { confirm: "maybe" }), { ok: false, error: "Delete Virtual machine 'web'? must be one of: yes, no" });
  assert.deepEqual(session.answerForm(form.formId, "accept", { confirm: "no" }), { ok: true });
  await done;
  const result = find(session, "tool_result");
  assert.match(result?.text ?? "", /"deleted": false/);
  const shown = find(session, "tool_call");
  assert.equal(JSON.stringify(shown?.args).includes("evil.example"), false);
  await session.close();
});

test("a rejected host approval never reaches the server", async () => {
  const session = newSession();
  const provider = scripted((_r, turn) =>
    turn === 0 ? [call("c1", "kcore_rpc", { service: "controller", method: "createVm", request: {}, confirm: true }), { type: "done" }] : [{ type: "done" }],
  );
  const done = session.send("make a vm", provider, "m");
  const form = await waitFor(() => find(session, "form"));
  assert.equal(form.kind, "approval");
  session.answerForm(form.formId, "decline", undefined);
  await done;
  assert.match(find(session, "tool_result")?.text ?? "", /did not approve/);
  await session.close();
});

test("open questions become a form and the answers are handed back to the model", async () => {
  const session = newSession();
  const provider = scripted((_r, turn) => (turn === 0 ? [call("c1", "kcore_plan", { kind: "vm", spec: {} }), { type: "done" }] : [{ type: "done" }]));
  const done = session.send("plan a vm", provider, "m");
  const form = await waitFor(() => find(session, "form"));
  assert.equal(form.kind, "form");
  const content: Record<string, string> = {};
  for (const [name, property] of Object.entries(form.schema.properties ?? {})) {
    if ((form.schema.required ?? []).includes(name)) content[name] = property.enum ? String(property.enum[0]) : "answer";
  }
  assert.deepEqual(session.answerForm(form.formId, "accept", content), { ok: true });
  await done;
  const text = find(session, "tool_result")?.text ?? "";
  assert.match(text, /Operator form answers/);
  await session.close();
});

test("cancel releases a pending form and ends the turn", async () => {
  const session = newSession();
  const provider = scripted((_r, turn) => (turn === 0 ? [call("c1", "kcore_delete", { kind: "vm", name: "web" }), { type: "done" }] : [{ type: "done" }]));
  const done = session.send("delete", provider, "m");
  await waitFor(() => find(session, "form"));
  session.cancel();
  await done;
  const end = session.history().filter((e) => e.type === "turn_end").at(-1);
  assert.equal((end as { reason: string }).reason, "cancelled");
  assert.equal(session.pendingForms().length, 0);
  await session.close();
});

test("unknown tools and bad JSON come back as tool errors", async () => {
  const session = newSession();
  const provider = scripted((_r, turn) =>
    turn === 0
      ? [{ type: "tool_call", call: { id: "a", name: "rm_rf", arguments: "{}" } }, { type: "tool_call", call: { id: "b", name: "kcore_catalog", arguments: "{oops" } }, { type: "done" }]
      : [{ type: "done" }],
  );
  await session.send("go", provider, "m");
  const results = session.history().filter((e) => e.type === "tool_result") as Array<{ ok: boolean; text: string }>;
  assert.equal(results.length, 2);
  assert.ok(results.every((r) => !r.ok));
  await session.close();
});
