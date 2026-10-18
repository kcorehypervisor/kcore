export type Field = {
  name: string;
  label: string;
  kind: "text" | "number" | "bool" | "json";
  required: boolean;
};

export type Action = {
  name: string;
  label: string;
  confirm: boolean;
  client?: boolean;
  preload?: boolean;
  fields: Field[];
};

export type Resource = {
  id: string;
  title: string;
  group: string;
  terraform: string[];
  kctl: string;
  idField: string;
  list: boolean;
  creatable: boolean;
  inspect?: boolean;
  fields: Field[];
  actions: Action[];
};

export type Catalog = { resources: Resource[] };

export type Row = Record<string, unknown>;

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers);
  if (init?.body && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  const res = await fetch(path, { credentials: "same-origin", ...init, headers });
  if (!res.ok) {
    throw new Error((await res.text()) || res.statusText);
  }
  const type = res.headers.get("content-type") ?? "";
  if (type.includes("json")) return (await res.json()) as T;
  return (await res.text()) as T;
}

export function specToValues(fields: Field[], spec: Record<string, unknown>): Record<string, string | boolean> {
  const values: Record<string, string | boolean> = {};
  for (const field of fields) {
    const raw = spec[field.name];
    if (raw === undefined || raw === null) continue;
    if (field.kind === "bool") values[field.name] = Boolean(raw);
    else if (field.kind === "json") values[field.name] = JSON.stringify(raw);
    else values[field.name] = String(raw);
  }
  return values;
}

export function payload(fields: Field[], values: Record<string, string | boolean>): Record<string, unknown> {
  const body: Record<string, unknown> = {};
  for (const field of fields) {
    const raw = values[field.name];
    if (raw === undefined || raw === "") continue;
    if (field.kind === "number") body[field.name] = Number(raw);
    else if (field.kind === "bool") body[field.name] = Boolean(raw);
    else if (field.kind === "json") {
      const text = String(raw).trim();
      if (!text) continue;
      try {
        body[field.name] = JSON.parse(text);
      } catch {
        throw new Error(`${field.label} must be a JSON list`);
      }
    } else body[field.name] = raw;
  }
  return body;
}
