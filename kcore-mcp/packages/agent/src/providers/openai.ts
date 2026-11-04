import type { ModelEvent, Provider, StreamRequest } from "./types.js";

export type OpenAICompatibleOptions = {
  id: string;
  baseUrl: string;
  apiKey: string;
  fetch?: typeof fetch;
  fallbackModels: string[];
};

/** Chat Completions streaming. OpenAI API keys and DeepSeek both speak this. */
export class OpenAICompatible implements Provider {
  readonly id: string;
  private readonly baseUrl: string;
  private readonly apiKey: string;
  private readonly fetchImpl: typeof fetch;
  private readonly fallbackModels: string[];

  constructor(options: OpenAICompatibleOptions) {
    this.id = options.id;
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.apiKey = options.apiKey;
    this.fetchImpl = options.fetch ?? fetch;
    this.fallbackModels = options.fallbackModels;
  }

  async models(): Promise<string[]> {
    try {
      const response = await this.fetchImpl(`${this.baseUrl}/models`, {
        headers: { authorization: `Bearer ${this.apiKey}` },
        signal: AbortSignal.timeout(10_000),
      });
      if (!response.ok) return this.fallbackModels;
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      const ids = (body.data ?? []).map((item) => item.id).filter((id): id is string => typeof id === "string");
      return ids.length > 0 ? ids.sort() : this.fallbackModels;
    } catch {
      return this.fallbackModels;
    }
  }

  async *stream(request: StreamRequest): AsyncIterable<ModelEvent> {
    const response = await this.fetchImpl(`${this.baseUrl}/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${this.apiKey}` },
      body: JSON.stringify({
        model: request.model,
        messages: request.messages,
        stream: true,
        stream_options: { include_usage: true },
        ...(request.tools.length > 0
          ? {
              tools: request.tools.map((tool) => ({
                type: "function",
                function: { name: tool.name, description: tool.description, parameters: tool.parameters },
              })),
            }
          : {}),
      }),
      signal: request.signal,
    });
    if (!response.ok || !response.body) {
      const detail = await response.text().catch(() => "");
      throw new Error(`${this.id} returned ${response.status}: ${detail.slice(0, 500)}`);
    }

    const calls = new Map<number, { id: string; name: string; arguments: string }>();
    let usage: { inputTokens?: number; outputTokens?: number } | undefined;
    const decoder = new TextDecoder();
    let buffer = "";

    for await (const chunk of response.body as unknown as AsyncIterable<Uint8Array>) {
      buffer += decoder.decode(chunk, { stream: true });
      let newline: number;
      while ((newline = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (data === "[DONE]") continue;
        let parsed: StreamChunk;
        try {
          parsed = JSON.parse(data) as StreamChunk;
        } catch {
          continue;
        }
        if (parsed.usage) {
          usage = { inputTokens: parsed.usage.prompt_tokens, outputTokens: parsed.usage.completion_tokens };
        }
        const delta = parsed.choices?.[0]?.delta;
        if (!delta) continue;
        if (delta.content) yield { type: "text", delta: delta.content };
        for (const piece of delta.tool_calls ?? []) {
          const slot = calls.get(piece.index) ?? { id: "", name: "", arguments: "" };
          if (piece.id) slot.id = piece.id;
          if (piece.function?.name) slot.name += piece.function.name;
          if (piece.function?.arguments) slot.arguments += piece.function.arguments;
          calls.set(piece.index, slot);
        }
      }
    }

    for (const [index, call] of [...calls.entries()].sort((a, b) => a[0] - b[0])) {
      yield { type: "tool_call", call: { id: call.id || `call_${index}`, name: call.name, arguments: call.arguments || "{}" } };
    }
    yield { type: "done", usage };
  }
}

type StreamChunk = {
  choices?: Array<{
    delta?: {
      content?: string | null;
      tool_calls?: Array<{ index: number; id?: string; function?: { name?: string; arguments?: string } }>;
    };
  }>;
  usage?: { prompt_tokens?: number; completion_tokens?: number };
};
