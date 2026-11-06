//! The tools as the model sees them. `confirm` is absent everywhere: the host asks the operator
//! and sets it. `connection` only accepts the node-agent address.

use serde_json::{json, Value};

use super::rpc::SERVICE_NAMES;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

pub const INSTRUCTIONS: &str = "kcore tools dial the kcore controller gRPC API with the operator's certificate. Apply is a declarative upsert: it creates a missing resource, updates mutable fields, and rejects immutable changes.

Before every create, update, delete, migrate, drain, or node install:
1. Call kcore_advise and ask the operator every blocking question. Wait for the answers.
2. Also ask the recommended questions unless the operator said to use defaults.
3. Call kcore_plan and show that plan.
4. Call kcore_apply or kcore_delete only after the operator explicitly agrees. The operator approves in the UI.

Ask for image URLs, checksums, addresses, disk devices, and SSH keys. Use kcore_catalog when you are unsure which resource matches the request. kcore_rpc calls any other implemented unary controller, node, or kcore-provisioning method. Provisioning covers images, machines, boot targets, Redfish discovery, BMC boot, and power control. Pass a method that is not a get, list, classify, check, or plan only after the operator agrees.";

fn object() -> Value {
    json!({ "type": "object", "additionalProperties": true })
}

fn connection() -> Value {
    json!({
        "type": "object",
        "description": "Only node is accepted: the node-agent host:port for node-scoped RPCs. Everything else comes from the operator's context.",
        "properties": { "node": { "type": "string" } },
        "additionalProperties": false,
    })
}

fn text(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

pub fn tool_defs() -> Vec<ToolDef> {
    let spec_answers = || {
        let mut value = object();
        value["description"] = json!("Answers collected so far.");
        value
    };
    vec![
        ToolDef {
            name: "kcore_catalog",
            description: "List the resources this server can plan and apply, including which fields are mutable. Read-only.",
            parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "kcore_rpc",
            description: "Call one implemented unary RPC on the controller, a node, or kcore-provisioning. Omit method to list them. Provisioning methods need the operator to start kctl agent with --provisioning. Request fields use proto JSON names in camelCase. Streaming RPCs and methods that always return UNIMPLEMENTED are not listed. Mutations are shown to the operator for approval.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "enum": SERVICE_NAMES, "description": "gRPC service. Required when method is set." },
                    "method": text("CamelCase RPC name, such as listNodes or getPkiStatus."),
                    "request": { "type": "object", "additionalProperties": true, "description": "RPC request message. Empty for methods with an empty request." },
                    "connection": connection(),
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kcore_advise",
            description: "Return the questions the operator must answer before a cluster, VM, or other kcore change. Call this first. Read-only.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "intent": text("What the operator wants, in their words."),
                    "kind": text("Resource id such as vm, network, or cluster."),
                    "spec": spec_answers(),
                    "manifest": text("YAML manifest, when the operator already has one."),
                    "acceptDefaults": { "type": "boolean", "description": "Skip recommended questions." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kcore_plan",
            description: "Terraform-style plan. If required answers are missing, returns the questions instead of contacting the cluster. With checkCluster, calls the controller read, ClassifyDiskLayout, or PlanClusterUpdate RPC.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string" },
                    "intent": { "type": "string" },
                    "spec": object(),
                    "manifest": { "type": "string" },
                    "current": { "type": ["object", "null"], "additionalProperties": true, "description": "Existing spec from a previous read. Omit when unknown." },
                    "acceptDefaults": { "type": "boolean" },
                    "checkCluster": { "type": "boolean", "description": "Call the controller to read current state or run a plan RPC." },
                    "connection": connection(),
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kcore_apply",
            description: "Declarative apply, like terraform apply. Refuses to run until blocking questions are answered. The operator approves the change in the UI.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string" },
                    "intent": { "type": "string" },
                    "spec": object(),
                    "manifest": { "type": "string" },
                    "acceptDefaults": { "type": "boolean" },
                    "confirmReplace": { "type": "boolean", "description": "True only when the operator agreed to delete and recreate an immutable change." },
                    "current": { "type": ["object", "null"], "additionalProperties": true },
                    "connection": connection(),
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kcore_read",
            description: "List or get a resource from the controller. Read-only.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string" },
                    "name": { "type": "string" },
                    "connection": connection(),
                },
                "required": ["kind"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kcore_delete",
            description: "Destroy a resource, like terraform destroy. Cluster-update delete cancels a non-terminal update and keeps history. The operator approves in the UI.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string" },
                    "name": { "type": "string" },
                    "connection": connection(),
                },
                "required": ["kind", "name"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kcore_operation",
            description: "Power state, migrate, drain, cordon, node approval, cluster-update approve/cancel/rollback, or node install. Asks before any disk wipe or mutation.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": text("set-vm-state, migrate-vm, drain-node, cordon-node, uncordon-node, approve-node, reject-node, approve-update, cancel-update, rollback-update, or install-node"),
                    "spec": object(),
                    "connection": connection(),
                },
                "required": ["action", "spec"],
                "additionalProperties": false,
            }),
        },
    ]
}
