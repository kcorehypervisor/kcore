import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";
import { InputError } from "./names.js";
import { resolveConnection, type ConnectionOptions, type ResolvedConnection } from "./connection.js";

type GrpcClient = {
  close: () => void;
  [method: string]: unknown;
};

export const SERVICE_NAMES = [
  "controller",
  "controller-admin",
  "node-compute",
  "node-container",
  "node-storage",
  "node-info",
  "node-admin",
] as const;

export type ServiceName = (typeof SERVICE_NAMES)[number];

type Loaded = Record<ServiceName, grpc.ServiceClientConstructor>;

let loaded: Loaded | undefined;

export function protoDir(): string {
  const here = dirname(fileURLToPath(import.meta.url));
  const candidates = [join(here, "proto"), join(here, "..", "proto")];
  for (const dir of candidates) {
    if (existsSync(join(dir, "controller.proto"))) return dir;
  }
  throw new InputError("controller.proto is missing from the kcore MCP package");
}

function services(): Loaded {
  if (loaded) return loaded;
  const dir = protoDir();
  const definition = protoLoader.loadSync(
    [join(dir, "controller.proto"), join(dir, "node.proto")],
    {
      keepCase: false,
      longs: String,
      enums: String,
      defaults: true,
      oneofs: true,
      includeDirs: [dir],
    },
  );
  const pkg = grpc.loadPackageDefinition(definition) as {
    kcore: {
      controller: { Controller: grpc.ServiceClientConstructor; ControllerAdmin: grpc.ServiceClientConstructor };
      node: {
        NodeCompute: grpc.ServiceClientConstructor;
        NodeContainer: grpc.ServiceClientConstructor;
        NodeStorage: grpc.ServiceClientConstructor;
        NodeInfo: grpc.ServiceClientConstructor;
        NodeAdmin: grpc.ServiceClientConstructor;
      };
    };
  };
  loaded = {
    controller: pkg.kcore.controller.Controller,
    "controller-admin": pkg.kcore.controller.ControllerAdmin,
    "node-compute": pkg.kcore.node.NodeCompute,
    "node-container": pkg.kcore.node.NodeContainer,
    "node-storage": pkg.kcore.node.NodeStorage,
    "node-info": pkg.kcore.node.NodeInfo,
    "node-admin": pkg.kcore.node.NodeAdmin,
  };
  return loaded;
}

export function serviceClient(name: ServiceName): grpc.ServiceClientConstructor {
  return services()[name];
}

export function controllerMethods(): string[] {
  return Object.keys(services().controller.service);
}

function credentials(conn: ResolvedConnection): grpc.ChannelCredentials {
  if (conn.insecure) return grpc.credentials.createInsecure();
  if (!conn.ca || !conn.cert || !conn.key) {
    throw new InputError("Controller mTLS needs a CA, client certificate, and client key in the kcore context.");
  }
  return grpc.credentials.createSsl(conn.ca, conn.key, conn.cert);
}

function channelOptions(conn: ResolvedConnection): grpc.ChannelOptions {
  const options: grpc.ChannelOptions = {
    "grpc.keepalive_time_ms": 30_000,
  };
  if (conn.tlsServerName) options["grpc.ssl_target_name_override"] = conn.tlsServerName;
  return options;
}

export async function callController(options: ConnectionOptions, method: string, request: Record<string, unknown>): Promise<unknown> {
  return callApi("controller", undefined, options, method, request);
}

export async function callNode(address: string, options: ConnectionOptions, method: string, request: Record<string, unknown>): Promise<unknown> {
  return callApi("node-admin", address, options, method, request);
}

export async function callNodeCompute(address: string, options: ConnectionOptions, method: string, request: Record<string, unknown>): Promise<unknown> {
  return callApi("node-compute", address, options, method, request);
}

export async function callApi(
  service: ServiceName,
  address: string | undefined,
  options: ConnectionOptions,
  method: string,
  request: Record<string, unknown>,
): Promise<unknown> {
  const conn = resolveConnection(address ? { ...options, controller: address } : options);
  return callService(services()[service], conn.address, conn, method, request);
}

function callService(
  Ctor: grpc.ServiceClientConstructor,
  address: string,
  conn: ResolvedConnection,
  method: string,
  request: Record<string, unknown>,
): Promise<unknown> {
  const client = new Ctor(address, credentials(conn), channelOptions(conn)) as unknown as GrpcClient;
  const fn = client[method];
  if (typeof fn !== "function") {
    client.close();
    throw new InputError(`The gRPC API has no ${method} method.`);
  }
  const deadline = new Date(Date.now() + conn.timeoutMs);
  return new Promise((resolve, reject) => {
    (fn as (req: unknown, meta: { deadline: Date }, cb: (err: grpc.ServiceError | null, res: unknown) => void) => void).call(
      client,
      request,
      { deadline },
      (error, response) => {
        client.close();
        if (error) {
          reject(new Error(error.details || error.message));
          return;
        }
        resolve(response);
      },
    );
  });
}

export function redact(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(redact);
  if (!value || typeof value !== "object") return value;
  const out: Record<string, unknown> = {};
  for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
    if (typeof item === "string" && item.length > 0 && /key.?pem|private.?key|password|secret/i.test(key)) out[key] = "[redacted]";
    else out[key] = redact(item);
  }
  return out;
}
