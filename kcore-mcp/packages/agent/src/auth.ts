import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
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
  /** Where the operator signs in and creates a key. */
  keyUrl: string;
  baseUrl: string;
  baseUrlEnv: string;
  modelsUrl?: string;
  envKey: string;
  fallbackModels: string[];
  /** The id OpenCode uses for the same provider in its auth.json. */
  openCodeId: string;
};

export const PROVIDERS: ProviderDef[] = [
  {
    id: "openai",
    kind: "openai-compatible",
    label: "OpenAI API key",
    keyUrl: "https://platform.openai.com/api-keys",
    baseUrl: "https://api.openai.com/v1",
    baseUrlEnv: "OPENAI_BASE_URL",
    envKey: "OPENAI_API_KEY",
    fallbackModels: ["gpt-4.1"],
    openCodeId: "openai",
  },
  {
    id: "anthropic",
    kind: "anthropic",
    label: "Claude (Anthropic API key)",
    note: "Needs a Claude Console API key. Anthropic does not allow Claude.ai or Claude subscription logins in other apps. To use Claude without your own Console account, connect OpenCode below and pick a claude model.",
    keyUrl: "https://platform.claude.com/settings/keys",
    baseUrl: "https://api.anthropic.com/v1",
    baseUrlEnv: "ANTHROPIC_BASE_URL",
    envKey: "ANTHROPIC_API_KEY",
    fallbackModels: ["claude-sonnet-4-5"],
    openCodeId: "anthropic",
  },
  {
    id: "deepseek",
    kind: "openai-compatible",
    label: "DeepSeek API key",
    note: "API key only. DeepSeek has no sign-in for other apps; the link opens its key page.",
    keyUrl: "https://platform.deepseek.com/api_keys",
    baseUrl: "https://api.deepseek.com",
    baseUrlEnv: "DEEPSEEK_BASE_URL",
    envKey: "DEEPSEEK_API_KEY",
    fallbackModels: ["deepseek-chat"],
    openCodeId: "deepseek",
  },
  {
    id: "opencode",
    kind: "openai-compatible",
    label: "OpenCode Console (pay as you go)",
    note: "One key for Claude, GPT, DeepSeek, Gemini, Kimi and others, billed by OpenCode. Models are listed by what your key can use.",
    keyUrl: "https://opencode.ai/auth",
    baseUrl: "https://opencode.ai/inference/openai/v1",
    baseUrlEnv: "OPENCODE_BASE_URL",
    modelsUrl: "https://opencode.ai/inference/v1/models",
    envKey: "OPENCODE_API_KEY",
    fallbackModels: ["claude-sonnet-5-5"],
    openCodeId: "opencode",
  },
  {
    id: "opencode-go",
    kind: "openai-compatible",
    label: "OpenCode Go (subscription)",
    note: "The OpenCode Go plan: open models such as DeepSeek, Kimi and GLM with the usage the plan includes. Use a key from a workspace that has Go.",
    keyUrl: "https://opencode.ai/auth",
    baseUrl: "https://opencode.ai/inference/go/openai/v1",
    baseUrlEnv: "OPENCODE_GO_BASE_URL",
    modelsUrl: "https://opencode.ai/inference/go/v1/models",
    envKey: "OPENCODE_GO_API_KEY",
    fallbackModels: ["deepseek-v4-pro"],
    openCodeId: "opencode-go",
  },
];

export const CHATGPT_ID = "chatgpt";

export type OpenCodeScan = {
  found: boolean;
  /** Plain API keys OpenCode holds for providers we support. Nothing secret. */
  importable: Array<{ id: string; label: string; stored: boolean }>;
  /** Logins we leave alone, with the reason. */
  skipped: Array<{ openCodeId: string; reason: string }>;
};

export type ProviderInfo = {
  id: string;
  label: string;
  kind: "api" | "oauth";
  note?: string;
  keyUrl?: string;
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

  private readonly openCodePath: string;

  constructor(
    home: string,
    options: { fetch?: typeof fetch; env?: NodeJS.ProcessEnv; chatgptIssuer?: string; openCodeAuthPath?: string } = {},
  ) {
    this.path = join(home, "auth.json");
    const env = options.env ?? process.env;
    const dataHome = env.XDG_DATA_HOME?.trim() || join(homedir(), ".local", "share");
    this.openCodePath = options.openCodeAuthPath ?? join(dataHome, "opencode", "auth.json");
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

  private readOpenCode(): Record<string, { type?: string; key?: string }> | undefined {
    if (!existsSync(this.openCodePath)) return undefined;
    try {
      const parsed = JSON.parse(readFileSync(this.openCodePath, "utf8")) as unknown;
      return parsed && typeof parsed === "object" ? (parsed as Record<string, { type?: string; key?: string }>) : undefined;
    } catch {
      return undefined;
    }
  }

  /** Looks at the operator's OpenCode login file without returning any secret. */
  scanOpenCode(): OpenCodeScan {
    const entries = this.readOpenCode();
    if (!entries) return { found: false, importable: [], skipped: [] };
    const stored = this.read();
    const importable: OpenCodeScan["importable"] = [];
    const skipped: OpenCodeScan["skipped"] = [];
    for (const [openCodeId, entry] of Object.entries(entries)) {
      const def = PROVIDERS.find((p) => p.openCodeId === openCodeId);
      if (!def) continue;
      if (entry?.type === "api" && typeof entry.key === "string" && entry.key.trim()) {
        importable.push({ id: def.id, label: def.label, stored: Boolean(stored[def.id]?.key) });
      } else {
        skipped.push({ openCodeId, reason: "Signed in with an account, not a key. That login belongs to OpenCode." });
      }
    }
    return { found: true, importable, skipped };
  }

  /** Copies OpenCode API keys we do not already hold. Account logins are never copied. */
  importOpenCode(): string[] {
    const entries = this.readOpenCode();
    if (!entries) return [];
    const imported: string[] = [];
    for (const def of PROVIDERS) {
      const entry = entries[def.openCodeId];
      if (entry?.type !== "api" || typeof entry.key !== "string" || !entry.key.trim()) continue;
      if (this.read()[def.id]?.key) continue;
      this.setKey(def.id, entry.key);
      imported.push(def.id);
    }
    return imported;
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
      return { id: def.id, label: def.label, kind: "api", note: def.note, keyUrl: def.keyUrl, connected: Boolean(found), source: found?.source, defaultModel: def.fallbackModels[0] ?? "" };
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
    const baseUrl = this.env[def.baseUrlEnv]?.trim() || def.baseUrl;
    if (def.kind === "anthropic") return new Anthropic({ baseUrl, apiKey: found.key, fetch: this.fetchImpl });
    return new OpenAICompatible({
      id: def.id,
      baseUrl,
      apiKey: found.key,
      fetch: this.fetchImpl,
      fallbackModels: def.fallbackModels,
      // A redirected base URL (a local gateway, a test server) keeps the standard /models path.
      modelsUrl: this.env[def.baseUrlEnv]?.trim() ? undefined : def.modelsUrl,
    });
  }
}
