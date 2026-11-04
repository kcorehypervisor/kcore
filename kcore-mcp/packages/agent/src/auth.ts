import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { OpenAICompatible } from "./providers/openai.js";
import type { Provider } from "./providers/types.js";

type ProviderDef = {
  id: string;
  label: string;
  baseUrl: string;
  envKey: string;
  fallbackModels: string[];
};

export const PROVIDERS: ProviderDef[] = [
  { id: "openai", label: "OpenAI API key", baseUrl: "https://api.openai.com/v1", envKey: "OPENAI_API_KEY", fallbackModels: ["gpt-4.1"] },
  { id: "deepseek", label: "DeepSeek API key", baseUrl: "https://api.deepseek.com", envKey: "DEEPSEEK_API_KEY", fallbackModels: ["deepseek-chat"] },
];

type AuthFile = Record<string, { type: "api"; key: string }>;

export class AuthStore {
  private readonly path: string;
  private readonly fetchImpl: typeof fetch | undefined;
  private readonly env: NodeJS.ProcessEnv;

  constructor(home: string, options: { fetch?: typeof fetch; env?: NodeJS.ProcessEnv } = {}) {
    this.path = join(home, "auth.json");
    this.fetchImpl = options.fetch;
    this.env = options.env ?? process.env;
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

  /** What the browser may know: never the key itself. */
  describe(): Array<{ id: string; label: string; connected: boolean; source?: "file" | "env"; defaultModel: string }> {
    return PROVIDERS.map((def) => {
      const found = this.keyFor(def);
      return { id: def.id, label: def.label, connected: Boolean(found), source: found?.source, defaultModel: def.fallbackModels[0] ?? "" };
    });
  }

  provider(id: string): Provider | undefined {
    const def = PROVIDERS.find((p) => p.id === id);
    const found = def ? this.keyFor(def) : undefined;
    if (!def || !found) return undefined;
    const baseUrl = this.env[`${def.id.toUpperCase()}_BASE_URL`]?.trim() || def.baseUrl;
    return new OpenAICompatible({ id: def.id, baseUrl, apiKey: found.key, fetch: this.fetchImpl, fallbackModels: def.fallbackModels });
  }
}
