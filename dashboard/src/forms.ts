/** Form values merged over the clicked row. Forms hold strings and booleans. */
export type Values = Record<string, unknown>;

export class FormError extends Error {}

export function text(v: Values, key: string): string {
  const value = v[key];
  if (typeof value === "string") return value.trim();
  if (typeof value === "number" || typeof value === "boolean" || typeof value === "bigint") return String(value);
  return "";
}

export function first(v: Values, ...keys: string[]): string {
  for (const key of keys) {
    const value = text(v, key);
    if (value) return value;
  }
  return "";
}

export function flag(v: Values, key: string): boolean {
  const value = v[key];
  if (typeof value === "boolean") return value;
  if (typeof value === "number") return value === 1;
  if (typeof value === "string") return ["1", "true", "yes", "on"].includes(value.trim().toLowerCase());
  return false;
}

export function int(v: Values, key: string): number {
  const raw = text(v, key);
  if (!raw) return 0;
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || !Number.isInteger(parsed)) throw new FormError(`${key} must be a whole number`);
  return parsed;
}

export function i64(v: Values, key: string): bigint {
  const raw = text(v, key);
  if (!raw) return 0n;
  try {
    return BigInt(raw);
  } catch {
    throw new FormError(`${key} must be a whole number`);
  }
}

const UNITS: Record<string, bigint> = { "": 1n, k: 1n << 10n, m: 1n << 20n, g: 1n << 30n, t: 1n << 40n, p: 1n << 50n };

/** Bytes from "4294967296", "4G", "4 GiB" or "1.5 TiB". Units are binary, as in kctl. */
export function bytes(v: Values, key: string): bigint {
  const raw = text(v, key);
  if (!raw) return 0n;
  const match = raw.toLowerCase().match(/^(\d+)(?:\.(\d+))?\s*([kmgtp]?)(?:i?b)?$/);
  if (!match) throw new FormError(`${key} must be a size such as 4 GiB or 512 MiB`);
  const [, whole, fraction = "", unit] = match;
  const scale = 10n ** BigInt(fraction.length);
  const value = (BigInt(whole) * scale + BigInt(fraction || "0")) * UNITS[unit];
  if (value % scale !== 0n) throw new FormError(`${key} must be a whole number of bytes`);
  return value / scale;
}

export function strings(v: Values, key: string): string[] {
  const value = v[key];
  if (Array.isArray(value)) return value.map((item) => String(item).trim()).filter(Boolean);
  return text(v, key)
    .split(/[,\n]/)
    .map((part) => part.trim())
    .filter(Boolean);
}

export function ints(v: Values, key: string): number[] {
  return strings(v, key).map((part) => {
    const parsed = Number(part);
    if (!Number.isInteger(parsed)) throw new FormError(`${key} must be a list of whole numbers`);
    return parsed;
  });
}

export function required(v: Values, key: string): string {
  const value = text(v, key);
  if (!value) throw new FormError(`${key} is required`);
  return value;
}

export function positive(v: Values, key: string): bigint {
  const value = i64(v, key);
  if (value <= 0n) throw new FormError(`${key} must be greater than 0`);
  return value;
}

export function positiveBytes(v: Values, key: string): bigint {
  const value = bytes(v, key);
  if (value <= 0n) throw new FormError(`${key} must be greater than 0`);
  return value;
}

/** The row's name, falling back to its id. Most RPCs take the name. */
export function ident(v: Values): string {
  return first(v, "name", "id");
}

/** True when the values came from a listed row rather than a blank form. */
export function fromRow(v: Values): boolean {
  const spec = v.spec;
  return Boolean(spec) && typeof spec === "object" && !Array.isArray(spec);
}

export function records(v: Values, key: string): Values[] {
  const value = v[key];
  let items: unknown = value;
  if (typeof value === "string") {
    if (!value.trim()) return [];
    try {
      items = JSON.parse(value);
    } catch {
      throw new FormError(`${key} must be a JSON list`);
    }
  }
  if (!Array.isArray(items)) return [];
  return items.filter((item): item is Values => Boolean(item) && typeof item === "object" && !Array.isArray(item));
}

export function labels(v: Values, key = "labels"): Record<string, string> {
  const out: Record<string, string> = {};
  const value = v[key];
  if (value && typeof value === "object" && !Array.isArray(value)) {
    for (const [name, item] of Object.entries(value)) if (typeof item === "string") out[name] = item;
    return out;
  }
  for (const pair of strings(v, key)) {
    const at = pair.indexOf("=");
    if (at > 0) out[pair.slice(0, at).trim()] = pair.slice(at + 1).trim();
  }
  return out;
}

export function base64ToBytes(raw: string): Uint8Array {
  const clean = raw.replace(/\s+/g, "");
  const binary = atob(clean);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
}

export function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunk = 0x8000;
  for (let index = 0; index < bytes.length; index += chunk) {
    binary += String.fromCharCode(...bytes.subarray(index, index + chunk));
  }
  return btoa(binary);
}
