import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { ElicitResultSchema } from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";
import { advise, mergeAnswers, normalizeDesired, type Question } from "./advise.js";
import { KINDS, findKind } from "./catalog.js";
import { InputError } from "./names.js";
import { resolveConnection } from "./connection.js";
import { callController, callNode, redact } from "./grpc.js";
import { createClusterContext } from "./pki.js";
import { bootstrapCertRequest, buildApply, buildDelete, buildOperation, buildPlanCall, buildRead, type RpcCall } from "./requests.js";
import { diffSpec } from "./plan.js";
import type { ConnectionOptions } from "./connection.js";

const INSTRUCTIONS = `kcore MCP dials the kcore controller gRPC API. Apply is a declarative upsert: it creates a missing resource, updates mutable fields, and rejects immutable changes.

Before every create, update, delete, migrate, drain, or node install:
1. Call kcore_advise and ask the operator every blocking question. Wait for the answers.
2. Also ask the recommended questions unless the operator said to use defaults.
3. Call kcore_plan and show that plan.
4. Call kcore_apply or kcore_delete only after the operator explicitly agrees, with confirm set to true.

Ask for image URLs, checksums, addresses, disk devices, and SSH keys. Use kcore_catalog when you are unsure which resource matches the request.`;

const connectionShape = {
  config: z.string().optional().describe("Path to the kcore context file. Defaults to ~/.kcore/config."),
  controller: z.string().optional().describe("Controller host:port override."),
  insecure: z.boolean().optional().describe("Plain HTTP, skipping TLS client auth."),
  tlsServerName: z.string().optional().describe("SNI / certificate name when dialing an IP."),
  operator: z.string().optional().describe("Operator identity for --as."),
  node: z.string().optional().describe("Node-agent host:port for node-scoped commands."),
};

const specSchema = z.record(z.string(), z.unknown()).optional();

type ConnectionInput = ConnectionOptions;

function connectionOf(input: ConnectionInput | undefined, controller?: string): ConnectionOptions {
  return {
    config: input?.config,
    controller: controller || input?.controller,
    insecure: input?.insecure,
    tlsServerName: input?.tlsServerName,
    operator: input?.operator,
    timeoutMs: input?.timeoutMs,
  };
}

function dial(input: ConnectionInput | undefined, spec: Record<string, unknown>): ConnectionOptions {
  const controller = typeof spec.controller === "string" && spec.controller.trim() ? spec.controller.trim() : undefined;
  return connectionOf(input, controller);
}

async function execute(call: RpcCall, options: ConnectionOptions): Promise<unknown> {
  if (call.target === "node") {
    if (!call.address) throw new InputError("node address is required");
    return callNode(call.address, options, call.method, call.request);
  }
  return callController(options, call.method, call.request);
}

function text(payload: unknown) {
  return { content: [{ type: "text" as const, text: JSON.stringify(redact(payload), null, 2) }] };
}

function failure(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return text({ ok: false, error: message });
}

type ToolExtra = {
  sendRequest: (request: { method: string; params: Record<string, unknown> }, resultSchema: typeof ElicitResultSchema) => Promise<unknown>;
};

async function elicit(extra: ToolExtra, message: string, questions: Question[]): Promise<Record<string, unknown> | undefined> {
  if (questions.length === 0) return undefined;
  const properties: Record<string, unknown> = {};
  const required: string[] = [];
  for (const item of questions) {
    properties[item.id] = {
      type: "string",
      title: item.prompt,
      description: item.why,
      ...(item.choices ? { enum: item.choices } : {}),
    };
    if (item.required) required.push(item.id);
  }
  try {
    const result = ElicitResultSchema.parse(
      await extra.sendRequest(
        {
          method: "elicitation/create",
          params: {
            message,
            requestedSchema: { type: "object", properties, required },
          },
        },
        ElicitResultSchema,
      ),
    );
    if (result.action !== "accept" || !result.content) return undefined;
    return result.content;
  } catch {
    return undefined;
  }
}

export function createServer(): McpServer {
  const server = new McpServer(
    { name: "kcore", version: "0.1.0" },
    { instructions: INSTRUCTIONS },
  );

  server.registerTool(
    "kcore_catalog",
    {
      title: "List kcore resources",
      description: "List the resources this server can plan and apply, including which fields are mutable. Read-only.",
      inputSchema: {},
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async () =>
      text({
        resources: KINDS.map((kind) => ({
          id: kind.id,
          yamlKind: kind.yamlKind,
          summary: kind.summary,
          mutable: kind.mutable,
          immutable: kind.immutable,
        })),
      }),
  );

  server.registerTool(
    "kcore_advise",
    {
      title: "Ask before changing kcore",
      description:
        "Return the questions the operator must answer before a cluster, VM, or other kcore change. Call this first. Read-only.",
      inputSchema: {
        intent: z.string().optional().describe("What the operator wants, in their words."),
        kind: z.string().optional().describe("Resource id such as vm, network, or cluster."),
        spec: specSchema.describe("Answers collected so far."),
        manifest: z.string().optional().describe("YAML manifest, when the operator already has one."),
        acceptDefaults: z.boolean().optional().describe("Skip recommended questions."),
      },
      annotations: { readOnlyHint: true, openWorldHint: true },
    },
    async (input) => text(advise(input)),
  );

  server.registerTool(
    "kcore_plan",
    {
      title: "Plan a kcore change",
      description:
        "Terraform-style plan. If required answers are missing, returns the questions instead of contacting the cluster. With checkCluster, calls the controller read, ClassifyDiskLayout, or PlanClusterUpdate RPC.",
      inputSchema: {
        kind: z.string().optional(),
        intent: z.string().optional(),
        spec: specSchema,
        manifest: z.string().optional(),
        current: z.record(z.string(), z.unknown()).nullable().optional().describe("Existing spec from a previous read. Omit when unknown."),
        acceptDefaults: z.boolean().optional(),
        checkCluster: z.boolean().optional().describe("Call the controller to read current state or run a plan RPC."),
        connection: z.object(connectionShape).optional(),
      },
      annotations: { readOnlyHint: true, openWorldHint: true },
    },
    async (input, extra) => {
      try {
        const prepared = await prepare(input, extra as ToolExtra);
        if (!prepared.ready || !prepared.desired) return text(prepared.advice);
        if (prepared.desired.kind.id === "cluster") {
          return text({
            ok: true,
            kind: "cluster",
            plan: {
              action: "create",
              summary:
                "Generate a cluster CA, a controller certificate for this address, and a client certificate. The context is saved for later gRPC calls. Private keys stay on disk.",
            },
            files: ["ca.crt", "ca.key", "sub-ca.crt", "sub-ca.key", "controller.crt", "controller.key", "kctl.crt", "kctl.key"],
            warnings: prepared.advice.warnings,
            next: "Show this plan to the operator. Call kcore_apply with confirm true only after they agree.",
          });
        }
        const local = diffSpec(prepared.desired.kind, prepared.desired.spec, input.current);
        let cluster: unknown;
        if (input.checkCluster) {
          const call = buildPlanCall(prepared.desired.kind, prepared.desired.spec);
          try {
            cluster = await execute(call, dial(input.connection, prepared.desired.spec));
          } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            if (/not found/i.test(message)) cluster = { found: false, message };
            else throw error;
          }
        }
        return text({
          ok: true,
          kind: prepared.desired.kind.id,
          plan: local,
          rpc: buildApply(prepared.desired.kind, prepared.desired.spec).map((call) => call.method),
          warnings: prepared.advice.warnings,
          recommendedQuestions: prepared.advice.recommended,
          cluster,
          next: "Show this plan to the operator. Call kcore_apply with confirm true only after they agree.",
        });
      } catch (error) {
        return failure(error);
      }
    },
  );

  server.registerTool(
    "kcore_apply",
    {
      title: "Apply a kcore change",
      description:
        "Declarative apply, like terraform apply. Refuses to run until blocking questions are answered and confirm is true.",
      inputSchema: {
        kind: z.string().optional(),
        intent: z.string().optional(),
        spec: specSchema,
        manifest: z.string().optional(),
        acceptDefaults: z.boolean().optional(),
        confirm: z.boolean().describe("True only after the operator has agreed to this exact plan."),
        confirmReplace: z.boolean().optional().describe("True only when the operator agreed to delete and recreate an immutable change."),
        current: z.record(z.string(), z.unknown()).nullable().optional(),
        connection: z.object(connectionShape).optional(),
      },
      annotations: { readOnlyHint: false, destructiveHint: true, openWorldHint: true },
    },
    async (input, extra) => {
      try {
        const prepared = await prepare(input, extra as ToolExtra);
        if (!prepared.ready || !prepared.desired) return text({ applied: false, ...prepared.advice });
        const local = diffSpec(prepared.desired.kind, prepared.desired.spec, input.current);
        if (local.action === "replace" && input.confirmReplace !== true) {
          return text({
            applied: false,
            plan: local,
            askTheUser: "This change replaces the resource. Ask the operator, then call kcore_apply again with confirmReplace true.",
          });
        }
        if (input.confirm !== true) {
          const accepted = await elicit(extra as ToolExtra, "Apply this kcore change now?", [
            {
              id: "confirm",
              prompt: "Apply this change now?",
              why: local.summary,
              required: true,
              choices: ["yes", "no"],
            },
          ]);
          if (accepted?.confirm !== "yes") {
            return text({
              applied: false,
              plan: local,
              warnings: prepared.advice.warnings,
              askTheUser: "Show the plan and ask the operator to agree. Call kcore_apply again with confirm true after they do.",
            });
          }
        }
        if (prepared.desired.kind.id === "cluster") {
          const created = await createClusterContext(prepared.desired.spec, connectionOf(input.connection));
          return text({ applied: true, plan: local, ...created });
        }
        const options = dial(input.connection, prepared.desired.spec);
        const responses = [];
        for (const call of buildApply(prepared.desired.kind, prepared.desired.spec)) {
          responses.push({ method: call.method, response: await execute(call, options) });
        }
        return text({
          applied: true,
          plan: local,
          responses,
        });
      } catch (error) {
        return failure(error);
      }
    },
  );

  server.registerTool(
    "kcore_read",
    {
      title: "Read kcore resources",
      description: "List or get a resource from the controller. Read-only.",
      inputSchema: {
        kind: z.string(),
        name: z.string().optional(),
        connection: z.object(connectionShape).optional(),
      },
      annotations: { readOnlyHint: true, openWorldHint: true },
    },
    async (input) => {
      try {
        const kind = findKind(input.kind);
        if (!kind) throw new InputError(`unknown kind ${input.kind}`);
        const call = buildRead(kind, input.name);
        const response = await execute(call, connectionOf(input.connection));
        return text({ ok: true, method: call.method, response });
      } catch (error) {
        return failure(error);
      }
    },
  );

  server.registerTool(
    "kcore_delete",
    {
      title: "Delete a kcore resource",
      description:
        "Destroy a resource, like terraform destroy. Cluster-update delete cancels a non-terminal update and keeps history. Requires confirm.",
      inputSchema: {
        kind: z.string(),
        name: z.string(),
        confirm: z.boolean().describe("True only after the operator agreed to delete this resource."),
        connection: z.object(connectionShape).optional(),
      },
      annotations: { readOnlyHint: false, destructiveHint: true, openWorldHint: true },
    },
    async (input, extra) => {
      try {
        const kind = findKind(input.kind);
        if (!kind) throw new InputError(`unknown kind ${input.kind}`);
        if (kind.id === "cluster") {
          return text({
            deleted: false,
            askTheUser:
              "Cluster delete removes local trust material. Ask the operator to edit ~/.kcore/config themselves. This tool will not delete certificates.",
          });
        }
        if (input.confirm !== true) {
          const accepted = await elicit(extra as ToolExtra, `Delete ${kind.title} ${input.name}?`, [
            {
              id: "confirm",
              prompt: `Delete ${kind.title} '${input.name}'?`,
              why: kind.id === "cluster-update" ? "This cancels a non-terminal update and keeps history." : "The controller deletes the resource.",
              required: true,
              choices: ["yes", "no"],
            },
          ]);
          if (accepted?.confirm !== "yes") {
            return text({
              deleted: false,
              askTheUser: `Ask the operator before deleting ${kind.title} '${input.name}'. Call kcore_delete with confirm true after they agree.`,
            });
          }
        }
        const call = buildDelete(kind, input.name);
        const response = await execute(call, connectionOf(input.connection));
        return text({ deleted: true, method: call.method, response });
      } catch (error) {
        return failure(error);
      }
    },
  );

  server.registerTool(
    "kcore_operation",
    {
      title: "Run a kcore day-2 operation",
      description:
        "Power state, migrate, drain, cordon, node approval, cluster-update approve/cancel/rollback, or node install. Asks before any disk wipe or mutation.",
      inputSchema: {
        action: z.string().describe(
          "set-vm-state, migrate-vm, drain-node, cordon-node, uncordon-node, approve-node, reject-node, approve-update, cancel-update, rollback-update, or install-node",
        ),
        spec: z.record(z.string(), z.unknown()),
        confirm: z.boolean(),
        connection: z.object(connectionShape).optional(),
      },
      annotations: { readOnlyHint: false, destructiveHint: true, openWorldHint: true },
    },
    async (input, extra) => {
      try {
        if (input.action === "install-node") {
          const spec = input.spec;
          const questions: Question[] = [];
          if (!spec.node) {
            questions.push({
              id: "node",
              prompt: "What is the node-agent address (host:port)?",
              why: "Install talks to the node directly.",
              required: true,
              example: "10.0.0.21:9091",
            });
          }
          if (!spec.osDisk) {
            questions.push({
              id: "osDisk",
              prompt: "Which disk should KcoreOS install onto?",
              why: "This disk is partitioned for the OS. Confirm the device with the operator.",
              required: true,
              example: "/dev/sda",
            });
          }
          if (!spec.joinController) {
            questions.push({
              id: "joinController",
              prompt: "Which controller should the node join (host:port)?",
              why: "The node registers with this controller after install.",
              required: true,
              example: "10.0.0.10:9090",
            });
          }
          if (questions.length > 0) {
            const answers = await elicit(extra as ToolExtra, "A few answers before installing a node.", questions);
            if (!answers) return text({ applied: false, blocking: questions, askTheUser: "Ask the operator these questions before installing a node." });
            Object.assign(spec, answers);
          }
          if (input.spec.acknowledge !== "wipe the selected disks") {
            return text({
              applied: false,
              askTheUser:
                "Node install partitions the selected disks. Ask the operator to confirm the exact devices, then call again with spec.acknowledge set to 'wipe the selected disks' and confirm true.",
            });
          }
        }
        if (input.confirm !== true) {
          const accepted = await elicit(extra as ToolExtra, `Run ${input.action}?`, [
            {
              id: "confirm",
              prompt: `Run ${input.action} now?`,
              why: "This changes the cluster.",
              required: true,
              choices: ["yes", "no"],
            },
          ]);
          if (accepted?.confirm !== "yes") {
            return text({
              applied: false,
              askTheUser: `Ask the operator before running ${input.action}. Call kcore_operation with confirm true after they agree.`,
            });
          }
        }
        if (input.action === "install-node") {
          const boot = bootstrapCertRequest(input.spec);
          const issued = await callController(connectionOf(input.connection), "issueNodeBootstrapCert", {
            nodeId: boot.nodeId,
            nodeHost: boot.nodeHost,
          }) as { certPem?: string; keyPem?: string; success?: boolean; message?: string };
          if (issued.success === false) {
            return text({ applied: false, error: issued.message || "the controller refused the node bootstrap certificate" });
          }
          const call = buildOperation("install-node", input.spec);
          const material = resolveConnection(connectionOf(input.connection));
          call.request.caCertPem = material.ca?.toString("utf8") ?? "";
          call.request.nodeCertPem = issued.certPem ?? "";
          call.request.nodeKeyPem = issued.keyPem ?? "";
          const response = await execute(call, { ...connectionOf(input.connection), timeoutMs: 600_000 });
          return text({ applied: true, method: call.method, response });
        }
        const call = buildOperation(input.action, input.spec);
        const response = await execute(call, connectionOf(input.connection));
        return text({ applied: true, method: call.method, response });
      } catch (error) {
        return failure(error);
      }
    },
  );

  server.registerPrompt(
    "create-vm",
    {
      title: "Create a VM",
      description: "Interview the operator, plan a VM, and apply only after they agree.",
    },
    async () => ({
      messages: [
        {
          role: "user" as const,
          content: {
            type: "text" as const,
            text: "Help me create a kcore VM. Call kcore_advise for kind vm, ask me every question it returns, then kcore_plan, and wait for my yes before kcore_apply.",
          },
        },
      ],
    }),
  );

  server.registerPrompt(
    "bootstrap-cluster",
    {
      title: "Bootstrap a cluster",
      description: "Create a kcore context, then walk through the first network and VM.",
    },
    async () => ({
      messages: [
        {
          role: "user" as const,
          content: {
            type: "text" as const,
            text: "Help me bootstrap a kcore cluster. Start with kcore_advise for kind cluster, ask for the controller address, plan, and apply the context only after I agree. Then ask whether I want a network and a first VM.",
          },
        },
      ],
    }),
  );

  server.registerResource(
    "catalog",
    "kcore://catalog",
    {
      description: "Resource kinds, mutable fields, and immutable fields.",
      mimeType: "application/json",
    },
    async (uri) => ({
      contents: [
        {
          uri: uri.href,
          mimeType: "application/json",
          text: JSON.stringify(
            KINDS.map((kind) => ({
              id: kind.id,
              summary: kind.summary,
              mutable: kind.mutable,
              immutable: kind.immutable,
            })),
          ),
        },
      ],
    }),
  );

  return server;
}

async function prepare(
  input: {
    kind?: string;
    intent?: string;
    spec?: Record<string, unknown>;
    manifest?: string;
    acceptDefaults?: boolean;
  },
  extra: ToolExtra,
): Promise<{ ready: boolean; desired?: ReturnType<typeof normalizeDesired>["desired"]; advice: ReturnType<typeof advise> }> {
  let spec = { ...(input.spec ?? {}) };
  let adviceResult = advise({ ...input, spec });
  if (!adviceResult.ready && adviceResult.blocking.length > 0) {
    const answers = await elicit(
      extra,
      "kcore needs a few answers before it will plan or apply this change.",
      adviceResult.blocking,
    );
    if (answers) {
      spec = mergeAnswers(spec, answers);
      adviceResult = advise({ ...input, spec });
    }
  }
  const normalized = normalizeDesired({ ...input, spec });
  if (normalized.kindError && !normalized.desired) {
    return { ready: false, advice: adviceResult };
  }
  return { ready: adviceResult.ready, desired: normalized.desired, advice: adviceResult };
}
