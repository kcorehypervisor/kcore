import { InputError } from "./names.js";
import { SERVICE_NAMES, serviceClient, type ServiceName } from "./grpc.js";

/** Node methods whose handlers always return UNIMPLEMENTED. */
const UNIMPLEMENTED = new Set([
  "node-compute/createVm",
  "node-compute/updateVm",
  "node-compute/deleteVm",
  "node-compute/rebootVm",
  "node-compute/pullImage",
  "node-compute/createWorkload",
  "node-compute/deleteWorkload",
]);

export type RpcMethod = {
  service: ServiceName;
  method: string;
  readOnly: boolean;
};

export function isServiceName(value: string): value is ServiceName {
  return (SERVICE_NAMES as readonly string[]).includes(value);
}

export function isReadOnlyMethod(method: string): boolean {
  return /^(get|list|classify|check|plan)/.test(method);
}

export function rpcCatalog(service?: ServiceName): RpcMethod[] {
  const names = service ? [service] : [...SERVICE_NAMES];
  const methods: RpcMethod[] = [];
  for (const name of names) {
    const definition = serviceClient(name).service;
    for (const [rawName, raw] of Object.entries(definition)) {
      const desc = raw as { requestStream?: boolean; responseStream?: boolean };
      if (desc.requestStream || desc.responseStream) continue;
      const method = rawName[0].toLowerCase() + rawName.slice(1);
      if (UNIMPLEMENTED.has(`${name}/${method}`)) continue;
      methods.push({ service: name, method, readOnly: isReadOnlyMethod(method) });
    }
  }
  methods.sort((a, b) => a.service.localeCompare(b.service) || a.method.localeCompare(b.method));
  return methods;
}

export function requireRpc(serviceName: string, method: string): RpcMethod {
  if (!isServiceName(serviceName)) {
    throw new InputError(`unknown gRPC service '${serviceName}'. Use one of: ${SERVICE_NAMES.join(", ")}`);
  }
  const found = rpcCatalog(serviceName).find((item) => item.method === method);
  if (!found) {
    throw new InputError(
      `${serviceName}.${method} is not an implemented unary RPC. Streaming methods and node stubs that always return UNIMPLEMENTED are omitted.`,
    );
  }
  return found;
}
