import { createHash, createPublicKey, randomBytes, randomUUID, verify as verifySignature, type JsonWebKey } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// Sign in with ChatGPT, open-source flow: https://developers.openai.com/siwc/token-sharing-open-source
const DYNAMIC_CLIENT = "dynamic_agent_client";
const AGENT_NAME = "kcore";
const RESOURCE = "https://api.openai.com/v1";
const DIRECT_SCOPE = "chatgpt.tokens.use.direct";
const SCOPES = `openid profile email offline_access resource.invoke ${DIRECT_SCOPE}`;
const REFRESH_MARGIN_MS = 3 * 60_000;
const ATTEMPT_TTL_MS = 10 * 60_000;
const DEAD_REFRESH = new Set([
  "invalid_grant",
  "invalid_refresh_token",
  "token_expired",
  "refresh_token_expired",
  "refresh_token_invalidated",
  "refresh_token_reused",
]);

export const CALLBACK_PATH = "/auth/callback";
export const CHATGPT_USAGE_URL = "https://chatgpt.com/settings/usage";

export type ChatGPTOptions = {
  home: string;
  fetch?: typeof fetch;
  issuer?: string;
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
};

export type ChatGPTRecord = {
  email: string;
  issuer: string;
  subject: string;
  client_id: string;
  ext_agent_host_id: string;
  id_token?: string;
  access_token?: string;
  refresh_token?: string;
  token_type: string;
  expires_in: number;
  expires_at?: number;
  scopes: string[];
  saved_at: string;
};

export type ChatGPTStatus = {
  signedIn: boolean;
  email?: string;
  pending: boolean;
  error?: string;
  /** A previous account is remembered, so the next sign-in reuses its registration. */
  returning: boolean;
};

type Attempt = {
  verifier: string;
  nonce: string;
  redirectUri: string;
  clientId?: string;
  createdAt: number;
};

type TokenResponse = {
  access_token?: string;
  refresh_token?: string;
  id_token?: string;
  token_type?: string;
  expires_in?: number;
  scope?: string;
  error?: string;
  error_description?: string;
};

type Discovery = { jwks_uri?: string; revocation_endpoint?: string; token_endpoint?: string };

export class NotSignedIn extends Error {
  constructor(message = "ChatGPT is not signed in") {
    super(message);
  }
}

export class ChatGPTAuth {
  private readonly home: string;
  private readonly fetchImpl: typeof fetch;
  private readonly issuer: string;
  private readonly now: () => number;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly attempts = new Map<string, Attempt>();
  private lastError: string | undefined;
  private refreshing: Promise<ChatGPTRecord> | undefined;
  private discovery: Promise<Discovery> | undefined;

  constructor(options: ChatGPTOptions) {
    this.home = options.home;
    this.fetchImpl = options.fetch ?? fetch;
    this.issuer = (options.issuer ?? "https://auth.openai.com").replace(/\/+$/, "");
    this.now = options.now ?? Date.now;
    this.sleep = options.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
  }

  private get recordPath(): string {
    return join(this.home, "chatgpt.json");
  }

  private get hostPath(): string {
    return join(this.home, "host.json");
  }

  private get authorizeUrl(): string {
    return `${this.issuer}/api/accounts/authorize`;
  }

  private get tokenUrl(): string {
    return `${this.issuer}/api/accounts/oauth/token`;
  }

  /** One opaque id per machine, chosen before the first sign-in and never changed. */
  hostId(): string {
    if (existsSync(this.hostPath)) {
      try {
        const found = (JSON.parse(readFileSync(this.hostPath, "utf8")) as { ext_agent_host_id?: string }).ext_agent_host_id;
        if (found) return found;
      } catch {
        // fall through and write a fresh one
      }
    }
    const id = `urn:uuid:${randomUUID()}`;
    this.writeAtomic(this.hostPath, { ext_agent_host_id: id });
    return id;
  }

  private load(): ChatGPTRecord | undefined {
    if (!existsSync(this.recordPath)) return undefined;
    try {
      return JSON.parse(readFileSync(this.recordPath, "utf8")) as ChatGPTRecord;
    } catch {
      return undefined;
    }
  }

  private writeAtomic(path: string, value: unknown): void {
    mkdirSync(this.home, { recursive: true, mode: 0o700 });
    const temp = `${path}.${process.pid}.${randomBytes(4).toString("hex")}.tmp`;
    writeFileSync(temp, JSON.stringify(value, null, 2), { mode: 0o600 });
    chmodSync(temp, 0o600);
    renameSync(temp, path);
  }

  private save(record: ChatGPTRecord): void {
    this.writeAtomic(this.recordPath, record);
  }

  status(): ChatGPTStatus {
    this.expireAttempts();
    const record = this.load();
    return {
      signedIn: Boolean(record?.access_token && record.scopes.includes(DIRECT_SCOPE)),
      email: record?.email,
      pending: this.attempts.size > 0,
      error: this.lastError,
      returning: Boolean(record?.client_id),
    };
  }

  private expireAttempts(): void {
    for (const [state, attempt] of this.attempts) {
      if (this.now() - attempt.createdAt > ATTEMPT_TTL_MS) this.attempts.delete(state);
    }
  }

  /**
   * The page to open in the browser. `port` is the port this agent listens on; only the port may change
   * between sign-ins, the host and path are fixed by OpenAI's registration.
   */
  start(port: number, options: { newAccount?: boolean } = {}): string {
    this.expireAttempts();
    this.lastError = undefined;
    const saved = this.load();
    const reuse = !options.newAccount && saved?.client_id ? saved : undefined;
    const state = base64url(randomBytes(24));
    const nonce = base64url(randomBytes(24));
    const verifier = base64url(randomBytes(48));
    const redirectUri = `http://127.0.0.1:${port}${CALLBACK_PATH}`;
    this.attempts.set(state, { verifier, nonce, redirectUri, clientId: reuse?.client_id, createdAt: this.now() });

    const query = new URLSearchParams();
    query.set("client_id", reuse?.client_id ?? DYNAMIC_CLIENT);
    if (!reuse) query.set("agent_name_hint", AGENT_NAME);
    query.set("ext_agent_host_id", this.hostId());
    if (reuse?.id_token) query.set("id_token_hint", reuse.id_token);
    if (reuse?.email) query.set("login_hint", reuse.email);
    query.set("response_type", "code");
    query.set("redirect_uri", redirectUri);
    query.set("scope", SCOPES);
    query.set("resource", RESOURCE);
    query.set("state", state);
    query.set("nonce", nonce);
    query.set("code_challenge", base64url(createHash("sha256").update(verifier).digest()));
    query.set("code_challenge_method", "S256");
    return `${this.authorizeUrl}?${query.toString()}`;
  }

  /** The browser landed on our loopback callback. The message is shown to the operator, so it carries no secrets. */
  async handleCallback(params: URLSearchParams): Promise<{ ok: boolean; message: string }> {
    this.expireAttempts();
    const state = params.get("state") ?? "";
    const attempt = this.attempts.get(state);
    if (!attempt) return this.fail("This sign-in attempt is unknown or has expired. Start it again from the agent page.");
    this.attempts.delete(state);

    const oauthError = params.get("error");
    if (oauthError) {
      return this.fail(
        oauthError === "access_denied"
          ? "Sign-in was declined in the browser, so nothing was connected."
          : `ChatGPT sign-in failed: ${oauthError}${params.get("error_description") ? ` (${params.get("error_description")})` : ""}`,
      );
    }
    const code = params.get("code");
    if (!code) return this.fail("ChatGPT did not return an authorization code.");

    const callbackClient = params.get("client_id") ?? undefined;
    let clientId = attempt.clientId;
    if (clientId) {
      if (callbackClient && callbackClient !== clientId) return this.fail("ChatGPT answered for a different registration, so the result was rejected.");
    } else {
      if (!callbackClient || callbackClient === DYNAMIC_CLIENT) return this.fail("ChatGPT did not issue a client id, so registration is incomplete.");
      clientId = callbackClient;
    }

    let tokens: TokenResponse;
    try {
      tokens = await this.postToken(
        new URLSearchParams({
          grant_type: "authorization_code",
          client_id: clientId,
          code,
          code_verifier: attempt.verifier,
          redirect_uri: attempt.redirectUri,
          resource: RESOURCE,
        }),
      );
    } catch (error) {
      if (error instanceof TokenError && error.code === "invalid_grant") {
        return this.fail("ChatGPT rejected the sign-in code. Start the sign-in again.");
      }
      return this.fail(`Could not exchange the sign-in code: ${describeError(error)}`);
    }
    if (!tokens.access_token || !tokens.id_token) return this.fail("ChatGPT returned an incomplete token response.");

    let claims: Record<string, unknown>;
    try {
      claims = await this.verifyIdToken(tokens.id_token, clientId, attempt.nonce);
    } catch (error) {
      return this.fail(`ChatGPT returned an ID token that did not check out: ${describeError(error)}`);
    }

    const scopes = (tokens.scope ?? params.get("scope") ?? "").split(/[\s+]+/).filter(Boolean);
    if (!scopes.includes(DIRECT_SCOPE)) {
      return this.fail(
        `ChatGPT plan use was not granted to this app. Allow it under ChatGPT Settings, then sign in again. Usage is shown at ${CHATGPT_USAGE_URL}.`,
      );
    }

    const subject = String(claims.sub);
    const previous = this.load();
    if (attempt.clientId && previous && previous.subject !== subject) {
      return this.fail("The ChatGPT account that signed in is not the account this registration belongs to.");
    }

    const replaced = previous && previous.subject !== subject && previous.refresh_token ? previous : undefined;
    const expiresIn = Number(tokens.expires_in ?? 3600);
    this.save({
      email: typeof claims.email === "string" ? claims.email : (previous?.email ?? ""),
      issuer: this.issuer,
      subject,
      client_id: clientId,
      ext_agent_host_id: this.hostId(),
      id_token: tokens.id_token,
      access_token: tokens.access_token,
      refresh_token: tokens.refresh_token,
      token_type: tokens.token_type ?? "Bearer",
      expires_in: expiresIn,
      expires_at: this.now() + expiresIn * 1000,
      scopes,
      saved_at: new Date(this.now()).toISOString(),
    });
    this.lastError = undefined;
    if (replaced) void this.revoke(replaced).catch(() => undefined);
    return { ok: true, message: "Signed in with ChatGPT. You can close this tab and go back to the agent." };
  }

  private fail(message: string): { ok: false; message: string } {
    this.lastError = message;
    return { ok: false, message };
  }

  /** A token good for at least three more minutes. Refreshes are serialized because the refresh token rotates. */
  async accessToken(force = false): Promise<string> {
    const record = this.load();
    if (!record?.access_token || !record.scopes.includes(DIRECT_SCOPE)) throw new NotSignedIn();
    const fresh = (record.expires_at ?? 0) - this.now() > REFRESH_MARGIN_MS;
    if (fresh && !force) return record.access_token;
    this.refreshing ??= this.refresh().finally(() => {
      this.refreshing = undefined;
    });
    const updated = await this.refreshing;
    if (!updated.access_token) throw new NotSignedIn();
    return updated.access_token;
  }

  private async refresh(): Promise<ChatGPTRecord> {
    const record = this.load();
    if (!record?.refresh_token) throw new NotSignedIn("ChatGPT is not signed in, or has no refresh token. Sign in again.");
    let tokens: TokenResponse;
    try {
      tokens = await this.postToken(
        new URLSearchParams({
          grant_type: "refresh_token",
          client_id: record.client_id,
          refresh_token: record.refresh_token,
          resource: RESOURCE,
        }),
      );
    } catch (error) {
      if (error instanceof TokenError && DEAD_REFRESH.has(error.code)) {
        this.save({ ...record, access_token: undefined, refresh_token: undefined, id_token: undefined, expires_at: undefined });
        this.lastError = "ChatGPT no longer accepts this sign-in. Sign in again.";
        throw new NotSignedIn(this.lastError);
      }
      throw error;
    }
    if (!tokens.access_token) throw new Error("ChatGPT returned no access token on refresh");
    const scopes = tokens.scope ? tokens.scope.split(/\s+/).filter(Boolean) : record.scopes;
    const expiresIn = Number(tokens.expires_in ?? record.expires_in);
    const next: ChatGPTRecord = {
      ...record,
      access_token: tokens.access_token,
      refresh_token: tokens.refresh_token ?? record.refresh_token,
      id_token: tokens.id_token ?? record.id_token,
      expires_in: expiresIn,
      expires_at: this.now() + expiresIn * 1000,
      scopes,
      saved_at: new Date(this.now()).toISOString(),
    };
    this.save(next);
    return next;
  }

  /**
   * Stops using the account, revokes the refresh token, and clears the tokens. The registration and the host id
   * stay so the next sign-in reuses them. Returns whether OpenAI confirmed the revocation.
   */
  async signOut(): Promise<{ revoked: boolean }> {
    const record = this.load();
    this.attempts.clear();
    this.lastError = undefined;
    if (!record) return { revoked: true };
    let revoked = true;
    if (record.refresh_token) {
      try {
        await this.revoke(record);
      } catch {
        revoked = false;
      }
    }
    this.save({ ...record, access_token: undefined, refresh_token: undefined, id_token: undefined, expires_at: undefined });
    return { revoked };
  }

  private async revoke(record: ChatGPTRecord): Promise<void> {
    const endpoint = (await this.discover()).revocation_endpoint;
    if (!endpoint) throw new Error("OpenAI publishes no revocation endpoint");
    let last: unknown;
    for (let attempt = 0; attempt < 3; attempt += 1) {
      try {
        const response = await this.fetchImpl(endpoint, {
          method: "POST",
          headers: { "content-type": "application/x-www-form-urlencoded", accept: "application/json" },
          body: new URLSearchParams({ token: record.refresh_token ?? "", token_type_hint: "refresh_token", client_id: record.client_id }),
          signal: AbortSignal.timeout(15_000),
        });
        if (response.status === 200) return;
        if (response.status < 500) throw new Error(`revocation answered ${response.status}`);
        last = new Error(`revocation answered ${response.status}`);
      } catch (error) {
        last = error;
        if (error instanceof Error && error.message.startsWith("revocation answered 4")) throw error;
      }
      await this.sleep(250 * 2 ** attempt);
    }
    throw last instanceof Error ? last : new Error("revocation failed");
  }

  private discover(): Promise<Discovery> {
    this.discovery ??= this.fetchImpl(`${this.issuer}/.well-known/openid-configuration`, { signal: AbortSignal.timeout(15_000) })
      .then(async (response) => {
        if (!response.ok) throw new Error(`discovery answered ${response.status}`);
        return (await response.json()) as Discovery;
      })
      .catch((error: unknown) => {
        this.discovery = undefined;
        throw error;
      });
    return this.discovery;
  }

  private async postToken(body: URLSearchParams): Promise<TokenResponse> {
    const response = await this.fetchImpl(this.tokenUrl, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded", accept: "application/json" },
      body,
      signal: AbortSignal.timeout(30_000),
    });
    const text = await response.text();
    let parsed: TokenResponse = {};
    try {
      parsed = JSON.parse(text) as TokenResponse;
    } catch {
      // keep the empty object; the status tells the story
    }
    if (!response.ok) throw new TokenError(parsed.error ?? `http_${response.status}`, parsed.error_description);
    return parsed;
  }

  private async verifyIdToken(token: string, clientId: string, nonce: string): Promise<Record<string, unknown>> {
    const parts = token.split(".");
    if (parts.length !== 3) throw new Error("not a JWT");
    const header = JSON.parse(Buffer.from(parts[0], "base64url").toString("utf8")) as { alg?: string; kid?: string };
    const claims = JSON.parse(Buffer.from(parts[1], "base64url").toString("utf8")) as Record<string, unknown>;
    if (header.alg !== "RS256" && header.alg !== "ES256") throw new Error(`unsupported signing algorithm ${header.alg ?? "none"}`);

    const jwksUri = (await this.discover()).jwks_uri;
    if (!jwksUri) throw new Error("OpenAI publishes no JWKS");
    const response = await this.fetchImpl(jwksUri, { signal: AbortSignal.timeout(15_000) });
    if (!response.ok) throw new Error(`JWKS answered ${response.status}`);
    const keys = ((await response.json()) as { keys?: Array<JsonWebKey & { kid?: string }> }).keys ?? [];
    const jwk = keys.find((k) => k.kid === header.kid) ?? (keys.length === 1 && !header.kid ? keys[0] : undefined);
    if (!jwk) throw new Error("signing key not found");
    const ok = verifySignature(
      "sha256",
      Buffer.from(`${parts[0]}.${parts[1]}`),
      header.alg === "ES256" ? { key: createPublicKey({ key: jwk, format: "jwk" }), dsaEncoding: "ieee-p1363" } : createPublicKey({ key: jwk, format: "jwk" }),
      Buffer.from(parts[2], "base64url"),
    );
    if (!ok) throw new Error("bad signature");

    if (claims.iss !== this.issuer) throw new Error("wrong issuer");
    const audience = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
    if (!audience.includes(clientId)) throw new Error("wrong audience");
    if (typeof claims.exp !== "number" || claims.exp * 1000 < this.now() - 60_000) throw new Error("expired");
    if (claims.nonce !== nonce) throw new Error("nonce mismatch");
    if (typeof claims.sub !== "string" || !claims.sub) throw new Error("no subject");
    return claims;
  }
}

class TokenError extends Error {
  constructor(
    readonly code: string,
    description?: string,
  ) {
    super(description ? `${code}: ${description}` : code);
  }
}

function base64url(buffer: Buffer): string {
  return buffer.toString("base64url");
}

function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
