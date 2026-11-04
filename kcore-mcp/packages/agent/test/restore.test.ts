import assert from "node:assert/strict";
import { appendFileSync, mkdtempSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createServer } from "../../server/src/server.js";
import { Session } from "../src/session.js";
import { listTranscripts, readTranscript, repairMessages } from "../src/transcript.js";
import { createApp } from "../src/http.js";
import { AuthStore } from "../src/auth.js";
import { call, scripted } from "./support.js";

const ID = "11111111-2222-3333-4444-555555555555";

test("a session comes back after a restart with its events, title, and model context", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sessions-"));
  const first = new Session({ id: ID, createServer, logDir: dir });
  await first.send(
    "what can you manage?",
    scripted((_r, turn) => (turn === 0 ? [call("c1", "kcore_catalog", {}), { type: "done" }] : [{ type: "text", delta: "Many kinds." }, { type: "done" }])),
    "m",
  );
  const before = first.history();
  await first.close();
  assert.equal(statSync(join(dir, `${ID}.jsonl`)).mode & 0o777, 0o600);
  assert.deepEqual(listTranscripts(dir), [ID]);

  const restored = new Session({ id: ID, createServer, logDir: dir, restore: readTranscript(dir, ID) });
  assert.equal(restored.title, "what can you manage?");
  assert.deepEqual(restored.history(), before);

  const provider = scripted(() => [{ type: "text", delta: "Yes." }, { type: "done" }]);
  await restored.send("and volumes?", provider, "m");
  const sent = provider.requests[0].messages;
  assert.deepEqual(sent.map((m) => m.role), ["system", "user", "assistant", "tool", "assistant", "user"]);
  assert.equal(restored.history().at(-1)?.id, before.length + 4);
  await restored.close();

  const again = readTranscript(dir, ID);
  assert.equal(again?.events.length, restored.history().length);
});

test("a crash mid-turn leaves a closed turn, a cancelled form, and a tool result for every call", () => {
  const dir = mkdtempSync(join(tmpdir(), "sessions-"));
  const lines = [
    { k: "meta", createdAt: 1, title: "delete web" },
    { k: "event", e: { id: 1, ts: 1, type: "user", text: "delete web" } },
    { k: "message", m: { role: "user", content: "delete web" } },
    { k: "event", e: { id: 2, ts: 2, type: "turn_start", provider: "p", model: "m" } },
    { k: "message", m: { role: "assistant", content: null, tool_calls: [{ id: "c1", type: "function", function: { name: "kcore_delete", arguments: "{}" } }] } },
    { k: "event", e: { id: 3, ts: 3, type: "form", formId: "f4", kind: "approval", message: "Delete?", schema: { type: "object", properties: {} } } },
  ];
  writeFileSync(join(dir, `${ID}.jsonl`), lines.map((l) => JSON.stringify(l)).join("\n") + '\n{"k":"event","e":{"id":4,"ts":4,"ty');
  const restored = new Session({ id: ID, createServer, logDir: dir, restore: readTranscript(dir, ID) });
  const types = restored.history().map((e) => e.type);
  assert.deepEqual(types, ["user", "turn_start", "form", "form_done", "error", "turn_end"]);
  assert.deepEqual(restored.pendingForms(), []);
  assert.equal(restored.isBusy, false);
  const messages = readTranscript(dir, ID)?.messages ?? [];
  const last = messages.at(-1);
  assert.equal(last?.role, "tool");
  assert.match((last as { content: string }).content, /restarted before this call finished/);
  assert.equal(repairMessages(messages).length, messages.length);
  appendFileSync(join(dir, "ignored.txt"), "");
  assert.deepEqual(listTranscripts(dir), [ID]);
});

test("the HTTP app lists sessions it finds on disk", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sessions-"));
  const first = new Session({ id: ID, createServer, logDir: dir });
  await first.send("hello", scripted(() => [{ type: "text", delta: "hi" }, { type: "done" }]), "m");
  await first.close();
  const token = "t".repeat(40);
  const app = createApp({ token, auth: new AuthStore(dir, { env: {} }), createServer, uiDir: dir, logDir: dir, summary: () => ({ controller: "x" }) });
  await new Promise<void>((r) => app.server.listen(0, "127.0.0.1", r));
  try {
    const port = (app.server.address() as { port: number }).port;
    const headers = { authorization: `Bearer ${token}` };
    const list = (await (await fetch(`http://127.0.0.1:${port}/api/sessions`, { headers })).json()) as { sessions: Array<{ id: string; title: string }> };
    assert.deepEqual(list.sessions.map((s) => [s.id, s.title]), [[ID, "hello"]]);
    const detail = (await (await fetch(`http://127.0.0.1:${port}/api/sessions/${ID}`, { headers })).json()) as { events: unknown[] };
    assert.equal(detail.events.length, 4);
  } finally {
    await app.close();
  }
});
