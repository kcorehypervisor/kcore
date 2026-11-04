import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import type { AddressInfo } from "node:net";
import { AuthStore } from "../src/auth.js";
import { createApp } from "../src/http.js";
import { createServer } from "../../server/src/server.js";

const TOKEN = "t".repeat(40);

function tmp(): string {
  return mkdtempSync(join(tmpdir(), "kcore-opencode-"));
}

function openCodeFile(entries: unknown): string {
  const path = join(tmp(), "auth.json");
  writeFileSync(path, JSON.stringify(entries));
  return path;
}

test("OpenCode providers use the console gateway and its own model list", async () => {
  const seen: string[] = [];
  const auth = new AuthStore(tmp(), {
    env: { OPENCODE_API_KEY: "oc-key", OPENCODE_GO_API_KEY: "go-key" },
    fetch: async (input, init) => {
      seen.push(`${String(input)} ${(init?.headers as Record<string, string>).authorization}`);
      return Response.json({ data: [{ id: "claude-sonnet-5-5" }, { id: "deepseek-v4-pro" }] });
    },
  });
  assert.deepEqual(await auth.provider("opencode")!.models(), ["claude-sonnet-5-5", "deepseek-v4-pro"]);
  await auth.provider("opencode-go")!.models();
  assert.deepEqual(seen, [
    "https://opencode.ai/inference/v1/models Bearer oc-key",
    "https://opencode.ai/inference/go/v1/models Bearer go-key",
  ]);
  const infos = auth.describe();
  assert.equal(infos.find((p) => p.id === "opencode")?.defaultModel, "claude-sonnet-5-5");
  assert.equal(infos.find((p) => p.id === "anthropic")?.keyUrl, "https://platform.claude.com/settings/keys");
  assert.equal(infos.find((p) => p.id === "deepseek")?.keyUrl, "https://platform.deepseek.com/api_keys");
});

test("OPENCODE_BASE_URL redirects the gateway and the model list with it", async () => {
  const seen: string[] = [];
  const auth = new AuthStore(tmp(), {
    env: { OPENCODE_API_KEY: "k", OPENCODE_BASE_URL: "http://gw.internal/v1" },
    fetch: async (input) => {
      seen.push(String(input));
      return Response.json({ data: [] });
    },
  });
  await auth.provider("opencode")!.models();
  assert.deepEqual(seen, ["http://gw.internal/v1/models"]);
});

test("OpenCode import copies API keys, skips account logins and keys we already hold", () => {
  const path = openCodeFile({
    anthropic: { type: "api", key: "sk-ant-from-opencode" },
    deepseek: { type: "api", key: "sk-deep" },
    openai: { type: "oauth", refresh: "r", access: "a", expires: 1 },
    opencode: { type: "api", key: "oc-1" },
    unknown: { type: "api", key: "zzz" },
  });
  const home = tmp();
  const auth = new AuthStore(home, { env: {}, openCodeAuthPath: path });
  auth.setKey("deepseek", "mine");

  const scan = auth.scanOpenCode();
  assert.equal(scan.found, true);
  assert.deepEqual(scan.importable.map((i) => [i.id, i.stored]), [["anthropic", false], ["deepseek", true], ["opencode", false]]);
  assert.deepEqual(scan.skipped.map((s) => s.openCodeId), ["openai"]);
  assert.ok(!JSON.stringify(scan).includes("sk-ant-from-opencode"));

  assert.deepEqual(auth.importOpenCode(), ["anthropic", "opencode"]);
  const stored = JSON.parse(readFileSync(join(home, "auth.json"), "utf8")) as Record<string, { key: string }>;
  assert.equal(stored.anthropic?.key, "sk-ant-from-opencode");
  assert.equal(stored.deepseek?.key, "mine");
  assert.equal(stored.openai, undefined);
  assert.deepEqual(auth.importOpenCode(), []);
});

test("a missing or broken OpenCode file imports nothing", () => {
  const missing = new AuthStore(tmp(), { env: {}, openCodeAuthPath: join(tmp(), "nope.json") });
  assert.deepEqual(missing.scanOpenCode(), { found: false, importable: [], skipped: [] });
  assert.deepEqual(missing.importOpenCode(), []);
  const broken = join(tmp(), "auth.json");
  writeFileSync(broken, "{not json");
  const auth = new AuthStore(tmp(), { env: {}, openCodeAuthPath: broken });
  assert.equal(auth.scanOpenCode().found, false);
});

test("the import routes sit behind the token and never return a key", async () => {
  const path = openCodeFile({ anthropic: { type: "api", key: "sk-ant-secret-xyz" } });
  const app = createApp({
    token: TOKEN,
    auth: new AuthStore(tmp(), { env: {}, openCodeAuthPath: path }),
    createServer,
    uiDir: new URL("../ui", import.meta.url).pathname,
    summary: () => ({ controller: "127.0.0.1:9090", operator: "kctl" }),
  });
  await new Promise<void>((r) => app.server.listen(0, "127.0.0.1", r));
  const base = `http://127.0.0.1:${(app.server.address() as AddressInfo).port}`;
  try {
    assert.equal((await fetch(`${base}/api/import/opencode`)).status, 401);
    assert.equal((await fetch(`${base}/api/import/opencode`, { method: "POST" })).status, 401);
    const headers = { authorization: `Bearer ${TOKEN}`, "content-type": "application/json" };
    const scan = await (await fetch(`${base}/api/import/opencode`, { headers })).text();
    assert.ok(!scan.includes("sk-ant-secret-xyz"));
    const done = await fetch(`${base}/api/import/opencode`, { method: "POST", headers, body: "{}" });
    const text = await done.text();
    assert.ok(!text.includes("sk-ant-secret-xyz"));
    const body = JSON.parse(text) as { imported: string[]; providers: Array<{ id: string; connected: boolean }> };
    assert.deepEqual(body.imported, ["anthropic"]);
    assert.equal(body.providers.find((p) => p.id === "anthropic")?.connected, true);
  } finally {
    app.server.close();
  }
});
