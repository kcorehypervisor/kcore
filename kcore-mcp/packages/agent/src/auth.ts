import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { ChatGPTAuth } from "./chatgpt.js";
import { Anthropic } from "./providers/anthropic.js";
import { OpenAICompatible } from "./providers/openai.js";
import { OpenAIResponses } from "./providers/openai-responses.js";
import type { Provider } from "./providers/types.js";

type ProviderDef = {
  id: string;
  kind: "openai-compatible" | "anthropic";
  label: string;
  note?: string;
  baseUrl: string;
  envKey: string;
  fallbackModels: string[];
};

export const PROVIDERS: ProviderDef[] = [
  { id: "openai", kind: "openai-compatible", label: "OpenAI API key", baseUrl: "https://api.openai.com/v1", envKey: "OPENAI_API_KEY", fallbackModels: ["gpt-4.1"] },
  {
    id: "anthropic",
    kind: "anthropic",
    label: "Anthropic API key",
    note: "API key only. Anthropic does not allow Claude.ai logins in other apps.",
    baseUrl: "https://api.anthropic.com/v1",
    envKey: "ANTHROPIC_API_KEY",
    fallbackModels: ["claude-sonnet-4-5"],
  },
  { id: "deepseek", kind: "openai-compatible", label: "DeepSeek API key", note: "API key only. DeepSeek has no sign-in for other apps.", baseUrl: "https://api.deepseek.com", envKey: "DEEPSEEK_API_KEY", fallbackModels: ["deepseek-chat"] },
];

export const CHATGPT_ID = "chatgpt";

export type ProviderInfo = {
  id: string;
  label: string;
  kind: "api" | "oauth";
  note?: string;
  connected: boolean;
  source?: "file" | "env";
  account?: string;
  pending?: boolean;
  error?: string;
  returning?: boolean;
  defaultModel: string;
};

type AuthFile = Record<string, { type: "api"; key: string }>;

export class AuthStore {
  private readonly path: string;
  private readonly fetchImpl: typeof fetch | undefined;
  private readonly env: NodeJS.ProcessEnv;
  readonly chatgpt: ChatGPTAuth;

  constructor(home: string, options: { fetch?: typeof fetch; env?: NodeJS.ProcessEnv; chatgptIssuer?: string } = {}) {
    this.path = join(home, "auth.json");
    this.fetchImpl = options.fetch;
    this.env = options.env ?? process.env;
    this.chatgpt = new ChatGPTAuth({ home, fetch: options.fetch, issuer: options.chatgptIssuer });
  }

  private read(): AuthFile {
    if (!existsSync(this.path)) return {};
    try {
      return JSON.parse(readFileSync(this.path, "utf8")) as AuthFile;
    } catch {
      return {};
    }
  }

  setKey(id: string, key: string): void {
    if (!PROVIDERS.some((p) => p.id === id)) throw new Error(`unknown provider ${id}`);
    const trimmed = key.trim();
    if (!trimmed) throw new Error("key is empty");
    const file = this.read();
    file[id] = { type: "api", key: trimmed };
    mkdirSync(dirname(this.path), { recursive: true, mode: 0o700 });
    writeFileSync(this.path, JSON.stringify(file, null, 2), { mode: 0o600 });
    chmodSync(this.path, 0o600);
  }

  removeKey(id: string): void {
    const file = this.read();
    delete file[id];
    writeFileSync(this.path, JSON.stringify(file, null, 2), { mode: 0o600 });
  }

  private keyFor(def: ProviderDef): { key: string; source: "file" | "env" } | undefined {
    const stored = this.read()[def.id];
    if (stored?.key) return { key: stored.key, source: "file" };
    const fromEnv = this.env[def.envKey]?.trim();
    return fromEnv ? { key: fromEnv, source: "env" } : undefined;
  }

  /** What the browser may know: never a key or a token. */
  describe(): ProviderInfo[] {
    const chatgpt = this.chatgpt.status();
    const chatgptInfo: ProviderInfo = {
      id: CHATGPT_ID,
      label: "ChatGPT plan",
      kind: "oauth",
      note: "Uses your ChatGPT plan through Sign in with ChatGPT. Not available on every plan; use an OpenAI key if sign-in is refused.",
      connected: chatgpt.signedIn,
      account: chatgpt.email,
      pending: chatgpt.pending,
      error: chatgpt.error,
      returning: chatgpt.returning,
      defaultModel: "gpt-5",
    };
    const keyed = PROVIDERS.map((def): ProviderInfo => {
      const found = this.keyFor(def);
      return { id: def.id, label: def.label, kind: "api", note: def.note, connected: Boolean(found), source: found?.source, defaultModel: def.fallbackModels[0] ?? "" };
    });
    return [chatgptInfo, ...keyed];
  }

  provider(id: string): Provider | undefined {
    if (id === CHATGPT_ID) {
      if (!this.chatgpt.status().signedIn) return undefined;
      return new OpenAIResponses({ id: CHATGPT_ID, token: (force) => this.chatgpt.accessToken(force), fetch: this.fetchImpl });
    }
    const def = PROVIDERS.find((p) => p.id === id);
    const found = def ? this.keyFor(def) : undefined;
    if (!def || !found) return undefined;
    const baseUrl = this.env[`${def.id.toUpperCase()}_BASE_URL`]?.trim() || def.baseUrl;
    if (def.kind === "anthropic") return new Anthropic({ baseUrl, apiKey: found.key, fetch: this.fetchImpl });
    return new OpenAICompatible({ id: def.id, baseUrl, apiKey: found.key, fetch: this.fetchImpl, fallbackModels: def.fallbackModels });
  }
}
