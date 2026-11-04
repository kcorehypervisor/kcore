import { isReadOnlyMethod } from "../../server/src/rpc.js";
import type { ToolSpec } from "./providers/types.js";

const CONFIRMED_BY_SERVER_FORM = new Set(["kcore_apply", "kcore_delete", "kcore_operation"]);

type Json = Record<string, unknown>;

/**
 * Shape a tool for the model. The model never picks the controller, the TLS
 * settings, or the context file: those come from the context `kctl agent`
 * wrote. It also never sets `confirm`; the host does, after the operator says yes.
 */
export function modelFacingTool(tool: { name: string; description?: string; inputSchema: unknown }): ToolSpec {
  const schema = structuredClone(tool.inputSchema as Json);
  const properties = (schema.properties ?? {}) as Record<string, Json>;
  if (properties.connection) {
    properties.connection = {
      type: "object",
      description: "Only node is accepted: the node-agent host:port for node-scoped RPCs. Everything else comes from the operator's context.",
      properties: { node: { type: "string" } },
      additionalProperties: false,
    };
  }
  delete properties.confirm;
  schema.properties = properties;
  if (Array.isArray(schema.required)) {
    schema.required = (schema.required as string[]).filter((name) => name !== "confirm");
  }
  return {
    name: tool.name,
    description: tool.description ?? "",
    parameters: schema,
  };
}

export type GatedCall = {
  arguments: Json;
  /** The host must show an approval card before calling, because the server will not ask. */
  hostApproval: { title: string; detail: unknown } | undefined;
  /** Arguments to send once approved. Equals `arguments` when no host approval is needed. */
  approvedArguments: Json;
};

export function gate(name: string, raw: unknown): GatedCall {
  const input: Json = raw && typeof raw === "object" && !Array.isArray(raw) ? { ...(raw as Json) } : {};
  const connection = input.connection && typeof input.connection === "object" ? (input.connection as Json) : undefined;
  if (connection && typeof connection.node === "string" && connection.node.trim()) {
    input.connection = { node: connection.node.trim() };
  } else {
    delete input.connection;
  }

  if (CONFIRMED_BY_SERVER_FORM.has(name)) {
    input.confirm = false;
    const spec = input.spec && typeof input.spec === "object" ? (input.spec as Json) : {};
    if (name === "kcore_operation" && input.action === "install-node") {
      return {
        arguments: input,
        hostApproval: {
          title: "Install kcore on a node and wipe its disks?",
          detail: { action: input.action, spec: { ...spec, acknowledge: undefined } },
        },
        approvedArguments: { ...input, confirm: true },
      };
    }
    return { arguments: input, hostApproval: undefined, approvedArguments: input };
  }

  if (name === "kcore_rpc") {
    const method = typeof input.method === "string" ? input.method : "";
    if (method && !isReadOnlyMethod(method)) {
      const service = typeof input.service === "string" ? input.service : "";
      return {
        arguments: { ...input, confirm: false },
        hostApproval: {
          title: `Call ${service}.${method}?`,
          detail: { service, method, request: input.request ?? {}, node: (input.connection as Json | undefined)?.node },
        },
        approvedArguments: { ...input, confirm: true },
      };
    }
    delete input.confirm;
  }
  return { arguments: input, hostApproval: undefined, approvedArguments: input };
}
