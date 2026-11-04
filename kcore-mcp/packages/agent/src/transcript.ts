import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import type { ChatMessage } from "./providers/types.js";
import type { AgentEvent } from "./session.js";

export type TranscriptRecord =
  | { k: "meta"; createdAt: number; title: string }
  | { k: "event"; e: AgentEvent }
  | { k: "message"; m: ChatMessage };

export type Restored = {
  id: string;
  createdAt: number;
  title: string;
  events: AgentEvent[];
  messages: ChatMessage[];
};

const INTERRUPTED = "The agent restarted before this call finished. Its result is unknown: check the cluster before retrying.";

/** Rebuilds a session from its jsonl log. A torn last line from a crash is skipped. */
export function readTranscript(dir: string, id: string): Restored | undefined {
  const path = join(dir, `${id}.jsonl`);
  if (!existsSync(path)) return undefined;
  let createdAt = 0;
  let title = "New session";
  const events: AgentEvent[] = [];
  const messages: ChatMessage[] = [];
  for (const line of readFileSync(path, "utf8").split("\n")) {
    if (!line.trim()) continue;
    let record: TranscriptRecord;
    try {
      record = JSON.parse(line) as TranscriptRecord;
    } catch {
      continue;
    }
    if (record.k === "meta") {
      createdAt = record.createdAt;
      title = record.title;
    } else if (record.k === "event") events.push(record.e);
    else if (record.k === "message") messages.push(record.m);
  }
  if (events.length === 0 && messages.length === 0) return undefined;
  return { id, createdAt: createdAt || events[0]?.ts || Date.now(), title, events, messages: repairMessages(messages) };
}

export function listTranscripts(dir: string): string[] {
  if (!existsSync(dir)) return [];
  return readdirSync(dir)
    .map((name) => /^([0-9a-f-]{36})\.jsonl$/.exec(name)?.[1])
    .filter((id): id is string => Boolean(id));
}

/** A model API refuses an assistant tool call with no matching result, so close any a crash left open. */
export function repairMessages(messages: ChatMessage[]): ChatMessage[] {
  const out: ChatMessage[] = [];
  for (let i = 0; i < messages.length; i += 1) {
    const message = messages[i];
    out.push(message);
    if (message.role !== "assistant" || !message.tool_calls) continue;
    const answered = new Set<string>();
    let j = i + 1;
    while (j < messages.length && messages[j].role === "tool") {
      const tool = messages[j] as Extract<ChatMessage, { role: "tool" }>;
      answered.add(tool.tool_call_id);
      out.push(tool);
      j += 1;
    }
    for (const call of message.tool_calls) {
      if (!answered.has(call.id)) out.push({ role: "tool", tool_call_id: call.id, content: INTERRUPTED });
    }
    i = j - 1;
  }
  return out;
}
