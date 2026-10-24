import { Code, ConnectError, createClient, type Client, type Interceptor, type Transport } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-web";
import { Auth, Controller, ControllerAdmin } from "./gen/controller_pb";

const TOKEN_KEY = "kcore-session";
const REQUEST_TIMEOUT_MS = 25_000;

type Listener = () => void;
const listeners = new Set<Listener>();

/** Session tokens live in sessionStorage, so closing the tab ends the browser's copy. */
export const tokenStore = {
  get(): string {
    try {
      return sessionStorage.getItem(TOKEN_KEY) ?? "";
    } catch {
      return "";
    }
  },
  set(token: string) {
    try {
      if (token) sessionStorage.setItem(TOKEN_KEY, token);
      else sessionStorage.removeItem(TOKEN_KEY);
    } catch {
      /* storage disabled: the token lives only for this page */
    }
    for (const listener of listeners) listener();
  },
  subscribe(listener: Listener): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
};

let pendingToken = "";

/** The five-minute token from Login. It is sent only while no session exists. */
export function setPendingToken(token: string) {
  pendingToken = token;
}

const bearer: Interceptor = (next) => async (req) => {
  const token = tokenStore.get() || pendingToken;
  if (token) req.header.set("authorization", `Bearer ${token}`);
  try {
    return await next(req);
  } catch (error) {
    const err = ConnectError.from(error);
    if (err.code === Code.Unauthenticated && tokenStore.get() && !req.method.parent.typeName.endsWith(".Auth")) {
      tokenStore.set("");
    }
    throw err;
  }
};

const origin = typeof window === "undefined" ? "http://localhost" : window.location.origin;

export let controller: Client<typeof Controller>;
export let admin: Client<typeof ControllerAdmin>;
export let auth: Client<typeof Auth>;
/** Console streams stay open, so they skip the unary deadline. */
export let consoleController: Client<typeof Controller>;

export function setTransport(unary: Transport, streaming: Transport = unary) {
  controller = createClient(Controller, unary);
  admin = createClient(ControllerAdmin, unary);
  auth = createClient(Auth, unary);
  consoleController = createClient(Controller, streaming);
}

setTransport(
  createGrpcWebTransport({ baseUrl: origin, interceptors: [bearer], defaultTimeoutMs: REQUEST_TIMEOUT_MS }),
  createGrpcWebTransport({ baseUrl: origin, interceptors: [bearer] }),
);

export function errorMessage(error: unknown): string {
  if (error instanceof ConnectError) {
    if (error.code === Code.DeadlineExceeded) return "The controller took too long to respond.";
    if (error.code === Code.Unavailable && !error.rawMessage) return "The controller is not reachable.";
    return error.rawMessage || Code[error.code];
  }
  if (error instanceof Error) return error.message;
  return String(error);
}
