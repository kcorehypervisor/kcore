export type FormSchema = {
  type?: string;
  properties?: Record<string, FormProperty>;
  required?: string[];
};

export type FormProperty = {
  type?: string;
  title?: string;
  description?: string;
  enum?: unknown[];
  enumNames?: string[];
  oneOf?: Array<{ const: unknown; title?: string }>;
  format?: string;
  minimum?: number;
  maximum?: number;
  default?: unknown;
};

export function choicesOf(property: FormProperty): Array<{ value: string; label: string }> | undefined {
  if (Array.isArray(property.oneOf)) return property.oneOf.map((o) => ({ value: String(o.const), label: o.title ?? String(o.const) }));
  if (Array.isArray(property.enum)) return property.enum.map((v, i) => ({ value: String(v), label: property.enumNames?.[i] ?? String(v) }));
  return undefined;
}

export type FormContent = Record<string, string | number | boolean>;

/** An approval is a form whose only field is a yes/no `confirm`. */
export function isApproval(schema: FormSchema): boolean {
  const names = Object.keys(schema.properties ?? {});
  if (names.length !== 1 || names[0] !== "confirm") return false;
  const choices = schema.properties?.confirm?.enum;
  return Array.isArray(choices) && choices.includes("yes") && choices.includes("no");
}

export function validateForm(schema: FormSchema, input: unknown): { ok: true; content: FormContent } | { ok: false; error: string } {
  if (!input || typeof input !== "object" || Array.isArray(input)) return { ok: false, error: "form content must be an object" };
  const given = input as Record<string, unknown>;
  const properties = schema.properties ?? {};
  const content: FormContent = {};

  for (const name of Object.keys(given)) {
    if (!(name in properties)) return { ok: false, error: `unknown field ${name}` };
  }
  for (const [name, property] of Object.entries(properties)) {
    const value = given[name];
    const label = property.title ?? name;
    if (value === undefined || value === null || (typeof value === "string" && value.trim() === "")) {
      if ((schema.required ?? []).includes(name)) return { ok: false, error: `${label} is required` };
      continue;
    }
    if (property.type === "number" || property.type === "integer") {
      const number = typeof value === "number" ? value : Number(value);
      if (!Number.isFinite(number)) return { ok: false, error: `${label} must be a number` };
      if (property.type === "integer" && !Number.isInteger(number)) return { ok: false, error: `${label} must be a whole number` };
      if (property.minimum !== undefined && number < property.minimum) return { ok: false, error: `${label} must be at least ${property.minimum}` };
      if (property.maximum !== undefined && number > property.maximum) return { ok: false, error: `${label} must be at most ${property.maximum}` };
      content[name] = number;
    } else if (property.type === "boolean") {
      if (typeof value !== "boolean") return { ok: false, error: `${label} must be true or false` };
      content[name] = value;
    } else {
      if (typeof value !== "string") return { ok: false, error: `${label} must be text` };
      const choices = choicesOf(property);
      if (choices && !choices.some((c) => c.value === value)) return { ok: false, error: `${label} must be one of: ${choices.map((c) => c.value).join(", ")}` };
      content[name] = value;
    }
  }
  return { ok: true, content };
}
