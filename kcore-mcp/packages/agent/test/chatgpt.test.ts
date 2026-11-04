import assert from "node:assert/strict";
import { generateKeyPairSync, createSign, type KeyObject } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { ChatGPTAuth, NotSignedIn } from "../src/chatgpt.js";
import { OpenAIResponses, toResponsesInput } from "../src/providers/openai-responses.js";

const ISSUER = "https://auth.example.test";
const START = 1_800_000_000_000;
const SCOPE = "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";

const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk = { ...(publicKey.export({ format: "jwk" }) as object), kid: "k1", alg: "RS256", use: "sig" };

function jwt(claims: Record<string, unknown>, key: KeyObject = privateKey): string {
  const header = Buffer.from(JSON.stringify({ alg: "RS256", kid: "k1" })).toString("base64url");
  const payload = Buffer.from(JSON.stringify(claims)).toString("base64url");
  const signature = createSign("sha256").update(`${header}.${payload}`).sign(key).toString("base64url");
  return `${header}.${payload}.${signature}`;
}

type Fake = { fetch: typeof fetch; calls: Array<{ url: string; form: URLSearchParams }>; config: { idToken?: (nonce: string, clientId: string) => string; scope: string; tokenError?: string; revokeStatus: number; refreshToken: number } };

function fake(): Fake {
  const calls: Fake["calls"] = [];
  const config: Fake["config"] = { scope: SCOPE, revokeStatus: 200, refreshToken: 0 };
  let lastNonce = "";
  const f: typeof fetch = async (input, init) => {
    const url = String(input);
    const form = new URLSearchParams(String(init?.body ?? ""));
    if (url.endsWith("/.well-known/openid-configuration")) {
      return Response.json({ jwks_uri: `${ISSUER}/jwks`, revocation_endpoint: `${ISSUER}/revoke` });
    }
    if (url === `${ISSUER}/jwks`) return Response.json({ keys: [jwk] });
    calls.push({ url, form });
    if (url === `${ISSUER}/revoke`) return new Response("", { status: config.revokeStatus });
    if (url === `${ISSUER}/api/accounts/oauth/token`) {
      if (config.tokenError) return Response.json({ error: config.tokenError }, { status: 400 });
      if (form.get("grant_type") === "refresh_token") {
        config.refreshToken += 1;
        return Response.json({ access_token: `access-r${config.refreshToken}`, refresh_token: `refresh-r${config.refreshToken}`, expires_in: 3600, scope: SCOPE });
      }
      const clientId = form.get("client_id") ?? "";
      const idToken = config.idToken?.(lastNonce, clientId) ?? jwt({ iss: ISSUER, aud: clientId, exp: START / 1000 + 600, nonce: lastNonce, sub: "user-1", email: "me@example.test" });
      return Response.json({ access_token: "access-1", refresh_token: "refresh-1", id_token: idToken, token_type: "Bearer", expires_in: 3600, scope: config.scope });
    }
    throw new Error(`unexpected fetch ${url}`);
  };
  return Object.assign({ fetch: f, calls, config }, { setNonce: (n: string) => (lastNonce = n) }) as Fake;
}

function setup() {
  const home = mkdtempSync(join(tmpdir(), "chatgpt-"));
  const net = fake();
  let now = START;
  const auth = new ChatGPTAuth({ home, fetch: net.fetch, issuer: ISSUER, now: () => now, sleep: async () => undefined });
  const nonceOf = (url: string) => new URL(url).searchParams.get("nonce") ?? "";
  return { home, net, auth, advance: (ms: number) => (now += ms), nonceOf, set: (n: string) => (net as unknown as { setNonce(n: string): void }).setNonce(n) };
}

async function signIn(t: ReturnType<typeof setup>, clientId = "oaiapp_abc", extra: Record<string, string> = {}) {
  const url = new URL(t.auth.start(4321));
  t.set(url.searchParams.get("nonce") ?? "");
  const params = new URLSearchParams({ code: "code-1", state: url.searchParams.get("state") ?? "", scope: SCOPE, client_id: clientId, ...extra });
  return { url, result: await t.auth.handleCallback(params) };
}

test("first sign-in registers, validates the ID token, and stores 0600 credentials", async () => {
  const t = setup();
  const { url, result } = await signIn(t);
  assert.equal(url.origin + url.pathname, `${ISSUER}/api/accounts/authorize`);
  assert.equal(url.searchParams.get("client_id"), "dynamic_agent_client");
  assert.equal(url.searchParams.get("agent_name_hint"), "kcore");
  assert.match(url.searchParams.get("ext_agent_host_id") ?? "", /^urn:uuid:[0-9a-f-]{36}$/);
  assert.equal(url.searchParams.get("redirect_uri"), "http://127.0.0.1:4321/auth/callback");
  assert.equal(url.searchParams.get("code_challenge_method"), "S256");
  assert.equal(url.searchParams.get("resource"), "https://api.openai.com/v1");
  assert.match(url.searchParams.get("scope") ?? "", /chatgpt\.tokens\.use\.direct/);
  assert.equal(result.ok, true, result.message);

  const exchange = t.net.calls.find((c) => c.form.get("grant_type") === "authorization_code");
  assert.equal(exchange?.form.get("client_id"), "oaiapp_abc");
  assert.equal(exchange?.form.get("redirect_uri"), "http://127.0.0.1:4321/auth/callback");
  assert.ok(exchange?.form.get("code_verifier"));

  const file = join(t.home, "chatgpt.json");
  assert.equal(statSync(file).mode & 0o777, 0o600);
  const saved = JSON.parse(readFileSync(file, "utf8"));
  assert.equal(saved.client_id, "oaiapp_abc");
  assert.equal(saved.subject, "user-1");
  assert.equal(await t.auth.accessToken(), "access-1");
  assert.deepEqual({ ...t.auth.status(), error: undefined }, { signedIn: true, email: "me@example.test", pending: false, error: undefined, returning: true });
});

test("the host id is chosen once and kept", async () => {
  const t = setup();
  const first = t.auth.hostId();
  assert.equal(new ChatGPTAuth({ home: t.home, issuer: ISSUER }).hostId(), first);
});

test("state is single use and must be known", async () => {
  const t = setup();
  const { url } = await signIn(t);
  const replay = await t.auth.handleCallback(new URLSearchParams({ code: "x", state: url.searchParams.get("state") ?? "", client_id: "oaiapp_abc" }));
  assert.equal(replay.ok, false);
  const stranger = await t.auth.handleCallback(new URLSearchParams({ code: "x", state: "nope", client_id: "oaiapp_abc" }));
  assert.equal(stranger.ok, false);
});

test("a declined sign-in exchanges nothing", async () => {
  const t = setup();
  const url = new URL(t.auth.start(4321));
  const result = await t.auth.handleCallback(new URLSearchParams({ error: "access_denied", state: url.searchParams.get("state") ?? "" }));
  assert.equal(result.ok, false);
  assert.match(result.message, /declined/);
  assert.equal(t.net.calls.length, 0);
  assert.equal(t.auth.status().signedIn, false);
});

test("registration without an issued client id is refused", async () => {
  const t = setup();
  const url = new URL(t.auth.start(4321));
  const result = await t.auth.handleCallback(new URLSearchParams({ code: "c", state: url.searchParams.get("state") ?? "" }));
  assert.equal(result.ok, false);
  assert.match(result.message, /client id/);
});

test("an ID token with a bad nonce, audience, signature, or issuer is rejected", async () => {
  for (const [label, claims, key] of [
    ["nonce", (n: string, c: string) => ({ iss: ISSUER, aud: c, exp: START / 1000 + 600, nonce: `${n}x`, sub: "u" }), privateKey],
    ["audience", (n: string) => ({ iss: ISSUER, aud: "someone-else", exp: START / 1000 + 600, nonce: n, sub: "u" }), privateKey],
    ["issuer", (n: string, c: string) => ({ iss: "https://evil.test", aud: c, exp: START / 1000 + 600, nonce: n, sub: "u" }), privateKey],
    ["signature", (n: string, c: string) => ({ iss: ISSUER, aud: c, exp: START / 1000 + 600, nonce: n, sub: "u" }), generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey],
  ] as const) {
    const t = setup();
    t.net.config.idToken = (n, c) => jwt(claims(n, c), key);
    const { result } = await signIn(t);
    assert.equal(result.ok, false, label);
    assert.equal(t.auth.status().signedIn, false, label);
    assert.equal(existsSync(join(t.home, "chatgpt.json")), false, label);
  }
});

test("without chatgpt.tokens.use.direct the account is not usable for inference", async () => {
  const t = setup();
  t.net.config.scope = "openid email profile offline_access resource.invoke";
  const { result } = await signIn(t, "oaiapp_abc", { scope: "openid email profile offline_access resource.invoke" });
  assert.equal(result.ok, false);
  assert.match(result.message, /plan use was not granted/);
  assert.equal(t.auth.status().signedIn, false);
});

test("tokens refresh three minutes before expiry and the rotated token replaces the old one", async () => {
  const t = setup();
  await signIn(t);
  assert.equal(await t.auth.accessToken(), "access-1");
  t.advance(3600_000 - 2 * 60_000);
  const [a, b] = await Promise.all([t.auth.accessToken(), t.auth.accessToken()]);
  assert.equal(a, "access-r1");
  assert.equal(b, "access-r1");
  assert.equal(t.net.config.refreshToken, 1);
  const refresh = t.net.calls.find((c) => c.form.get("grant_type") === "refresh_token");
  assert.equal(refresh?.form.get("client_id"), "oaiapp_abc");
  assert.equal(refresh?.form.get("refresh_token"), "refresh-1");
  assert.equal(refresh?.form.get("resource"), "https://api.openai.com/v1");
  assert.equal(refresh?.form.has("scope"), false);
  assert.equal(JSON.parse(readFileSync(join(t.home, "chatgpt.json"), "utf8")).refresh_token, "refresh-r1");
});

test("a refresh token OpenAI no longer accepts signs the account out but keeps the registration", async () => {
  const t = setup();
  await signIn(t);
  t.advance(3600_000);
  t.net.config.tokenError = "invalid_grant";
  await assert.rejects(() => t.auth.accessToken(), NotSignedIn);
  assert.equal(t.auth.status().signedIn, false);
  const next = new URL(t.auth.start(4321));
  assert.equal(next.searchParams.get("client_id"), "oaiapp_abc");
  assert.equal(next.searchParams.get("agent_name_hint"), null);
  assert.equal(next.searchParams.get("login_hint"), "me@example.test");
});

test("signing in again reuses the client id and sends the retained hints, and another account is refused", async () => {
  const t = setup();
  await signIn(t);
  const url = new URL(t.auth.start(5555));
  assert.equal(url.searchParams.get("client_id"), "oaiapp_abc");
  assert.ok(url.searchParams.get("id_token_hint"));
  t.net.config.idToken = (n, c) => jwt({ iss: ISSUER, aud: c, exp: START / 1000 + 600, nonce: n, sub: "someone-else" });
  t.set(url.searchParams.get("nonce") ?? "");
  const result = await t.auth.handleCallback(new URLSearchParams({ code: "c", state: url.searchParams.get("state") ?? "", client_id: "oaiapp_abc" }));
  assert.equal(result.ok, false);
  assert.equal(JSON.parse(readFileSync(join(t.home, "chatgpt.json"), "utf8")).subject, "user-1");
});

test("sign out revokes the refresh token, clears tokens, and keeps the registration", async () => {
  const t = setup();
  await signIn(t);
  const outcome = await t.auth.signOut();
  assert.equal(outcome.revoked, true);
  const revoke = t.net.calls.find((c) => c.url.endsWith("/revoke"));
  assert.equal(revoke?.form.get("token"), "refresh-1");
  assert.equal(revoke?.form.get("token_type_hint"), "refresh_token");
  assert.equal(revoke?.form.get("client_id"), "oaiapp_abc");
  const saved = JSON.parse(readFileSync(join(t.home, "chatgpt.json"), "utf8"));
  assert.equal(saved.access_token, undefined);
  assert.equal(saved.refresh_token, undefined);
  assert.equal(saved.id_token, undefined);
  assert.equal(saved.client_id, "oaiapp_abc");
  await assert.rejects(() => t.auth.accessToken(), NotSignedIn);
});

test("sign out still clears locally when revocation cannot be confirmed", async () => {
  const t = setup();
  await signIn(t);
  t.net.config.revokeStatus = 503;
  const outcome = await t.auth.signOut();
  assert.equal(outcome.revoked, false);
  assert.equal(t.auth.status().signedIn, false);
});

function sse(events: unknown[]): Response {
  return new Response(events.map((e) => `data: ${JSON.stringify(e)}\n\n`).join(""), { headers: { "content-type": "text/event-stream" } });
}

test("the Responses adapter sends the shape ChatGPT plan usage requires and reads the stream", async () => {
  let sent: Record<string, unknown> = {};
  const responses = new OpenAIResponses({
    id: "chatgpt",
    token: async () => "tok",
    fetch: async (_url, init) => {
      sent = JSON.parse(String(init?.body));
      assert.equal((init?.headers as Record<string, string>).authorization, "Bearer tok");
      return sse([
        { type: "response.output_text.delta", delta: "Hel" },
        { type: "response.output_text.delta", delta: "lo" },
        { type: "response.output_item.done", item: { type: "function_call", call_id: "c1", name: "kcore_read", arguments: '{"a":1}' } },
        { type: "response.completed", response: { usage: { input_tokens: 5, output_tokens: 2 } } },
      ]);
    },
  });
  const events = [];
  for await (const e of responses.stream({
    model: "m",
    messages: [
      { role: "system", content: "be brief" },
      { role: "user", content: "hi" },
      { role: "assistant", content: null, tool_calls: [{ id: "c0", type: "function", function: { name: "kcore_read", arguments: "{}" } }] },
      { role: "tool", tool_call_id: "c0", content: "ok" },
    ],
    tools: [{ name: "kcore_read", description: "d", parameters: { type: "object", properties: {} } }],
  })) {
    events.push(e);
  }
  assert.equal(sent.store, false);
  assert.equal(sent.stream, true);
  assert.equal(sent.instructions, "be brief");
  for (const banned of ["temperature", "max_output_tokens", "previous_response_id", "user", "metadata"]) assert.equal(banned in sent, false, banned);
  const tools = sent.tools as Array<{ type: string; name: string; tools: Array<{ name: string }> }>;
  assert.equal(tools[0].type, "namespace");
  assert.equal(tools[0].name, "kcore");
  assert.equal(tools[0].tools[0].name, "kcore_read");
  const input = sent.input as Array<Record<string, unknown>>;
  assert.equal(input.some((i) => i.role === "system"), false);
  assert.deepEqual(input[1], { type: "function_call", call_id: "c0", name: "kcore_read", namespace: "kcore", arguments: "{}" });
  assert.deepEqual(input[2], { type: "function_call_output", call_id: "c0", output: "ok" });
  assert.deepEqual(events, [
    { type: "text", delta: "Hel" },
    { type: "text", delta: "lo" },
    { type: "tool_call", call: { id: "c1", name: "kcore_read", arguments: '{"a":1}' } },
    { type: "done", usage: { inputTokens: 5, outputTokens: 2 } },
  ]);
});

test("a stream with no response.completed, a failure, or a usage limit is an error", async () => {
  const run = async (events: unknown[]) => {
    const p = new OpenAIResponses({ id: "c", token: async () => "t", fetch: async () => sse(events) });
    for await (const _ of p.stream({ model: "m", messages: [{ role: "user", content: "x" }], tools: [] })) void _;
  };
  await assert.rejects(() => run([{ type: "response.output_text.delta", delta: "x" }]), /before response\.completed/);
  await assert.rejects(() => run([{ type: "response.failed", response: { error: { code: "subscription_sharing_usage_limit_exceeded" } } }]), /usage limit/);
  await assert.rejects(() => run([{ type: "response.incomplete", response: { incomplete_details: { reason: "max_tokens" } } }]), /max_tokens/);
});

test("a 401 refreshes the token once and retries", async () => {
  const seen: Array<string | boolean | undefined> = [];
  let calls = 0;
  const p = new OpenAIResponses({
    id: "c",
    token: async (force) => {
      seen.push(force);
      return force ? "new" : "old";
    },
    fetch: async (_u, init) => {
      calls += 1;
      return (init?.headers as Record<string, string>).authorization === "Bearer new" ? sse([{ type: "response.completed", response: {} }]) : new Response("no", { status: 401 });
    },
  });
  for await (const _ of p.stream({ model: "m", messages: [{ role: "user", content: "x" }], tools: [] })) void _;
  assert.equal(calls, 2);
  assert.deepEqual(seen, [undefined, true]);
});

test("the model list keeps what ChatGPT marks as listed", async () => {
  const p = new OpenAIResponses({
    id: "c",
    token: async () => "t",
    fetch: async () => Response.json({ models: [{ slug: "a", visibility: "list" }, { slug: "hidden", visibility: "hide" }, { slug: "b", visibility: "list" }] }),
  });
  assert.deepEqual(await p.models(), ["a", "b"]);
});

test("toResponsesInput drops system messages", () => {
  assert.deepEqual(toResponsesInput([{ role: "system", content: "s" }, { role: "user", content: "u" }], "kcore"), [{ role: "user", content: "u" }]);
});
