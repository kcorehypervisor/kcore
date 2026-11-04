import type { ModelEvent, Provider, StreamRequest } from "../src/providers/types.js";

export type Script = (request: StreamRequest, turn: number) => ModelEvent[];

export function scripted(script: Script): Provider & { requests: StreamRequest[] } {
  const requests: StreamRequest[] = [];
  return {
    id: "scripted",
    requests,
    async models() {
      return ["scripted-1"];
    },
    async *stream(request) {
      requests.push(request);
      for (const event of script(request, requests.length - 1)) yield event;
    },
  };
}

export const call = (id: string, name: string, args: unknown): ModelEvent => ({
  type: "tool_call",
  call: { id, name, arguments: JSON.stringify(args) },
});

export async function waitFor<T>(read: () => T | undefined, ms = 5000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const value = read();
    if (value !== undefined) return value;
    if (Date.now() > end) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 10));
  }
}
