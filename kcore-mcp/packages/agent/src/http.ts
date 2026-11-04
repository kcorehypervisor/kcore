import { randomUUID, timingSafeEqual } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer as createHttpServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { join } from "node:path";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { AuthStore } from "./auth.js";
import { CALLBACK_PATH } from "./chatgpt.js";
import { Session, type AgentEvent, type LiveEvent } from "./session.js";
import type { ClusterSummary } from "./summary.js";
import { listTranscripts, readTranscript } from "./transcript.js";

export type AppOptions = {
  token: string;
  auth: AuthStore;
  createServer: () => McpServer;
  uiDir: string;
  logDir?: string;
  summary: () => ClusterSummary;
};

const STATIC: Record<string, { file: string; type: string }> = {
  "/": { file: "index.html", type: "text/html; charset=utf-8" },
  "/app.js": { file: "app.js", type: "text/javascript; charset=utf-8" },
  "/style.css": { file: "style.css", type: "text/css; charset=utf-8" },
};

const MAX_BODY = 1_000_000;

export type App = {
  server: Server;
  sessions: Map<string, Session>;
  close(): Promise<void>;
};

export function createApp(options: AppOptions): App {
  const sessions = new Map<string, Session>();
  const expected = Buffer.from(options.token);
  if (options.logDir) {
    for (const id of listTranscripts(options.logDir)) {
      const restore = readTranscript(options.logDir, id);
      if (restore) sessions.set(id, new Session({ id, createServer: options.createServer, logDir: options.logDir, restore }));
    }
  }

  const server = createHttpServer((req, res) => {
    handle(req, res).catch((error) => {
      if (!res.headersSent) json(res, 500, { error: error instanceof Error ? error.message : String(error) });
      else res.end();
    });
  });

  function hostAllowed(req: IncomingMessage): boolean {
    const address = server.address() as AddressInfo | null;
    if (!address) return false;
    const host = req.headers.host ?? "";
    return host === `127.0.0.1:${address.port}` || host === `localhost:${address.port}`;
  }

  function originAllowed(req: IncomingMessage): boolean {
    const origin = req.headers.origin;
    if (!origin) return true;
    const address = server.address() as AddressInfo | null;
    if (!address) return false;
    return origin === `http://127.0.0.1:${address.port}` || origin === `http://localhost:${address.port}`;
  }

  function authorized(req: IncomingMessage): boolean {
    const header = req.headers.authorization ?? "";
    if (!header.startsWith("Bearer ")) return false;
    const given = Buffer.from(header.slice(7));
    return given.length === expected.length && timingSafeEqual(given, expected);
  }

  async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    res.setHeader("cache-control", "no-store");
    res.setHeader("x-content-type-options", "nosniff");
    res.setHeader("referrer-policy", "no-referrer");
    res.setHeader("x-frame-options", "DENY");
    res.setHeader(
      "content-security-policy",
      "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    );

    if (!hostAllowed(req) || !originAllowed(req)) return json(res, 403, { error: "forbidden host or origin" });

    const url = new URL(req.url ?? "/", "http://localhost");
    const path = url.pathname;
    const method = req.method ?? "GET";

    if (method === "GET" && STATIC[path]) {
      const entry = STATIC[path];
      const body = await readFile(join(options.uiDir, entry.file));
      res.writeHead(200, { "content-type": entry.type });
      res.end(body);
      return;
    }

    if (method === "GET" && path === "/favicon.ico") {
      res.writeHead(204);
      res.end();
      return;
    }

    if (method === "GET" && path === CALLBACK_PATH) {
      const outcome = await options.auth.chatgpt.handleCallback(url.searchParams);
      res.writeHead(outcome.ok ? 200 : 400, { "content-type": "text/html; charset=utf-8" });
      res.end(callbackPage(outcome.ok, outcome.message));
      return;
    }

    if (!path.startsWith("/api/")) return json(res, 404, { error: "not found" });
    if (!authorized(req)) return json(res, 401, { error: "missing or wrong session token" });

    if (method === "GET" && path === "/api/cluster/summary") return json(res, 200, options.summary());
    if (method === "GET" && path === "/api/providers") return json(res, 200, { providers: options.auth.describe() });

    if (path === "/api/providers/chatgpt/signin" && method === "POST") {
      const body = await readJson(req);
      const address = server.address() as AddressInfo;
      return json(res, 200, { url: options.auth.chatgpt.start(address.port, { newAccount: body.newAccount === true }) });
    }
    if (path === "/api/providers/chatgpt/signin" && method === "DELETE") {
      const { revoked } = await options.auth.chatgpt.signOut();
      return json(res, 200, { providers: options.auth.describe(), revoked });
    }

    let match = /^\/api\/providers\/([a-z0-9-]+)\/models$/.exec(path);
    if (match && method === "GET") {
      const provider = options.auth.provider(match[1]);
      if (!provider) return json(res, 404, { error: "provider is not connected" });
      return json(res, 200, { models: await provider.models() });
    }

    match = /^\/api\/providers\/([a-z0-9-]+)\/key$/.exec(path);
    if (match && method === "PUT") {
      const body = await readJson(req);
      try {
        options.auth.setKey(match[1], typeof body.key === "string" ? body.key : "");
      } catch (error) {
        return json(res, 400, { error: error instanceof Error ? error.message : String(error) });
      }
      return json(res, 200, { providers: options.auth.describe() });
    }
    if (match && method === "DELETE") {
      options.auth.removeKey(match[1]);
      return json(res, 200, { providers: options.auth.describe() });
    }

    if (path === "/api/sessions" && method === "GET") {
      return json(res, 200, {
        sessions: [...sessions.values()]
          .sort((a, b) => b.createdAt - a.createdAt)
          .map((s) => ({ id: s.id, title: s.title, createdAt: s.createdAt, busy: s.isBusy })),
      });
    }
    if (path === "/api/sessions" && method === "POST") {
      const id = randomUUID();
      sessions.set(id, new Session({ id, createServer: options.createServer, logDir: options.logDir }));
      return json(res, 201, { id });
    }

    match = /^\/api\/sessions\/([0-9a-f-]{36})(?:\/(messages|events|cancel|forms\/f\d+))?$/.exec(path);
    if (!match) return json(res, 404, { error: "not found" });
    const session = sessions.get(match[1]);
    if (!session) return json(res, 404, { error: "no such session" });
    const sub = match[2];

    if (!sub && method === "GET") {
      return json(res, 200, { id: session.id, title: session.title, busy: session.isBusy, events: session.history() });
    }

    if (sub === "events" && method === "GET") {
      const after = Number(url.searchParams.get("after") ?? "0") || 0;
      res.writeHead(200, { "content-type": "text/event-stream", connection: "keep-alive" });
      const write = (event: AgentEvent | LiveEvent) => {
        if (event.type === "delta") res.write(`event: delta\ndata: ${JSON.stringify(event)}\n\n`);
        else res.write(`id: ${event.id}\ndata: ${JSON.stringify(event)}\n\n`);
      };
      for (const event of session.history(after)) write(event);
      const unsubscribe = session.subscribe(write);
      const keepAlive = setInterval(() => res.write(": keep-alive\n\n"), 15_000);
      req.on("close", () => {
        clearInterval(keepAlive);
        unsubscribe();
      });
      return;
    }

    if (sub === "messages" && method === "POST") {
      const body = await readJson(req);
      const text = typeof body.text === "string" ? body.text.trim() : "";
      const providerId = typeof body.provider === "string" ? body.provider : "";
      const model = typeof body.model === "string" ? body.model.trim() : "";
      if (!text) return json(res, 400, { error: "text is required" });
      const provider = options.auth.provider(providerId);
      if (!provider) return json(res, 400, { error: `provider ${providerId || "(none)"} is not connected` });
      if (!model) return json(res, 400, { error: "model is required" });
      if (session.isBusy) return json(res, 409, { error: "a turn is already running" });
      void session.send(text, provider, model);
      return json(res, 202, { accepted: true });
    }

    if (sub === "cancel" && method === "POST") {
      session.cancel();
      return json(res, 200, { ok: true });
    }

    if (sub?.startsWith("forms/") && method === "POST") {
      const body = await readJson(req);
      const action = body.action === "accept" || body.action === "decline" || body.action === "cancel" ? body.action : undefined;
      if (!action) return json(res, 400, { error: "action must be accept, decline, or cancel" });
      const outcome = session.answerForm(sub.slice("forms/".length), action, body.content);
      return outcome.ok ? json(res, 200, { ok: true }) : json(res, 400, { error: outcome.error });
    }

    return json(res, 405, { error: "method not allowed" });
  }

  return {
    server,
    sessions,
    async close() {
      for (const session of sessions.values()) await session.close();
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

function callbackPage(ok: boolean, message: string): string {
  const safe = message.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c] as string);
  return `<!doctype html><meta charset="utf-8"><title>kcore agent</title><body style="font:16px system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem"><h1>${ok ? "Signed in" : "Not signed in"}</h1><p>${safe}</p>`;
}

function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json; charset=utf-8" });
  res.end(JSON.stringify(body));
}

async function readJson(req: IncomingMessage): Promise<Record<string, unknown>> {
  let size = 0;
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    size += (chunk as Buffer).length;
    if (size > MAX_BODY) throw new Error("request body too large");
    chunks.push(chunk as Buffer);
  }
  if (chunks.length === 0) return {};
  const parsed = JSON.parse(Buffer.concat(chunks).toString("utf8")) as unknown;
  return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : {};
}

