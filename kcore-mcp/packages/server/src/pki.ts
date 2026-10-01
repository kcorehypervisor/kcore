import { randomBytes, X509Certificate } from "node:crypto";
import { chmod, mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { isIP } from "node:net";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import {
  BasicConstraintsExtension,
  ExtendedKeyUsageExtension,
  KeyUsageFlags,
  KeyUsagesExtension,
  SubjectAlternativeNameExtension,
  X509CertificateGenerator,
  cryptoProvider,
  type Extension,
} from "@peculiar/x509";
import { parse as parseYaml, stringify as stringifyYaml } from "yaml";
import { InputError, assertAddress, assertArg, assertName } from "./names.js";
import type { ConnectionOptions } from "./connection.js";

const DAY = 24 * 60 * 60 * 1000;
const CA_DAYS = 3650;
const SUB_CA_DAYS = 1825;
const LEAF_DAYS = 365;
const SIGNING = { name: "ECDSA", namedCurve: "P-256", hash: "SHA-256" } as const;
const CLIENT_AUTH = "1.3.6.1.5.5.7.3.2";
const SERVER_AUTH = "1.3.6.1.5.5.7.3.1";

cryptoProvider.set(globalThis.crypto);

export type ClusterMaterial = {
  context: string;
  controller: string;
  configPath: string;
  certsDir: string;
  files: string[];
};

type ContextEntry = Record<string, unknown>;
type ConfigFile = {
  "current-context"?: string;
  contexts?: Record<string, ContextEntry>;
};

export async function createClusterContext(spec: Record<string, unknown>, options: ConnectionOptions = {}): Promise<ClusterMaterial> {
  const controller = assertAddress(required(spec.controller, "controller address"), "controller");
  const context = assertName(typeof spec.name === "string" && spec.name.trim() ? spec.name.trim() : "default", "context name");
  const force = spec.force === true || spec.force === "yes" || spec.force === "true";
  const host = hostOf(controller);
  const configPath = options.config?.trim() || process.env.KCORE_CONFIG?.trim() || join(homedir(), ".kcore", "config");
  const certsDir = spec.certsDir ? assertArg(String(spec.certsDir), "certs directory") : join(homedir(), ".kcore", context);
  const config = await loadConfig(configPath);
  if (!force && hasCredentials(config.contexts?.[context])) {
    throw new InputError(
      `Context '${context}' already has TLS credentials in ${configPath}. Generating a new CA would replace that trust root. Set force to yes only when that is intended.`,
    );
  }

  const ca = await selfSigned("kcore-cluster-ca", CA_DAYS, [
    new BasicConstraintsExtension(true, undefined, true),
    new KeyUsagesExtension(KeyUsageFlags.keyCertSign | KeyUsageFlags.cRLSign, true),
  ]);
  const sub = await signedBy(ca, "kcore-cluster-sub-ca", SUB_CA_DAYS, [
    new BasicConstraintsExtension(true, 0, true),
    new KeyUsagesExtension(KeyUsageFlags.keyCertSign | KeyUsageFlags.cRLSign, true),
  ]);
  const controllerCert = await signedBy(ca, `kcore-controller-${host}`, LEAF_DAYS, [
    new BasicConstraintsExtension(false, undefined, true),
    new KeyUsagesExtension(KeyUsageFlags.digitalSignature | KeyUsageFlags.keyEncipherment, true),
    new ExtendedKeyUsageExtension([SERVER_AUTH, CLIENT_AUTH], true),
    san(host),
  ]);
  const client = await signedBy(ca, "kctl", LEAF_DAYS, [
    new BasicConstraintsExtension(false, undefined, true),
    new KeyUsagesExtension(KeyUsageFlags.digitalSignature, true),
    new ExtendedKeyUsageExtension([CLIENT_AUTH], true),
  ]);

  const files: Array<[string, string, number]> = [
    ["ca.crt", ca.cert, 0o644],
    ["ca.key", ca.key, 0o600],
    ["sub-ca.crt", sub.cert, 0o644],
    ["sub-ca.key", sub.key, 0o600],
    ["controller.crt", controllerCert.cert, 0o644],
    ["controller.key", controllerCert.key, 0o600],
    ["kctl.crt", client.cert, 0o644],
    ["kctl.key", client.key, 0o600],
  ];
  await mkdir(certsDir, { recursive: true, mode: 0o700 });
  if (!force) {
    for (const [name] of files) {
      try {
        await readFile(join(certsDir, name));
        throw new InputError(`Certificates already exist in ${certsDir}. Set force to yes to replace them.`);
      } catch (error) {
        if (error instanceof InputError) throw error;
      }
    }
  }
  for (const [name, pem, mode] of files) {
    const path = join(certsDir, name);
    await writeFile(path, pem, { mode });
    await chmod(path, mode);
  }

  config.contexts ??= {};
  config.contexts[context] = {
    controller,
    controllers: [controller],
    insecure: false,
    "ca-data": Buffer.from(ca.cert, "utf8").toString("base64"),
    "cert-data": Buffer.from(client.cert, "utf8").toString("base64"),
    "key-data": Buffer.from(client.key, "utf8").toString("base64"),
  };
  config["current-context"] = context;
  await mkdir(dirname(configPath), { recursive: true, mode: 0o700 });
  const tmp = `${configPath}.${process.pid}.tmp`;
  await writeFile(tmp, stringifyYaml(config), { mode: 0o600 });
  await chmod(tmp, 0o600);
  await rename(tmp, configPath);

  const issued = new X509Certificate(client.cert);
  const root = new X509Certificate(ca.cert);
  if (!issued.checkIssued(root) || !issued.subject.includes("kctl")) {
    throw new InputError("generated client certificate does not chain to the new cluster CA");
  }

  return {
    context,
    controller,
    configPath,
    certsDir,
    files: files.map(([name]) => name),
  };
}

type Material = { name: string; cert: string; key: string; publicKey: CryptoKey; privateKey: CryptoKey };

async function selfSigned(commonName: string, days: number, extensions: Extension[]): Promise<Material> {
  const keys = await generate();
  const certificate = await X509CertificateGenerator.createSelfSigned({
    serialNumber: serial(),
    name: `CN=${commonName}`,
    notBefore: new Date(),
    notAfter: new Date(Date.now() + days * DAY),
    signingAlgorithm: SIGNING,
    keys,
    extensions,
  });
  return pack(`CN=${commonName}`, certificate, keys);
}

async function signedBy(issuer: Material, commonName: string, days: number, extensions: Extension[]): Promise<Material> {
  const keys = await generate();
  const certificate = await X509CertificateGenerator.create({
    serialNumber: serial(),
    subject: `CN=${commonName}`,
    issuer: issuer.name,
    notBefore: new Date(),
    notAfter: new Date(Date.now() + days * DAY),
    signingAlgorithm: SIGNING,
    publicKey: keys.publicKey,
    signingKey: issuer.privateKey,
    extensions,
  });
  return pack(`CN=${commonName}`, certificate, keys);
}

async function generate(): Promise<CryptoKeyPair> {
  return crypto.subtle.generateKey(SIGNING, true, ["sign", "verify"]);
}

async function pack(name: string, certificate: { toString(format: "pem"): string }, keys: CryptoKeyPair): Promise<Material> {
  const pkcs8 = await crypto.subtle.exportKey("pkcs8", keys.privateKey);
  return {
    name,
    cert: certificate.toString("pem"),
    key: pem("PRIVATE KEY", Buffer.from(pkcs8)),
    publicKey: keys.publicKey,
    privateKey: keys.privateKey,
  };
}

function pem(label: string, body: Buffer): string {
  const lines = body.toString("base64").match(/.{1,64}/g) ?? [];
  return `-----BEGIN ${label}-----\n${lines.join("\n")}\n-----END ${label}-----\n`;
}

function serial(): string {
  const hex = randomBytes(16).toString("hex").replace(/^0+/, "");
  return hex.length > 0 ? hex : "01";
}

function san(host: string): SubjectAlternativeNameExtension {
  const type = isIP(host) ? "ip" : "dns";
  return new SubjectAlternativeNameExtension([{ type, value: host }], false);
}

function hostOf(address: string): string {
  if (address.startsWith("[")) {
    const end = address.indexOf("]");
    if (end > 1) return address.slice(1, end);
  }
  const split = address.lastIndexOf(":");
  return split > 0 ? address.slice(0, split) : address;
}

async function loadConfig(path: string): Promise<ConfigFile> {
  try {
    const parsed = parseYaml(await readFile(path, "utf8"));
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      throw new InputError(`${path} is not a kcore config mapping. Refusing to replace it.`);
    }
    return parsed as ConfigFile;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return { contexts: {} };
    if (error instanceof InputError) throw error;
    throw new InputError(`could not read ${path}: ${error instanceof Error ? error.message : String(error)}. Refusing to replace it.`);
  }
}

function hasCredentials(entry: ContextEntry | undefined): boolean {
  if (!entry) return false;
  return ["ca-data", "cert-data", "key-data", "ca", "cert", "key"].some((key) => {
    const value = entry[key];
    return typeof value === "string" && value.trim().length > 0;
  });
}

function required(value: unknown, label: string): string {
  if (typeof value !== "string" || value.trim().length === 0) throw new InputError(`${label} is required`);
  return value.trim();
}
