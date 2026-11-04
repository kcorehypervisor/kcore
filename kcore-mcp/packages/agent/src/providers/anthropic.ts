import type { ChatMessage, ModelEvent, Provider, StreamRequest } from "./types.js";

export type AnthropicOptions = {
  baseUrl?: string;
  apiKey: string;
  fetch?: typeof fetch;
  maxTokens?: number;
};

const VERSION = "2023-06-01";

/** Messages API streaming. API key only: Anthropic does not allow Claude.ai logins in third-party apps. */
export class Anthropic implements Provider {
  readonly id = "anthropic";
  private readonly baseUrl: string;
  private readonly apiKey: string;
  private readonly fetchImpl: typeof fetch;
  private readonly maxTokens: number;

  constructor(options: AnthropicOptions) {
    this.baseUrl = (options.baseUrl ?? "https://api.anthropic.com/v1").replace(/\/+$/, "");
    this.apiKey = options.apiKey;
    this.fetchImpl = options.fetch ?? fetch;
    this.maxTokens = options.maxTokens ?? 8192;
  }

  private headers(): Record<string, string> {
    return { "x-api-key": this.apiKey, "anthropic-version": VERSION, "content-type": "application/json" };
  }

  async models(): Promise<string[]> {
    try {
      const response = await this.fetchImpl(`${this.baseUrl}/models?limit=100`, { headers: this.headers(), signal: AbortSignal.timeout(10_000) });
      if (!response.ok) return ["claude-sonnet-4-5"];
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      const ids = (body.data ?? []).map((m) => m.id).filter((id): id is string => typeof id === "string");
      return ids.length > 0 ? ids : ["claude-sonnet-4-5"];
    } catch {
      return ["claude-sonnet-4-5"];
    }
  }

  async *stream(request: StreamRequest): AsyncIterable<ModelEvent> {
    const system = request.messages
      .filter((m): m is Extract<ChatMessage, { role: "system" }> => m.role === "system")
      .map((m) => m.content)
      .join("\n\n");
    const response = await this.fetchImpl(`${this.baseUrl}/messages`, {
      method: "POST",
      headers: this.headers(),
      body: JSON.stringify({
        model: request.model,
        max_tokens: this.maxTokens,
        stream: true,
        ...(system ? { system } : {}),
        messages: toAnthropicMessages(request.messages),
        ...(request.tools.length > 0
          ? { tools: request.tools.map((t) => ({ name: t.name, description: t.description, input_schema: t.parameters })) }
          : {}),
      }),
      signal: request.signal,
    });
    if (!response.ok || !response.body) {
      const detail = await response.text().catch(() => "");
      throw new Error(`anthropic returned ${response.status}: ${detail.slice(0, 500)}`);
    }

    const blocks = new Map<number, { id: string; name: string; json: string }>();
    let inputTokens: number | undefined;
    let outputTokens: number | undefined;
    const decoder = new TextDecoder();
    let buffer = "";

    for await (const chunk of response.body as unknown as AsyncIterable<Uint8Array>) {
      buffer += decoder.decode(chunk, { stream: true });
      let split: number;
      while ((split = buffer.indexOf("\n\n")) >= 0) {
        const raw = buffer.slice(0, split);
        buffer = buffer.slice(split + 2);
        const data = raw
          .split("\n")
          .filter((line) => line.startsWith("data:"))
          .map((line) => line.slice(5).trim())
          .join("");
        if (!data) continue;
        let event: AnthropicEvent;
        try {
          event = JSON.parse(data) as AnthropicEvent;
        } catch {
          continue;
        }
        switch (event.type) {
          case "message_start":
            inputTokens = event.message?.usage?.input_tokens;
            break;
          case "content_block_start":
            if (event.content_block?.type === "tool_use") {
              blocks.set(event.index ?? 0, { id: event.content_block.id ?? "", name: event.content_block.name ?? "", json: "" });
            }
            break;
          case "content_block_delta":
            if (event.delta?.type === "text_delta" && event.delta.text) yield { type: "text", delta: event.delta.text };
            if (event.delta?.type === "input_json_delta") {
              const block = blocks.get(event.index ?? 0);
              if (block) block.json += event.delta.partial_json ?? "";
            }
            break;
          case "content_block_stop": {
            const block = blocks.get(event.index ?? 0);
            if (block) {
              blocks.delete(event.index ?? 0);
              yield { type: "tool_call", call: { id: block.id, name: block.name, arguments: block.json || "{}" } };
            }
            break;
          }
          case "message_delta":
            outputTokens = event.usage?.output_tokens ?? outputTokens;
            break;
          case "error":
            throw new Error(`anthropic stream error: ${event.error?.message ?? "unknown"}`);
        }
      }
    }
    yield { type: "done", usage: { inputTokens, outputTokens } };
  }
}

type AnthropicEvent = {
  type: string;
  index?: number;
  message?: { usage?: { input_tokens?: number } };
  content_block?: { type?: string; id?: string; name?: string };
  delta?: { type?: string; text?: string; partial_json?: string };
  usage?: { output_tokens?: number };
  error?: { message?: string };
};

type Block = { type: "text"; text: string } | { type: "tool_use"; id: string; name: string; input: unknown } | { type: "tool_result"; tool_use_id: string; content: string };

export function toAnthropicMessages(messages: ChatMessage[]): Array<{ role: "user" | "assistant"; content: string | Block[] }> {
  const out: Array<{ role: "user" | "assistant"; content: string | Block[] }> = [];
  for (const message of messages) {
    if (message.role === "system") continue;
    if (message.role === "user") {
      out.push({ role: "user", content: message.content });
    } else if (message.role === "assistant") {
      const content: Block[] = [];
      if (message.content) content.push({ type: "text", text: message.content });
      for (const call of message.tool_calls ?? []) {
        let input: unknown = {};
        try {
          input = JSON.parse(call.function.arguments || "{}");
        } catch {
          input = {};
        }
        content.push({ type: "tool_use", id: call.id, name: call.function.name, input });
      }
      if (content.length > 0) out.push({ role: "assistant", content });
    } else {
      const result: Block = { type: "tool_result", tool_use_id: message.tool_call_id, content: message.content };
      const last = out.at(-1);
      if (last && last.role === "user" && Array.isArray(last.content) && last.content.every((b) => b.type === "tool_result")) {
        last.content.push(result);
      } else {
        out.push({ role: "user", content: [result] });
      }
    }
  }
  return out;
}
