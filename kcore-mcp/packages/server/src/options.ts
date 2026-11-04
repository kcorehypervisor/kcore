import type { OptionSourceId, Question } from "./advise.js";

export type ClusterOption = { value: string; label?: string };
export type LoadOptions = (source: OptionSourceId) => Promise<ClusterOption[]>;

const NOUN: Record<OptionSourceId, string> = {
  networks: "networks",
  "ssh-keys": "stored ssh keys",
  nodes: "nodes",
  vms: "VMs",
  "node-images": "images on the nodes",
};

/**
 * Fill questions from what the cluster already has. A restricted question becomes a pick list;
 * an unrestricted one keeps free text and lists the existing entries in its explanation.
 * A source that cannot be read leaves the question exactly as it was.
 */
export async function resolveQuestions(questions: Question[], load: LoadOptions): Promise<Question[]> {
  const cache = new Map<OptionSourceId, Promise<ClusterOption[] | undefined>>();
  const fetchOnce = (source: OptionSourceId) => {
    let pending = cache.get(source);
    if (!pending) {
      pending = load(source).then(
        (items) => items,
        () => undefined,
      );
      cache.set(source, pending);
    }
    return pending;
  };

  return Promise.all(
    questions.map(async (item) => {
      if (!item.source) return item;
      const found = await fetchOnce(item.source);
      if (!found) return item;
      const unique = dedupe(found);
      if (unique.length === 0) {
        const none = `No ${NOUN[item.source]} exist yet.`;
        return { ...item, why: `${item.why} ${none}` };
      }
      if (item.restrict && !item.choices) {
        return {
          ...item,
          choices: unique.map((o) => o.value),
          choiceLabels: unique.map((o) => o.label ?? o.value),
        };
      }
      const shown = unique.slice(0, 12).map((o) => o.value).join(", ");
      const more = unique.length > 12 ? `, and ${unique.length - 12} more` : "";
      return { ...item, why: `${item.why} Existing ${NOUN[item.source]}: ${shown}${more}.` };
    }),
  );
}

function dedupe(items: ClusterOption[]): ClusterOption[] {
  const seen = new Set<string>();
  const out: ClusterOption[] = [];
  for (const item of items) {
    if (!item.value || seen.has(item.value)) continue;
    seen.add(item.value);
    out.push(item);
  }
  return out;
}
