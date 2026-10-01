import type { ResourceKind } from "./catalog.js";
import { isBlank } from "./names.js";

export type FieldChange = {
  field: string;
  from: unknown;
  to: unknown;
  mutable: boolean;
};

export type LocalPlan = {
  action: "create" | "update" | "unchanged" | "replace";
  changes: FieldChange[];
  summary: string;
};

export function diffSpec(
  kind: ResourceKind,
  desired: Record<string, unknown>,
  current: Record<string, unknown> | null | undefined,
): LocalPlan {
  if (current == null) {
    return {
      action: "create",
      changes: [],
      summary: `${kind.title} will be applied as a declarative upsert. The controller creates it, updates mutable fields, or rejects immutable changes.`,
    };
  }

  const changes: FieldChange[] = [];
  const fields = [...kind.mutable, ...kind.immutable];
  for (const field of fields) {
    if (isBlank(desired[field]) && isBlank(current[field])) continue;
    if (!same(desired[field], current[field]) && desired[field] !== undefined) {
      changes.push({
        field,
        from: current[field] ?? null,
        to: desired[field],
        mutable: kind.mutable.includes(field),
      });
    }
  }

  const immutable = changes.filter((change) => !change.mutable);
  if (immutable.length > 0) {
    return {
      action: "replace",
      changes,
      summary: `${kind.title} already exists and the change touches immutable fields (${immutable.map((change) => change.field).join(", ")}). Replacement deletes and recreates it. Ask the operator before doing that.`,
    };
  }
  if (changes.length === 0) {
    return {
      action: "unchanged",
      changes,
      summary: `${kind.title} already matches this spec.`,
    };
  }
  return {
    action: "update",
    changes,
    summary: `${kind.title} will update ${changes.map((change) => change.field).join(", ")}.`,
  };
}

function same(left: unknown, right: unknown): boolean {
  return JSON.stringify(left ?? null) === JSON.stringify(right ?? null);
}
