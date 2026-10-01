import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { parse as parseYaml } from "yaml";
import { InputError, assertAddress, assertArg, assertName } from "./names.js";

export type ConnectionOptions = {
  config?: string;
  controller?: string;
  insecure?: boolean;
  tlsServerName?: string;
  operator?: string;
  timeoutMs?: number;
};

export type ResolvedConnection = {
  address: string;
  insecure: boolean;
  tlsServerName?: string;
  ca?: Buffer;
  cert?: Buffer;
  key?: Buffer;
  timeoutMs: number;
};

type ContextFile = {
  "current-context"?: string;
  contexts?: Record<string, ContextEntry>;
};

type ContextEntry = {
  controller?: string;
  controllers?: string[];
  insecure?: boolean;
  "tls-server-name"?: string;
  "cert-data"?: string;
  "key-data"?: string;
  "ca-data"?: string;
  cert?: string;
  key?: string;
  ca?: string;
  operator?: string;
};

export function resolveConnection(options: ConnectionOptions = {}): ResolvedConnection {
  const configPath = options.config?.trim() || process.env.KCORE_CONFIG?.trim() || join(homedir(), ".kcore", "config");
  const file = readContext(configPath);
  const current = file?.["current-context"];
  const entry = current && file?.contexts ? file.contexts[current] : undefined;
  const address = options.controller?.trim() || entry?.controller || entry?.controllers?.[0] || "";
  if (!address) {
    throw new InputError(
      `No controller address. Set connection.controller or add a current context to ${configPath}.`,
    );
  }
  const insecure = options.insecure ?? entry?.insecure ?? false;
  const tlsServerName = options.tlsServerName?.trim() || entry?.["tls-server-name"]?.trim() || undefined;
  const operator = options.operator?.trim() || entry?.operator?.trim() || "";
  const material = insecure ? {} : loadMaterial(entry, operator, configPath);
  return {
    address: assertAddress(address, "controller"),
    insecure,
    tlsServerName: tlsServerName ? assertArg(tlsServerName, "tls server name") : undefined,
    ...material,
    timeoutMs: options.timeoutMs ?? 120_000,
  };
}

function readContext(path: string): ContextFile | undefined {
  try {
    const text = readFileSync(path, "utf8");
    const parsed = parseYaml(text);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return undefined;
    return parsed as ContextFile;
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === "ENOENT") return undefined;
    throw new InputError(`could not read ${path}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function loadMaterial(entry: ContextEntry | undefined, operator: string, configPath: string): Pick<ResolvedConnection, "ca" | "cert" | "key"> {
  if (!entry && !operator) {
    throw new InputError(
      `No client certificate in ${configPath}. The MCP dials the controller with the CA, client certificate, and key stored in that context.`,
    );
  }
  const ca = entry ? pemBuffer(entry["ca-data"], entry.ca, "CA") : undefined;
  if (operator) {
    if (entry?.["cert-data"] || entry?.["key-data"]) {
      throw new InputError("An operator identity cannot be combined with inline cert-data or key-data.");
    }
    const name = assertName(operator, "operator");
    const dir = join(homedir(), ".kcore", "operators", name);
    return {
      ca,
      cert: readRequired(join(dir, "operator.crt"), "operator certificate"),
      key: readRequired(join(dir, "operator.key"), "operator key"),
    };
  }
  return {
    ca,
    cert: pemBuffer(entry?.["cert-data"], entry?.cert, "client certificate"),
    key: pemBuffer(entry?.["key-data"], entry?.key, "client key"),
  };
}

function pemBuffer(inline: string | undefined, path: string | undefined, label: string): Buffer | undefined {
  if (inline && inline.trim()) return Buffer.from(inline.trim(), "base64");
  if (path && path.trim()) return readRequired(path.trim(), label);
  return undefined;
}

function readRequired(path: string, label: string): Buffer {
  try {
    return readFileSync(path);
  } catch (error) {
    throw new InputError(`could not read ${label} at ${path}: ${error instanceof Error ? error.message : String(error)}`);
  }
}
