import { createRegistry, type DescEnum, type DescField, type DescMessage, type Message } from "@bufbuild/protobuf";
import { file_google_protobuf_timestamp, timestampDate, type Timestamp } from "@bufbuild/protobuf/wkt";
import { file_controller } from "./gen/controller_pb";

export type Row = Record<string, unknown>;

const prefixes = new WeakMap<DescEnum, string>();

/** The `WORD_` run every value name starts with, such as `CLUSTER_UPDATE_STRATEGY_`. */
function sharedPrefix(desc: DescEnum): string {
  const cached = prefixes.get(desc);
  if (cached !== undefined) return cached;
  let prefix = "";
  const names = desc.values.map((item) => item.name);
  if (names.length > 1) {
    const words = names[0].split("_");
    for (let count = words.length - 1; count > 0; count -= 1) {
      const candidate = `${words.slice(0, count).join("_")}_`;
      if (names.every((name) => name.startsWith(candidate) && name.length > candidate.length)) {
        prefix = candidate;
        break;
      }
    }
  }
  prefixes.set(desc, prefix);
  return prefix;
}

function shortName(desc: DescEnum, name: string): string {
  return name.slice(sharedPrefix(desc).length).toLowerCase().replace(/_/g, "-");
}

/** `VM_STATE_RUNNING` reads as `running`, `CLUSTER_ADMIN` as `cluster-admin`. */
export function enumLabel(desc: DescEnum, value: number): string {
  const found = desc.values.find((item) => item.number === value);
  if (!found) return String(value);
  return shortName(desc, found.name);
}

/** Parses the labels shown in forms back to enum numbers. */
export function enumValue(desc: DescEnum, text: string): number | undefined {
  const needle = text.trim().toLowerCase().replace(/_/g, "-");
  if (!needle) return undefined;
  const found = desc.values.find((item) => shortName(desc, item.name) === needle || item.name.toLowerCase().replace(/_/g, "-") === needle);
  if (found) return found.number;
  const parsed = Number(needle);
  return Number.isInteger(parsed) && desc.values.some((item) => item.number === parsed) ? parsed : undefined;
}

function scalar(value: unknown): unknown {
  if (typeof value === "bigint") {
    const asNumber = Number(value);
    return Number.isSafeInteger(asNumber) ? asNumber : value.toString();
  }
  if (value instanceof Uint8Array) return `${value.length} bytes`;
  return value;
}

function messageValue(desc: DescMessage, value: unknown): unknown {
  if (value === undefined || value === null) return null;
  if (desc.typeName === "google.protobuf.Timestamp") {
    return timestampDate(value as Timestamp).toISOString();
  }
  return toRow(desc, value as Message);
}

function fieldValue(field: DescField, value: unknown): unknown {
  switch (field.fieldKind) {
    case "scalar":
      return scalar(value);
    case "enum":
      return enumLabel(field.enum, value as number);
    case "message":
      return messageValue(field.message, value);
    case "list": {
      const items = (value as unknown[] | undefined) ?? [];
      if (field.listKind === "enum") return items.map((item) => enumLabel(field.enum, item as number));
      if (field.listKind === "message") return items.map((item) => messageValue(field.message, item));
      return items.map(scalar);
    }
    case "map": {
      const out: Record<string, unknown> = {};
      for (const [key, item] of Object.entries((value as Record<string, unknown> | undefined) ?? {})) {
        if (field.mapKind === "enum") out[key] = enumLabel(field.enum, item as number);
        else if (field.mapKind === "message") out[key] = messageValue(field.message, item);
        else out[key] = scalar(item);
      }
      return out;
    }
  }
}

/**
 * A plain row keyed by proto field names, the same names `kctl -o json` and
 * the Terraform provider show.
 */
export function toRow(desc: DescMessage, message: Message): Row {
  const row: Row = {};
  const record = message as unknown as Record<string, unknown>;
  for (const field of desc.fields) {
    let value: unknown;
    if (field.oneof) {
      const group = record[field.oneof.localName] as { case?: string; value?: unknown } | undefined;
      if (group?.case !== field.localName) continue;
      value = group.value;
    } else {
      value = record[field.localName];
    }
    row[field.name] = fieldValue(field, value);
  }
  return row;
}

export function toRows(desc: DescMessage, messages: readonly Message[]): Row[] {
  return messages.map((message) => toRow(desc, message));
}

const registry = createRegistry(file_controller, file_google_protobuf_timestamp);

function schemaOf(message: Message): DescMessage {
  const desc = registry.getMessage(message.$typeName);
  if (!desc) throw new Error(`unknown message ${message.$typeName}`);
  return desc;
}

/** One row for a whole response, such as GetClusterHealth. */
export function rowOf(message: Message): Row {
  return toRow(schemaOf(message), message);
}

/** Rows from a repeated message field of a response, named as in the proto. */
export function rowsOf(message: Message, field: string): Row[] {
  const desc = schemaOf(message);
  const found = desc.fields.find((item) => item.name === field);
  if (!found || found.fieldKind !== "list" || found.listKind !== "message") {
    throw new Error(`${desc.typeName} has no repeated message field ${field}`);
  }
  const items = (message as unknown as Record<string, Message[]>)[found.localName] ?? [];
  return toRows(found.message, items);
}

/**
 * Returns a mapper that lifts the scalars of nested records, such as `spec`
 * and `status`, to the top so tables can show `phase` next to `name`. The
 * nested records stay for the detail drawer.
 */
export function lift(...keys: string[]): (row: Row) => Row {
  return (row) => {
    const out: Row = { ...row };
    for (const key of keys) {
      const nested = row[key];
      if (!nested || typeof nested !== "object" || Array.isArray(nested)) continue;
      for (const [name, value] of Object.entries(nested as Row)) {
        if (name in out) continue;
        if (value !== null && typeof value === "object" && !Array.isArray(value)) continue;
        out[name] = value;
      }
    }
    return out;
  };
}

export const flatten = lift("status", "spec");
