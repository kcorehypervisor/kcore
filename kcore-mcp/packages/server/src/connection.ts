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
  /** kcore-provisioning gRPC address, host:port. */
  provisioning?: string;
  /** Provisioning listens without TLS unless this is false. */
  provisioningInsecure?: boolean;
  /** Bearer token for provisioning or the vault. Sent as an Authorization header. */
  token?: string;
  /** Vault coordinator host:port. */
  vault?: string;
  /** Vault listens without TLS unless this is false. */
  vaultInsecure?: boolean;
};

export type ResolvedConnection = {
  address: string;
  insecure: boolean;
  tlsServerName?: string;
  ca?: Buffer;
  cert?: Buffer;
  key?: Buffer;
  timeoutMs: number;
  /** Value of the authorization metadata header, including the Bearer prefix. */
  authorization?: string;
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
  provisioning?: string;
  token?: string;
  vault?: string;
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

export function resolveProvisioning(options: ConnectionOptions = {}): ResolvedConnection {
  const configPath = options.config?.trim() || process.env.KCORE_CONFIG?.trim() || join(homedir(), ".kcore", "config");
  const file = readContext(configPath);
  const current = file?.["current-context"];
  const entry = current && file?.contexts ? file.contexts[current] : undefined;
  const address =
    options.provisioning?.trim() ||
    process.env.KCORE_PROVISIONING?.trim() ||
    entry?.provisioning?.trim() ||
    "";
  if (!address) {
    throw new InputError(
      "No provisioning controller address. Set connection.provisioning or KCORE_PROVISIONING to host:port.",
    );
  }
  const timeoutMs = options.timeoutMs ?? 120_000;
  const checked = assertAddress(address, "provisioning");
  const authorization = bearer(options.token?.trim() || process.env.KCORE_PROVISIONING_TOKEN?.trim() || entry?.token?.trim());
  if (options.provisioningInsecure === false) {
    const base = resolveConnection(options);
    return { ...base, address: checked, insecure: false, timeoutMs, authorization };
  }
  return { address: checked, insecure: true, timeoutMs, authorization };
}

export function resolveVault(options: ConnectionOptions = {}): ResolvedConnection {
  const configPath = options.config?.trim() || process.env.KCORE_CONFIG?.trim() || join(homedir(), ".kcore", "config");
  const file = readContext(configPath);
  const current = file?.["current-context"];
  const entry = current && file?.contexts ? file.contexts[current] : undefined;
  const raw =
    options.vault?.trim() ||
    process.env.KCORE_VAULT_ENDPOINT?.trim() ||
    entry?.vault?.trim() ||
    "127.0.0.1:9191";
  const checked = assertAddress(raw.replace(/^https?:\/\//, ""), "vault");
  const timeoutMs = options.timeoutMs ?? 120_000;
  const authorization = bearer(options.token?.trim() || process.env.KCORE_VAULT_TOKEN?.trim() || entry?.token?.trim());
  const insecure = options.vaultInsecure ?? process.env.KCORE_VAULT_TLS !== "1";
  if (!insecure) {
    const base = resolveConnection(options);
    return { ...base, address: checked, insecure: false, timeoutMs, authorization };
  }
  return { address: checked, insecure: true, timeoutMs, authorization };
}

function bearer(token: string | undefined): string | undefined {
  if (!token) return undefined;
  return /^bearer\s+/i.test(token) ? token : `Bearer ${token}`;
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
  const material = inline && inline.trim()
    ? Buffer.from(inline.trim(), "base64")
    : path && path.trim()
      ? readRequired(path.trim(), label)
      : undefined;
  if (!material) return undefined;
  if (!material.toString("utf8").includes("-----BEGIN")) {
    throw new InputError(
      `${label} in the kcore context is not a PEM. cert-data, key-data, and ca-data must be base64-encoded PEM, the same encoding kctl writes.`,
    );
  }
  return material;
}

function readRequired(path: string, label: string): Buffer {
  try {
    return readFileSync(path);
  } catch (error) {
    throw new InputError(`could not read ${label} at ${path}: ${error instanceof Error ? error.message : String(error)}`);
  }
}
