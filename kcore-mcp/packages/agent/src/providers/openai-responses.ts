import type { ChatMessage, ModelEvent, Provider, StreamRequest } from "./types.js";

export type ResponsesOptions = {
  id: string;
  baseUrl?: string;
  /** Called per request; `force` asks for a fresh token after a 401. */
  token: (force?: boolean) => Promise<string>;
  fetch?: typeof fetch;
  namespace?: string;
};

/**
 * The Responses API as Sign in with ChatGPT allows it: `store: false`, `stream: true`, the whole history in `input`,
 * the system prompt in `instructions`, and function tools grouped in a namespace.
 */
export class OpenAIResponses implements Provider {
  readonly id: string;
  private readonly baseUrl: string;
  private readonly token: (force?: boolean) => Promise<string>;
  private readonly fetchImpl: typeof fetch;
  private readonly namespace: string;

  constructor(options: ResponsesOptions) {
    this.id = options.id;
    this.baseUrl = (options.baseUrl ?? "https://api.openai.com/v1").replace(/\/+$/, "");
    this.token = options.token;
    this.fetchImpl = options.fetch ?? fetch;
    this.namespace = options.namespace ?? "kcore";
  }

  async models(): Promise<string[]> {
    const response = await this.authed((token) =>
      this.fetchImpl(`${this.baseUrl}/models`, { headers: { authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(15_000) }),
    );
    if (!response.ok) throw new Error(`model list answered ${response.status}`);
    const body = (await response.json()) as { models?: Array<{ slug?: string; visibility?: string }> };
    return (body.models ?? []).filter((m) => m.visibility === "list" && m.slug).map((m) => m.slug as string);
  }

  private async authed(send: (token: string) => Promise<Response>): Promise<Response> {
    const first = await send(await this.token());
    if (first.status !== 401) return first;
    return send(await this.token(true));
  }

  async *stream(request: StreamRequest): AsyncIterable<ModelEvent> {
    const instructions = request.messages
      .filter((m): m is Extract<ChatMessage, { role: "system" }> => m.role === "system")
      .map((m) => m.content)
      .join("\n\n");
    const body = JSON.stringify({
      model: request.model,
      store: false,
      stream: true,
      ...(instructions ? { instructions } : {}),
      input: toResponsesInput(request.messages, this.namespace),
      ...(request.tools.length > 0
        ? {
            tools: [
              {
                type: "namespace",
                name: this.namespace,
                description: "kcore cluster management tools",
                tools: request.tools.map((t) => ({ type: "function", name: t.name, description: t.description, parameters: t.parameters })),
              },
            ],
          }
        : {}),
    });

    const response = await this.authed(async (token) =>
      this.fetchImpl(`${this.baseUrl}/responses`, {
        method: "POST",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json", accept: "text/event-stream" },
        body,
        signal: request.signal,
      }),
    );
    if (!response.ok || !response.body) {
      const detail = await response.text().catch(() => "");
      throw new Error(`chatgpt returned ${response.status}: ${detail.slice(0, 500)}`);
    }

    const decoder = new TextDecoder();
    let buffer = "";
    let completed = false;
    let usage: { inputTokens?: number; outputTokens?: number } | undefined;

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
        if (!data || data === "[DONE]") continue;
        let event: ResponsesEvent;
        try {
          event = JSON.parse(data) as ResponsesEvent;
        } catch {
          continue;
        }
        switch (event.type) {
          case "response.output_text.delta":
            if (event.delta) yield { type: "text", delta: event.delta };
            break;
          case "response.output_item.done":
            if (event.item?.type === "function_call" && event.item.call_id && event.item.name) {
              yield { type: "tool_call", call: { id: event.item.call_id, name: event.item.name, arguments: event.item.arguments || "{}" } };
            }
            break;
          case "response.completed":
            completed = true;
            usage = { inputTokens: event.response?.usage?.input_tokens, outputTokens: event.response?.usage?.output_tokens };
            break;
          case "response.failed":
            throw new Error(describeFailure(event.response?.error?.code, event.response?.error?.message));
          case "response.incomplete":
            throw new Error(`the response ended early: ${event.response?.incomplete_details?.reason ?? "unknown reason"}`);
          case "error":
            throw new Error(describeFailure(event.code, event.message));
        }
      }
    }
    if (!completed) throw new Error("the stream ended before response.completed");
    yield { type: "done", usage };
  }
}

type ResponsesEvent = {
  type: string;
  delta?: string;
  code?: string;
  message?: string;
  item?: { type?: string; call_id?: string; name?: string; arguments?: string };
  response?: {
    usage?: { input_tokens?: number; output_tokens?: number };
    error?: { code?: string; message?: string };
    incomplete_details?: { reason?: string };
  };
};

function describeFailure(code: string | undefined, message: string | undefined): string {
  if (code === "subscription_sharing_usage_limit_exceeded") {
    return "ChatGPT plan usage limit reached for this app. Check ChatGPT Settings → Usage, or switch to an API key.";
  }
  if (code === "subscription_sharing_usage_unavailable") {
    return "ChatGPT plan usage is not available for this account right now.";
  }
  return `chatgpt failed${code ? ` (${code})` : ""}${message ? `: ${message}` : ""}`;
}

type InputItem =
  | { role: "user"; content: string }
  | { type: "message"; role: "assistant"; content: Array<{ type: "output_text"; text: string }> }
  | { type: "function_call"; call_id: string; name: string; namespace: string; arguments: string }
  | { type: "function_call_output"; call_id: string; output: string };

export function toResponsesInput(messages: ChatMessage[], namespace: string): InputItem[] {
  const out: InputItem[] = [];
  for (const message of messages) {
    if (message.role === "system") continue;
    if (message.role === "user") {
      out.push({ role: "user", content: message.content });
    } else if (message.role === "assistant") {
      if (message.content) out.push({ type: "message", role: "assistant", content: [{ type: "output_text", text: message.content }] });
      for (const call of message.tool_calls ?? []) {
        out.push({ type: "function_call", call_id: call.id, name: call.function.name, namespace, arguments: call.function.arguments || "{}" });
      }
    } else {
      out.push({ type: "function_call_output", call_id: message.tool_call_id, output: message.content });
    }
  }
  return out;
}
