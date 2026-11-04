//! The kcore tools the model can call. Same behaviour as the kcore MCP server, in-process.

pub mod advise;
pub mod apps;
pub mod catalog;
pub mod names;
pub mod options;
pub mod plan;
pub mod requests;
pub mod rpc;
pub mod schema;

use std::path::PathBuf;
use std::time::Duration;

use crate::config::ConnectionInfo;
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};

use advise::{
    advise, merge_answers, normalize_desired, Advice, AdviseInput, Desired, OptionSource, Question,
};
use catalog::{find_kind, KINDS};
use names::{assert_address, assert_arg, assert_name, boolish, opt, required, InputError, Spec};
use options::{resolve_questions, ClusterOption};
use plan::diff_spec;
use requests::{
    bootstrap_cert_request, build_apply, build_delete, build_operation, build_plan_call,
    build_read, RpcCall, Target,
};
use rpc::{is_read_only_method, redact, require_rpc, rpc_catalog, Endpoints, ServiceName};

pub use schema::{tool_defs, INSTRUCTIONS};

const OPTION_TIMEOUT: Duration = Duration::from_secs(8);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

pub enum FormReply {
    Accept(Map<String, Value>),
    Decline,
    Cancel,
}

/// How a tool asks the operator something. The session turns the schema into a form in the UI.
pub trait Ask: Send + Sync {
    fn form<'a>(&'a self, message: &'a str, schema: Value) -> BoxFuture<'a, FormReply>;
}

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

pub struct Toolbox {
    pub endpoints: Endpoints,
    pub config_path: PathBuf,
    /// Why this cluster cannot be reached at all (bad config, missing certificates). Every call that
    /// needs the cluster answers with it.
    pub unavailable: Option<String>,
}

fn pretty(payload: &Value) -> ToolOutput {
    ToolOutput {
        text: serde_json::to_string_pretty(&redact(payload)).unwrap_or_else(|_| "{}".to_string()),
        is_error: false,
    }
}

fn failure(error: impl std::fmt::Display) -> ToolOutput {
    pretty(&json!({ "ok": false, "error": error.to_string() }))
}

fn bad<T>(message: impl Into<String>) -> anyhow::Result<T> {
    Err(InputError(message.into()).into())
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn form_schema(questions: &[Question]) -> Value {
    let mut properties = Map::new();
    let mut needed = Vec::new();
    for item in questions {
        let mut field = Map::new();
        if let Some(choices) = &item.choices {
            field.insert("type".into(), json!("string"));
            field.insert("title".into(), json!(item.prompt));
            field.insert("description".into(), json!(item.why));
            field.insert("enum".into(), json!(choices));
            if let Some(labels) = item
                .choice_labels
                .as_ref()
                .filter(|l| l.len() == choices.len())
            {
                field.insert("enumNames".into(), json!(labels));
            }
        } else if let Some(kind) = item.kind.as_deref() {
            field.insert("type".into(), json!(kind));
            field.insert("title".into(), json!(item.prompt));
            field.insert("description".into(), json!(item.why));
            if let Some(min) = item.min {
                field.insert("minimum".into(), json!(min));
            }
        } else {
            field.insert("type".into(), json!("string"));
            field.insert("title".into(), json!(item.prompt));
            field.insert("description".into(), json!(item.why));
        }
        properties.insert(item.id.clone(), Value::Object(field));
        if item.required {
            needed.push(item.id.clone());
        }
    }
    json!({ "type": "object", "properties": properties, "required": needed })
}

async fn elicit(
    ask: &dyn Ask,
    message: &str,
    questions: &[Question],
) -> Option<Map<String, Value>> {
    if questions.is_empty() {
        return None;
    }
    match ask.form(message, form_schema(questions)).await {
        FormReply::Accept(content) => Some(content),
        _ => None,
    }
}

async fn approved(ask: &dyn Ask, message: &str, prompt: &str, why: &str) -> bool {
    let mut question = Question::new("confirm", prompt, why, true);
    question.choices = Some(vec!["yes".to_string(), "no".to_string()]);
    elicit(ask, message, &[question])
        .await
        .and_then(|answers| answers.get("confirm").cloned())
        .is_some_and(|value| value == "yes")
}

fn node_agent_address(address: &str) -> String {
    if let Some(rest) = address.strip_prefix('[') {
        return match rest.find(']') {
            Some(end) => {
                let host = &rest[..end];
                let port = &rest[end + 1..];
                if port.is_empty() || port == ":9090" {
                    format!("[{host}]:9091")
                } else {
                    address.to_string()
                }
            }
            None => address.to_string(),
        };
    }
    match address.rfind(':') {
        None | Some(0) => format!("{address}:9091"),
        Some(split) => {
            if &address[split + 1..] == "9090" {
                format!("{}:9091", &address[..split])
            } else {
                address.to_string()
            }
        }
    }
}

fn spec_arg(args: &Value, key: &str) -> Spec {
    args.get(key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn text_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool) == Some(true)
}

fn node_of(args: &Value) -> Option<String> {
    args.get("connection")
        .and_then(|c| c.get("node"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

struct Prepared {
    ready: bool,
    desired: Option<Desired>,
    advice: Advice,
}

impl Toolbox {
    pub fn new(endpoints: Endpoints, config_path: PathBuf) -> Self {
        Self {
            endpoints,
            config_path,
            unavailable: None,
        }
    }

    /// A cluster whose connection could not even be built.
    pub fn unavailable(reason: String, config_path: PathBuf) -> Self {
        let controller = ConnectionInfo {
            address: String::new(),
            addresses: Vec::new(),
            insecure: true,
            tls_server_name: None,
            cert_pem: None,
            key_pem: None,
            ca_pem: None,
            cert: None,
            key: None,
            ca: None,
        };
        let mut toolbox = Self::new(
            Endpoints {
                controller,
                provisioning: None,
                provisioning_tls: false,
                timeout: Duration::from_secs(1),
            },
            config_path,
        );
        toolbox.unavailable = Some(reason);
        toolbox
    }

    pub async fn call(&self, name: &str, args: &Value, ask: &dyn Ask) -> ToolOutput {
        if let Some(reason) = &self.unavailable {
            if !matches!(name, "kcore_advise" | "kcore_catalog") {
                return failure(format!("This cluster cannot be reached: {reason}"));
            }
        }
        let result = match name {
            "kcore_catalog" => Ok(self.catalog()),
            "kcore_rpc" => self.rpc_tool(args).await,
            "kcore_advise" => Ok(pretty(&to_value(&advise(&AdviseInput::from_args(args))))),
            "kcore_plan" => self.plan_tool(args, ask).await,
            "kcore_apply" => self.apply_tool(args, ask).await,
            "kcore_read" => self.read_tool(args).await,
            "kcore_delete" => self.delete_tool(args, ask).await,
            "kcore_operation" => self.operation_tool(args, ask).await,
            other => Err(anyhow::anyhow!("There is no tool named {other}.")),
        };
        result.unwrap_or_else(failure)
    }

    fn catalog(&self) -> ToolOutput {
        pretty(&json!({
            "resources": KINDS.iter().map(|kind| json!({
                "id": kind.id,
                "yamlKind": kind.yaml_kind,
                "summary": kind.summary,
                "mutable": kind.mutable,
                "immutable": kind.immutable,
            })).collect::<Vec<_>>(),
            "rpc": "Call kcore_rpc with no method to list every implemented unary gRPC method.",
        }))
    }

    async fn execute(&self, call: &RpcCall, endpoints: &Endpoints) -> anyhow::Result<Value> {
        match call.target {
            Target::Node => {
                let address = call
                    .address
                    .as_deref()
                    .ok_or_else(|| InputError("node address is required".to_string()))?;
                endpoints
                    .call(
                        ServiceName::NodeAdmin,
                        Some(address),
                        call.method,
                        &call.request,
                    )
                    .await
            }
            Target::Controller => {
                endpoints
                    .call(ServiceName::Controller, None, call.method, &call.request)
                    .await
            }
        }
    }

    async fn rpc_tool(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let service = text_arg(args, "service");
        let Some(method) = text_arg(args, "method").filter(|m| !m.is_empty()) else {
            let filter = match service.as_deref() {
                Some(name) => Some(
                    ServiceName::parse(name)
                        .ok_or_else(|| InputError(format!("unknown gRPC service '{name}'")))?,
                ),
                None => None,
            };
            let methods: Vec<Value> = rpc_catalog(filter)
                .into_iter()
                .map(|m| json!({ "service": m.service.id(), "method": m.method, "readOnly": m.read_only }))
                .collect();
            return Ok(pretty(&json!({ "ok": true, "methods": methods })));
        };
        let Some(service) = service else {
            return bad("service is required when method is set");
        };
        let rpc = require_rpc(&service, &method)?;
        if !rpc.read_only && !flag(args, "confirm") {
            return Ok(pretty(&json!({
                "ok": false,
                "applied": false,
                "service": rpc.service.id(),
                "method": rpc.method,
                "askTheUser": format!("Show {}.{} to the operator and wait for their approval.", rpc.service.id(), rpc.method),
            })));
        }
        let node = node_of(args);
        if rpc.service.is_node() && node.is_none() {
            return bad("connection.node is required for a node RPC (host:port, usually :9091)");
        }
        let request = args.get("request").cloned().unwrap_or_else(|| json!({}));
        let response = self
            .endpoints
            .call(rpc.service, node.as_deref(), &rpc.method, &request)
            .await?;
        Ok(pretty(&json!({
            "ok": true,
            "service": rpc.service.id(),
            "method": rpc.method,
            "response": response,
        })))
    }

    async fn load_option(&self, source: OptionSource) -> Option<Vec<ClusterOption>> {
        let short = Endpoints {
            timeout: OPTION_TIMEOUT,
            ..self.endpoints.clone()
        };
        let list = |method: &'static str| {
            let short = short.clone();
            async move {
                short
                    .call(ServiceName::Controller, None, method, &json!({}))
                    .await
                    .ok()
            }
        };
        let items = |response: &Value, key: &str| -> Vec<Value> {
            response
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let field = |item: &Value, key: &str| -> String {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match source {
            OptionSource::Networks => {
                let response = list("listNetworks").await?;
                Some(
                    items(&response, "networks")
                        .iter()
                        .map(|n| {
                            let name = field(n, "name");
                            let ip = field(n, "externalIp");
                            ClusterOption {
                                label: Some(if ip.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{name} ({ip})")
                                }),
                                value: name,
                            }
                        })
                        .collect(),
                )
            }
            OptionSource::SshKeys => {
                let response = list("listSshKeys").await?;
                Some(
                    items(&response, "keys")
                        .iter()
                        .map(|k| ClusterOption {
                            value: field(k, "name"),
                            label: None,
                        })
                        .collect(),
                )
            }
            OptionSource::Nodes => {
                let response = list("listNodes").await?;
                Some(
                    items(&response, "nodes")
                        .iter()
                        .map(|n| {
                            let id = field(n, "nodeId");
                            let parts: Vec<String> = [field(n, "hostname"), field(n, "status")]
                                .into_iter()
                                .filter(|p| !p.is_empty())
                                .collect();
                            ClusterOption {
                                label: Some(if parts.is_empty() {
                                    id.clone()
                                } else {
                                    format!("{id} ({})", parts.join(", "))
                                }),
                                value: id,
                            }
                        })
                        .collect(),
                )
            }
            OptionSource::Vms => {
                let response = list("listVms").await?;
                Some(
                    items(&response, "vms")
                        .iter()
                        .map(|v| {
                            let name = field(v, "name");
                            let state = field(v, "state").to_lowercase().replace("vm_state_", "");
                            ClusterOption {
                                label: Some(if state.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{name} ({state})")
                                }),
                                value: name,
                            }
                        })
                        .collect(),
                )
            }
            OptionSource::NodeImages => {
                let listed = self.read_images(&short, None, None).await.ok()?;
                let mut out = Vec::new();
                for node in listed["nodes"].as_array().cloned().unwrap_or_default() {
                    for image in node["images"].as_array().cloned().unwrap_or_default() {
                        out.push(ClusterOption {
                            value: field(&image, "path"),
                            label: None,
                        });
                    }
                }
                Some(out)
            }
        }
    }

    async fn read_images(
        &self,
        endpoints: &Endpoints,
        node: Option<&str>,
        name: Option<&str>,
    ) -> anyhow::Result<Value> {
        let wanted = name.map(str::trim).filter(|n| !n.is_empty());
        let targets: Vec<(String, String, String)> =
            match node.map(str::trim).filter(|n| !n.is_empty()) {
                Some(address) => vec![(String::new(), String::new(), node_agent_address(address))],
                None => {
                    let response = endpoints
                        .call(ServiceName::Controller, None, "listNodes", &json!({}))
                        .await?;
                    response["nodes"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|n| {
                            let address = n["address"].as_str().filter(|a| !a.is_empty())?;
                            Some((
                                n["nodeId"].as_str().unwrap_or("").to_string(),
                                n["hostname"].as_str().unwrap_or("").to_string(),
                                node_agent_address(address),
                            ))
                        })
                        .collect()
                }
            };
        let mut listed = Vec::new();
        for (node_id, hostname, address) in targets {
            let mut entry = json!({ "nodeId": node_id, "hostname": hostname, "address": address });
            match endpoints
                .call(
                    ServiceName::NodeCompute,
                    Some(&address),
                    "listImages",
                    &json!({}),
                )
                .await
            {
                Ok(response) => {
                    let images: Vec<Value> = response["images"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|image| {
                            wanted.is_none_or(|w| image["name"] == w || image["path"] == w)
                        })
                        .collect();
                    entry["images"] = json!(images);
                }
                Err(error) => entry["error"] = json!(error.to_string()),
            }
            listed.push(entry);
        }
        Ok(json!({ "nodes": listed }))
    }

    async fn prepare(&self, input: &AdviseInput, ask: &dyn Ask) -> Prepared {
        let mut input = input.clone();
        let mut advice = advise(&input);
        if !advice.ready && !advice.blocking.is_empty() {
            let questions =
                resolve_questions(advice.blocking.clone(), |source| self.load_option(source)).await;
            if let Some(answers) = elicit(
                ask,
                "kcore needs a few answers before it will plan or apply this change.",
                &questions,
            )
            .await
            {
                input.spec = merge_answers(&input.spec, &answers);
                advice = advise(&input);
            }
        }
        let normalized = normalize_desired(&input);
        if normalized.kind_error.is_some() && normalized.desired.is_none() {
            return Prepared {
                ready: false,
                desired: None,
                advice,
            };
        }
        Prepared {
            ready: advice.ready,
            desired: normalized.desired,
            advice,
        }
    }

    async fn plan_tool(&self, args: &Value, ask: &dyn Ask) -> anyhow::Result<ToolOutput> {
        let prepared = self.prepare(&AdviseInput::from_args(args), ask).await;
        let Some(desired) = prepared.desired.filter(|_| prepared.ready) else {
            return Ok(pretty(&to_value(&prepared.advice)));
        };
        if desired.kind.id == "cluster" {
            return Ok(pretty(&json!({
                "ok": true,
                "kind": "cluster",
                "plan": {
                    "action": "create",
                    "summary": "Generate a cluster CA, a controller certificate for this address, and a client certificate. The context is saved for later gRPC calls. Private keys stay on disk.",
                },
                "files": ["ca.crt", "ca.key", "sub-ca.crt", "sub-ca.key", "controller.crt", "controller.key", "kctl.crt", "kctl.key"],
                "warnings": prepared.advice.warnings,
                "next": "Show this plan to the operator. Call kcore_apply only after they agree.",
            })));
        }
        let current = args.get("current").and_then(Value::as_object);
        let local = diff_spec(desired.kind, &desired.spec, current);
        let mut cluster = Value::Null;
        if flag(args, "checkCluster") {
            let call = build_plan_call(desired.kind, &desired.spec, None)?;
            match self.execute(&call, &self.endpoints).await {
                Ok(response) => cluster = response,
                Err(error) => {
                    let message = error.to_string();
                    if message.to_lowercase().contains("not found") {
                        cluster = json!({ "found": false, "message": message });
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        let rpc: Vec<&str> = build_apply(desired.kind, &desired.spec)?
            .iter()
            .map(|call| call.method)
            .collect();
        Ok(pretty(&json!({
            "ok": true,
            "kind": desired.kind.id,
            "plan": local,
            "rpc": rpc,
            "warnings": prepared.advice.warnings,
            "recommendedQuestions": prepared.advice.recommended,
            "cluster": cluster,
            "next": "Show this plan to the operator. Call kcore_apply only after they agree.",
        })))
    }

    async fn apply_tool(&self, args: &Value, ask: &dyn Ask) -> anyhow::Result<ToolOutput> {
        let prepared = self.prepare(&AdviseInput::from_args(args), ask).await;
        let Some(desired) = prepared.desired.filter(|_| prepared.ready) else {
            let mut payload = to_value(&prepared.advice);
            payload["applied"] = json!(false);
            return Ok(pretty(&payload));
        };
        let current = args.get("current").and_then(Value::as_object);
        let local = diff_spec(desired.kind, &desired.spec, current);
        if local.action == "replace" && !flag(args, "confirmReplace") {
            return Ok(pretty(&json!({
                "applied": false,
                "plan": local,
                "askTheUser": "This change replaces the resource. Ask the operator, then call kcore_apply again with confirmReplace true.",
            })));
        }
        if !flag(args, "confirm")
            && !approved(
                ask,
                "Apply this kcore change now?",
                "Apply this change now?",
                &local.summary,
            )
            .await
        {
            return Ok(pretty(&json!({
                "applied": false,
                "plan": local,
                "warnings": prepared.advice.warnings,
                "askTheUser": "Show the plan and ask the operator to agree. Call kcore_apply again after they do.",
            })));
        }
        if desired.kind.id == "cluster" {
            let created = self.create_cluster_context(&desired.spec).await?;
            let mut payload = json!({ "applied": true, "plan": local });
            if let (Value::Object(into), Value::Object(from)) = (&mut payload, created) {
                into.extend(from);
            }
            return Ok(pretty(&payload));
        }
        let mut responses = Vec::new();
        for call in build_apply(desired.kind, &desired.spec)? {
            let response = self.execute(&call, &self.endpoints).await?;
            responses.push(json!({ "method": call.method, "response": response }));
        }
        Ok(pretty(
            &json!({ "applied": true, "plan": local, "responses": responses }),
        ))
    }

    async fn create_cluster_context(&self, spec: &Spec) -> anyhow::Result<Value> {
        let controller = assert_address(
            &required(spec, "controller", "controller address")?,
            "controller",
        )?;
        let context = assert_name(
            &opt(spec, "name").unwrap_or_else(|| "default".to_string()),
            "context name",
        )?;
        let force = boolish(spec.get("force")) || opt(spec, "force").as_deref() == Some("yes");
        let certs_dir = match opt(spec, "certsDir") {
            Some(dir) => PathBuf::from(assert_arg(&dir, "certs directory")?),
            None => crate::config::default_cluster_certs_dir(&context),
        };
        let config_path = self.config_path.clone();
        let (controller_for_task, context_for_task, certs_for_task) =
            (controller.clone(), context.clone(), certs_dir.clone());
        tokio::task::spawn_blocking(move || {
            crate::commands::cluster::create(
                &config_path,
                &controller_for_task,
                &certs_for_task,
                &context_for_task,
                force,
            )
        })
        .await??;
        Ok(json!({
            "context": context,
            "controller": controller,
            "configPath": self.config_path.display().to_string(),
            "certsDir": certs_dir.display().to_string(),
            "files": ["ca.crt", "ca.key", "sub-ca.crt", "sub-ca.key", "controller.crt", "controller.key", "kctl.crt", "kctl.key"],
            "note": "Restart kctl agent to use this context. This session still talks to the controller it started with.",
        }))
    }

    async fn read_tool(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let kind_name = text_arg(args, "kind").unwrap_or_default();
        let Some(kind) = find_kind(Some(&kind_name)) else {
            return bad(format!("unknown kind {kind_name}"));
        };
        let name = text_arg(args, "name");
        if kind.id == "image" {
            let response = self
                .read_images(&self.endpoints, node_of(args).as_deref(), name.as_deref())
                .await?;
            return Ok(pretty(
                &json!({ "ok": true, "method": "listImages", "response": response }),
            ));
        }
        let call = build_read(kind, name.as_deref())?;
        let response = self.execute(&call, &self.endpoints).await?;
        Ok(pretty(
            &json!({ "ok": true, "method": call.method, "response": response }),
        ))
    }

    async fn delete_tool(&self, args: &Value, ask: &dyn Ask) -> anyhow::Result<ToolOutput> {
        let kind_name = text_arg(args, "kind").unwrap_or_default();
        let Some(kind) = find_kind(Some(&kind_name)) else {
            return bad(format!("unknown kind {kind_name}"));
        };
        let name = text_arg(args, "name").unwrap_or_default();
        if kind.id == "cluster" {
            return Ok(pretty(&json!({
                "deleted": false,
                "askTheUser": "Cluster delete removes local trust material. Ask the operator to edit ~/.kcore/config themselves. This tool will not delete certificates.",
            })));
        }
        if !flag(args, "confirm") {
            let why = if kind.id == "cluster-update" {
                "This cancels a non-terminal update and keeps history."
            } else {
                "The controller deletes the resource."
            };
            if !approved(
                ask,
                &format!("Delete {} {name}?", kind.title),
                &format!("Delete {} '{name}'?", kind.title),
                why,
            )
            .await
            {
                return Ok(pretty(&json!({
                    "deleted": false,
                    "askTheUser": format!("Ask the operator before deleting {} '{name}'.", kind.title),
                })));
            }
        }
        let call = build_delete(kind, &name)?;
        let response = self.execute(&call, &self.endpoints).await?;
        Ok(pretty(
            &json!({ "deleted": true, "method": call.method, "response": response }),
        ))
    }

    async fn operation_tool(&self, args: &Value, ask: &dyn Ask) -> anyhow::Result<ToolOutput> {
        let action = text_arg(args, "action").unwrap_or_default();
        let mut spec = spec_arg(args, "spec");
        if action == "install-node" {
            let mut questions: Vec<Question> = Vec::new();
            let mut need = |id: &str, prompt: &str, why: &str, example: &str| {
                let mut q = Question::new(id, prompt, why, true);
                q.example = Some(example.to_string());
                questions.push(q);
            };
            if !spec.contains_key("node") || spec["node"].is_null() || spec["node"] == "" {
                need(
                    "node",
                    "What is the node-agent address (host:port)?",
                    "Install talks to the node directly.",
                    "10.0.0.21:9091",
                );
            }
            if !spec.contains_key("osDisk") || spec["osDisk"].is_null() || spec["osDisk"] == "" {
                need(
                    "osDisk",
                    "Which disk should KcoreOS install onto?",
                    "This disk is partitioned for the OS. Confirm the device with the operator.",
                    "/dev/sda",
                );
            }
            if !spec.contains_key("joinController")
                || spec["joinController"].is_null()
                || spec["joinController"] == ""
            {
                need(
                    "joinController",
                    "Which controller should the node join (host:port)?",
                    "The node registers with this controller after install.",
                    "10.0.0.10:9090",
                );
            }
            if !questions.is_empty() {
                let Some(answers) =
                    elicit(ask, "A few answers before installing a node.", &questions).await
                else {
                    return Ok(pretty(&json!({
                        "applied": false,
                        "blocking": questions,
                        "askTheUser": "Ask the operator these questions before installing a node.",
                    })));
                };
                spec.extend(answers);
            }
            if spec.get("acknowledge").and_then(Value::as_str) != Some("wipe the selected disks") {
                return Ok(pretty(&json!({
                    "applied": false,
                    "askTheUser": "Node install partitions the selected disks. Ask the operator to confirm the exact devices, then call again with spec.acknowledge set to 'wipe the selected disks'.",
                })));
            }
        }
        if !flag(args, "confirm")
            && !approved(
                ask,
                &format!("Run {action}?"),
                &format!("Run {action} now?"),
                "This changes the cluster.",
            )
            .await
        {
            return Ok(pretty(&json!({
                "applied": false,
                "askTheUser": format!("Ask the operator before running {action}."),
            })));
        }
        if action == "install-node" {
            let boot = bootstrap_cert_request(&spec)?;
            let issued = self
                .endpoints
                .call(
                    ServiceName::Controller,
                    None,
                    "issueNodeBootstrapCert",
                    &json!({ "nodeId": boot.node_id, "nodeHost": boot.node_host }),
                )
                .await?;
            if issued["success"] == false {
                let message = issued["message"]
                    .as_str()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("the controller refused the node bootstrap certificate");
                return Ok(pretty(&json!({ "applied": false, "error": message })));
            }
            let mut call = build_operation("install-node", &spec)?;
            call.request["caCertPem"] = json!(self.endpoints.ca_pem().unwrap_or_default());
            call.request["nodeCertPem"] = json!(issued["certPem"].as_str().unwrap_or(""));
            call.request["nodeKeyPem"] = json!(issued["keyPem"].as_str().unwrap_or(""));
            let long = Endpoints {
                timeout: INSTALL_TIMEOUT,
                ..self.endpoints.clone()
            };
            let response = self.execute(&call, &long).await?;
            return Ok(pretty(
                &json!({ "applied": true, "method": call.method, "response": response }),
            ));
        }
        let call = build_operation(&action, &spec)?;
        let response = self.execute(&call, &self.endpoints).await?;
        Ok(pretty(
            &json!({ "applied": true, "method": call.method, "response": response }),
        ))
    }
}

/// Methods the host must put in front of the operator before the tool runs.
pub fn needs_host_approval(method: &str) -> bool {
    !method.is_empty() && !is_read_only_method(method)
}

#[cfg(test)]
impl Toolbox {
    /// A toolbox whose controller refuses connections, for tests that never need one.
    pub fn unreachable() -> Self {
        let info = ConnectionInfo {
            address: "127.0.0.1:1".to_string(),
            addresses: vec!["127.0.0.1:1".to_string()],
            insecure: true,
            tls_server_name: None,
            cert_pem: None,
            key_pem: None,
            ca_pem: None,
            cert: None,
            key: None,
            ca: None,
        };
        Self::new(
            Endpoints {
                controller: info,
                provisioning: None,
                provisioning_tls: false,
                timeout: Duration::from_secs(1),
            },
            PathBuf::from("/nonexistent/config"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Declines;

    impl Ask for Declines {
        fn form<'a>(&'a self, _message: &'a str, _schema: Value) -> BoxFuture<'a, FormReply> {
            Box::pin(async { FormReply::Decline })
        }
    }

    struct Records(std::sync::Mutex<Vec<Value>>);

    impl Ask for Records {
        fn form<'a>(&'a self, _message: &'a str, schema: Value) -> BoxFuture<'a, FormReply> {
            self.0.lock().unwrap().push(schema);
            Box::pin(async { FormReply::Decline })
        }
    }

    fn toolbox() -> Toolbox {
        Toolbox::unreachable()
    }

    #[test]
    fn every_documented_tool_is_defined_without_a_confirm_property() {
        let names: Vec<&str> = tool_defs().iter().map(|t| t.name).collect();
        for wanted in [
            "kcore_advise",
            "kcore_plan",
            "kcore_apply",
            "kcore_read",
            "kcore_delete",
            "kcore_operation",
            "kcore_catalog",
            "kcore_rpc",
        ] {
            assert!(names.contains(&wanted), "{wanted}");
        }
        for tool in tool_defs() {
            assert!(
                tool.parameters["properties"].get("confirm").is_none(),
                "{}",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn advise_lists_the_questions_and_apply_refuses_open_ones() {
        let toolbox = toolbox();
        let advice = toolbox
            .call(
                "kcore_advise",
                &json!({"kind": "vm", "spec": {}}),
                &Declines,
            )
            .await;
        assert!(advice.text.contains("Ask the operator"));
        let apply = toolbox
            .call(
                "kcore_apply",
                &json!({"kind": "vm", "spec": {}, "confirm": true}),
                &Declines,
            )
            .await;
        assert!(apply.text.contains("\"applied\": false"), "{}", apply.text);
    }

    #[tokio::test]
    async fn open_questions_are_put_to_the_operator_as_a_form() {
        let toolbox = toolbox();
        let ask = Records(Default::default());
        toolbox
            .call("kcore_plan", &json!({"kind": "ssh-key", "spec": {}}), &ask)
            .await;
        let forms = ask.0.lock().unwrap();
        assert_eq!(forms.len(), 1);
        assert!(forms[0]["properties"].get("name").is_some());
        assert!(forms[0]["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "publicKey"));
    }

    #[tokio::test]
    async fn a_declined_approval_changes_nothing() {
        let toolbox = toolbox();
        let out = toolbox
            .call(
                "kcore_delete",
                &json!({"kind": "vm", "name": "web-01"}),
                &Declines,
            )
            .await;
        assert!(out.text.contains("\"deleted\": false"), "{}", out.text);
        let out = toolbox
            .call(
                "kcore_operation",
                &json!({"action": "cordon-node", "spec": {"nodeId": "n1"}}),
                &Declines,
            )
            .await;
        assert!(out.text.contains("\"applied\": false"), "{}", out.text);
    }

    #[tokio::test]
    async fn install_needs_the_wipe_acknowledgement() {
        let toolbox = toolbox();
        let spec = json!({"node": "10.0.0.5:9091", "osDisk": "/dev/sda", "joinController": "10.0.0.1:9090"});
        let out = toolbox
            .call(
                "kcore_operation",
                &json!({"action": "install-node", "spec": spec, "confirm": true}),
                &Declines,
            )
            .await;
        assert!(out.text.contains("wipe the selected disks"), "{}", out.text);
    }

    #[tokio::test]
    async fn rpc_lists_methods_and_holds_back_mutations() {
        let toolbox = toolbox();
        let list = toolbox
            .call("kcore_rpc", &json!({"service": "controller"}), &Declines)
            .await;
        assert!(list.text.contains("listVms"));
        let held = toolbox
            .call(
                "kcore_rpc",
                &json!({"service": "controller", "method": "deleteVm", "request": {"vmId": "a"}}),
                &Declines,
            )
            .await;
        assert!(held.text.contains("\"applied\": false"), "{}", held.text);
        let stub = toolbox
            .call("kcore_rpc", &json!({"service": "node-compute", "method": "createVm", "connection": {"node": "x:1"}, "confirm": true}), &Declines)
            .await;
        assert!(
            stub.text.contains("not an implemented unary RPC"),
            "{}",
            stub.text
        );
    }

    #[test]
    fn node_addresses_move_to_the_agent_port() {
        assert_eq!(node_agent_address("10.0.0.1:9090"), "10.0.0.1:9091");
        assert_eq!(node_agent_address("10.0.0.1"), "10.0.0.1:9091");
        assert_eq!(node_agent_address("10.0.0.1:9100"), "10.0.0.1:9100");
        assert_eq!(node_agent_address("[::1]:9090"), "[::1]:9091");
    }
}
