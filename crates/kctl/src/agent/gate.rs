//! What the model may ask for, and what the operator must approve first.

use serde_json::{json, Map, Value};

use super::tools::needs_host_approval;

pub struct HostApproval {
    pub title: String,
    pub detail: Value,
}

pub struct GatedCall {
    pub arguments: Map<String, Value>,
    /// The host must show an approval card before calling, because the tool will not ask.
    pub host_approval: Option<HostApproval>,
    /// Arguments to send once approved. Equals `arguments` when no host approval is needed.
    pub approved_arguments: Map<String, Value>,
}

const CONFIRMED_BY_TOOL_FORM: [&str; 3] = ["kcore_apply", "kcore_delete", "kcore_operation"];

pub fn gate(name: &str, raw: &Value) -> GatedCall {
    let mut input: Map<String, Value> = raw.as_object().cloned().unwrap_or_default();
    let node = input
        .get("connection")
        .and_then(|c| c["node"].as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    match node {
        Some(node) => {
            input.insert("connection".to_string(), json!({ "node": node }));
        }
        None => {
            input.remove("connection");
        }
    }

    if CONFIRMED_BY_TOOL_FORM.contains(&name) {
        input.insert("confirm".to_string(), json!(false));
        if name == "kcore_operation"
            && input.get("action").and_then(Value::as_str) == Some("install-node")
        {
            let mut spec = input
                .get("spec")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            spec.remove("acknowledge");
            let mut approved = input.clone();
            approved.insert("confirm".to_string(), json!(true));
            return GatedCall {
                arguments: input.clone(),
                host_approval: Some(HostApproval {
                    title: "Install kcore on a node and wipe its disks?".to_string(),
                    detail: json!({ "action": input["action"], "spec": spec }),
                }),
                approved_arguments: approved,
            };
        }
        return GatedCall {
            approved_arguments: input.clone(),
            arguments: input,
            host_approval: None,
        };
    }

    if name == "kcore_rpc" {
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if needs_host_approval(&method) {
            let service = input
                .get("service")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut shown = input.clone();
            shown.insert("confirm".to_string(), json!(false));
            let mut approved = input.clone();
            approved.insert("confirm".to_string(), json!(true));
            return GatedCall {
                arguments: shown,
                host_approval: Some(HostApproval {
                    title: format!("Call {service}.{method}?"),
                    detail: json!({
                        "service": service,
                        "method": method,
                        "request": input.get("request").cloned().unwrap_or_else(|| json!({})),
                        "node": input.get("connection").and_then(|c| c.get("node")).cloned(),
                    }),
                }),
                approved_arguments: approved,
            };
        }
        input.remove("confirm");
    }
    GatedCall {
        approved_arguments: input.clone(),
        arguments: input,
        host_approval: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_cannot_pick_the_controller_or_tls_settings() {
        let out = gate(
            "kcore_read",
            &json!({ "kind": "vm", "connection": { "controller": "evil:9090", "insecure": true, "config": "/tmp/x", "node": "10.0.0.5:9091" } }),
        );
        assert_eq!(
            out.arguments["connection"],
            json!({ "node": "10.0.0.5:9091" })
        );
        let none = gate(
            "kcore_read",
            &json!({ "kind": "vm", "connection": { "controller": "evil:9090" } }),
        );
        assert!(!none.arguments.contains_key("connection"));
    }

    #[test]
    fn apply_delete_and_operation_always_go_to_the_tool_with_confirm_false() {
        for name in ["kcore_apply", "kcore_delete", "kcore_operation"] {
            let out = gate(
                name,
                &json!({ "confirm": true, "kind": "vm", "name": "a", "action": "set-vm-state", "spec": {} }),
            );
            assert_eq!(out.arguments["confirm"], false, "{name}");
            assert!(out.host_approval.is_none(), "{name}");
        }
    }

    #[test]
    fn node_install_and_mutating_rpcs_need_a_host_approval_reads_do_not() {
        let install = gate(
            "kcore_operation",
            &json!({ "action": "install-node", "spec": { "acknowledge": "wipe the selected disks", "disks": ["/dev/sda"] }, "confirm": true }),
        );
        let approval = install.host_approval.expect("install needs approval");
        assert_eq!(install.arguments["confirm"], false);
        assert_eq!(install.approved_arguments["confirm"], true);
        assert!(!approval
            .detail
            .to_string()
            .contains("wipe the selected disks"));

        let write = gate(
            "kcore_rpc",
            &json!({ "service": "controller", "method": "createVm", "request": {}, "confirm": true }),
        );
        assert!(write.host_approval.is_some());
        assert_eq!(write.arguments["confirm"], false);
        assert_eq!(write.approved_arguments["confirm"], true);

        let read = gate(
            "kcore_rpc",
            &json!({ "service": "controller", "method": "listNodes", "confirm": true }),
        );
        assert!(read.host_approval.is_none());
        assert!(!read.arguments.contains_key("confirm"));
    }
}
