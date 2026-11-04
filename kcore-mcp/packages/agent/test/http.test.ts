import assert from "node:assert/strict";
import { mkdtempSync, statSync } from "node:fs";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import type { AddressInfo } from "node:net";
import { AuthStore } from "../src/auth.js";
import { createApp } from "../src/http.js";
import { createServer } from "../../server/src/server.js";
import { waitFor } from "./support.js";

const TOKEN = "t".repeat(40);

function raw(port: number, path: string, headers: Record<string, string>): Promise<number> {
  return new Promise((resolve, reject) => {
    const req = request({ host: "127.0.0.1", port, path, headers }, (res) => {
      res.resume();
      resolve(res.statusCode ?? 0);
    });
    req.on("error", reject);
    req.end();
  });
}

function sse(chunks: unknown[]): Response {
  const body = chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join("") + "data: [DONE]\n\n";
  return new Response(body, { headers: { "content-type": "text/event-stream" } });
}

const fakeOpenAI: typeof fetch = async (input, init) => {
  const url = String(input);
  if (url.endsWith("/models")) return Response.json({ data: [{ id: "gpt-b" }, { id: "gpt-a" }] });
  const body = JSON.parse(String(init?.body)) as { messages: Array<{ role: string }>; tools: unknown[] };
  assert.ok(body.tools.length > 0);
  if (body.messages.some((m) => m.role === "tool")) {
    return sse([{ choices: [{ delta: { content: "Done." } }] }, { choices: [], usage: { prompt_tokens: 3, completion_tokens: 1 } }]);
  }
  return sse([
    { choices: [{ delta: { tool_calls: [{ index: 0, id: "call_1", function: { name: "kcore_cat", arguments: "" } }] } }] },
    { choices: [{ delta: { tool_calls: [{ index: 0, function: { name: "alog", arguments: "{}" } }] } }] },
  ]);
};

async function start() {
  const home = mkdtempSync(join(tmpdir(), "kcore-agent-"));
  const app = createApp({
    token: TOKEN,
    auth: new AuthStore(home, { fetch: fakeOpenAI, env: {} }),
    createServer,
    uiDir: new URL("../ui", import.meta.url).pathname,
    summary: () => ({ controller: "127.0.0.1:9090", operator: "kctl" }),
  });
  await new Promise<void>((r) => app.server.listen(0, "127.0.0.1", r));
  const port = (app.server.address() as AddressInfo).port;
  const base = `http://127.0.0.1:${port}`;
  const api = (path: string, init: RequestInit = {}) =>
    fetch(base + path, { ...init, headers: { authorization: `Bearer ${TOKEN}`, "content-type": "application/json", ...init.headers } });
  return { app, base, port, home, api };
}

test("the API needs the session token and a loopback Host", async () => {
  const { app, base, port } = await start();
  try {
    assert.equal((await fetch(`${base}/api/providers`)).status, 401);
    assert.equal((await fetch(`${base}/api/providers`, { headers: { authorization: "Bearer nope" } })).status, 401);
    const auth = { authorization: `Bearer ${TOKEN}` };
    assert.equal(await raw(port, "/api/providers", { ...auth, host: `evil.example:${port}` }), 403);
    assert.equal(await raw(port, "/api/providers", { ...auth, host: `127.0.0.1:${port}`, origin: "https://evil.example" }), 403);
    assert.equal(await raw(port, "/api/providers", { ...auth, host: `127.0.0.1:${port}` }), 200);
    const page = await fetch(`${base}/`);
    assert.equal(page.status, 200);
    assert.match(page.headers.get("content-security-policy") ?? "", /default-src 'none'/);
  } finally {
    await app.close();
  }
});

test("a provider key is stored 0600 and never returned", async () => {
  const { app, home, api } = await start();
  try {
    const put = await api("/api/providers/openai/key", { method: "PUT", body: JSON.stringify({ key: "sk-secret-123" }) });
    const text = await put.text();
    assert.equal(put.status, 200);
    assert.equal(text.includes("sk-secret-123"), false);
    assert.equal((statSync(join(home, "auth.json")).mode & 0o777).toString(8), "600");
    const models = (await (await api("/api/providers/openai/models")).json()) as { models: string[] };
    assert.deepEqual(models.models, ["gpt-a", "gpt-b"]);
    assert.equal((await api("/api/providers/nope/key", { method: "PUT", body: JSON.stringify({ key: "x" }) })).status, 400);
  } finally {
    await app.close();
  }
});

test("a chat turn streams from the provider, runs an MCP tool, and lands in the session", async () => {
  const { app, api } = await start();
  try {
    await api("/api/providers/openai/key", { method: "PUT", body: JSON.stringify({ key: "sk-x" }) });
    const { id } = (await (await api("/api/sessions", { method: "POST", body: "{}" })).json()) as { id: string };
    const refused = await api(`/api/sessions/${id}/messages`, { method: "POST", body: JSON.stringify({ text: "hi", provider: "deepseek", model: "m" }) });
    assert.equal(refused.status, 400);
    const sent = await api(`/api/sessions/${id}/messages`, { method: "POST", body: JSON.stringify({ text: "what do you manage?", provider: "openai", model: "gpt-a" }) });
    assert.equal(sent.status, 202);
    const events = await waitFor(() => {
      const s = app.sessions.get(id);
      const h = s?.history() ?? [];
      return h.some((e) => e.type === "turn_end") ? h : undefined;
    });
    const types = events.map((e) => e.type);
    assert.deepEqual(types, ["user", "turn_start", "tool_call", "tool_result", "assistant", "turn_end"]);
    const detail = (await (await api(`/api/sessions/${id}`)).json()) as { events: unknown[]; title: string };
    assert.equal(detail.events.length, 6);
    assert.equal(detail.title, "what do you manage?");
  } finally {
    await app.close();
  }
});

test("answering a form over HTTP validates the content", async () => {
  const { app, api } = await start();
  try {
    await api("/api/providers/openai/key", { method: "PUT", body: JSON.stringify({ key: "sk-x" }) });
    const { id } = (await (await api("/api/sessions", { method: "POST", body: "{}" })).json()) as { id: string };
    const session = app.sessions.get(id)!;
    const pending = session.send("x", { id: "s", models: async () => [], async *stream(r) {
      if (r.messages.some((m) => m.role === "tool")) { yield { type: "done" }; return; }
      yield { type: "tool_call", call: { id: "c", name: "kcore_delete", arguments: JSON.stringify({ kind: "vm", name: "web" }) } };
      yield { type: "done" };
    } }, "m");
    const form = await waitFor(() => session.history().find((e) => e.type === "form") as { formId: string } | undefined);
    const bad = await api(`/api/sessions/${id}/forms/${form.formId}`, { method: "POST", body: JSON.stringify({ action: "accept", content: { confirm: "perhaps" } }) });
    assert.equal(bad.status, 400);
    const good = await api(`/api/sessions/${id}/forms/${form.formId}`, { method: "POST", body: JSON.stringify({ action: "accept", content: { confirm: "no" } }) });
    assert.equal(good.status, 200);
    await pending;
  } finally {
    await app.close();
  }
});

test("OPENAI_BASE_URL redirects the provider", async () => {
  const seen: string[] = [];
  const auth = new AuthStore(mkdtempSync(join(tmpdir(), "kcore-agent-")), {
    env: { OPENAI_API_KEY: "sk-env", OPENAI_BASE_URL: "http://llm.internal:8000/v1/" },
    fetch: async (input) => {
      seen.push(String(input));
      return Response.json({ data: [{ id: "local-model" }] });
    },
  });
  assert.deepEqual(await auth.provider("openai")!.models(), ["local-model"]);
  assert.deepEqual(seen, ["http://llm.internal:8000/v1/models"]);
  assert.equal(auth.describe().find((p) => p.id === "openai")?.source, "env");
});

test("the ChatGPT sign-in starts behind the token, and the callback is open to the browser but needs a known state", async () => {
  const { app, base, port, api } = await start();
  try {
    assert.equal((await fetch(`${base}/api/providers/chatgpt/signin`, { method: "POST" })).status, 401);
    const started = (await (await api("/api/providers/chatgpt/signin", { method: "POST", body: "{}" })).json()) as { url: string };
    const url = new URL(started.url);
    assert.equal(url.searchParams.get("redirect_uri"), `http://127.0.0.1:${port}/auth/callback`);
    const providers = (await (await api("/api/providers")).json()) as { providers: Array<{ id: string; pending?: boolean; connected: boolean }> };
    const chatgpt = providers.providers.find((p) => p.id === "chatgpt");
    assert.equal(chatgpt?.pending, true);
    assert.equal(chatgpt?.connected, false);
    assert.deepEqual(providers.providers.map((p) => p.id), ["chatgpt", "openai", "anthropic", "deepseek", "opencode", "opencode-go"]);

    assert.equal((await fetch(`${base}/auth/callback?code=x&state=unknown`)).status, 400);
    const denied = await fetch(`${base}/auth/callback?error=access_denied&state=${url.searchParams.get("state")}`);
    assert.equal(denied.status, 400);
    assert.match(await denied.text(), /declined/);
    assert.equal(await raw(port, "/auth/callback?state=x", { host: `evil.example:${port}` }), 403);
  } finally {
    await app.close();
  }
});

test("a plan result carries its app, which is served sandboxed, and apps can only make read-only calls", async () => {
  const { app, base, api } = await start();
  try {
    const { id } = (await (await api("/api/sessions", { method: "POST", body: "{}" })).json()) as { id: string };
    const session = app.sessions.get(id)!;
    await session.send(
      "plan a cluster",
      {
        id: "s",
        models: async () => [],
        async *stream(r) {
          if (r.messages.some((m) => m.role === "tool")) {
            yield { type: "done" };
            return;
          }
          yield { type: "tool_call", call: { id: "p1", name: "kcore_plan", arguments: JSON.stringify({ kind: "cluster", spec: { name: "lab", controller: "10.0.0.1" } }) } };
          yield { type: "done" };
        },
      },
      "m",
    );
    const result = session.history().find((e) => e.type === "tool_result") as { app?: { uri: string }; ok: boolean };
    assert.deepEqual(result.app, { uri: "ui://kcore/plan" });

    const refused = await api(`/api/sessions/${id}/apps`, { method: "POST", body: JSON.stringify({ uri: "ui://evil/app" }) });
    assert.equal(refused.status, 400);
    const made = (await (await api(`/api/sessions/${id}/apps`, { method: "POST", body: JSON.stringify({ uri: "ui://kcore/plan" }) })).json()) as { url: string };
    assert.match(made.url, /^\/apps\/[0-9a-f]{48}$/);

    const page = await fetch(base + made.url);
    assert.equal(page.status, 200);
    assert.match(page.headers.get("content-security-policy") ?? "", /sandbox allow-scripts/);
    assert.match(page.headers.get("content-security-policy") ?? "", /default-src 'none'/);
    assert.equal(page.headers.get("x-frame-options"), null);
    assert.match(await page.text(), /ui\/initialize/);
    assert.equal((await fetch(`${base}/apps/${"0".repeat(48)}`)).status, 404);
    assert.match((await fetch(`${base}/`)).headers.get("content-security-policy") ?? "", /frame-src 'self'/);

    const read = await api(`/api/sessions/${id}/call`, { method: "POST", body: JSON.stringify({ name: "kcore_catalog", arguments: {} }) });
    assert.equal(read.status, 200);
    const blocked = await api(`/api/sessions/${id}/call`, { method: "POST", body: JSON.stringify({ name: "kcore_apply", arguments: { kind: "vm", confirm: true } }) });
    assert.equal(blocked.status, 400);
    assert.match(((await blocked.json()) as { error: string }).error, /may not call kcore_apply/);
  } finally {
    await app.close();
  }
});
