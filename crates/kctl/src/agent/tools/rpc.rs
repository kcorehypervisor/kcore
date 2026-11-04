//! Generic unary gRPC over the controller, node-agent, and provisioning APIs.
//!
//! Requests and responses are JSON. The message types come from the descriptor set built from
//! `proto/` at compile time, so a new RPC is reachable without code here.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{anyhow, Result};
use prost::Message;
use prost_reflect::{
    DescriptorPool, DeserializeOptions, DynamicMessage, MessageDescriptor, MethodDescriptor,
    SerializeOptions,
};
use serde_json::Value;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::{Request, Status};

use super::names::{input_error, InputError};
use crate::client;
use crate::config::ConnectionInfo;

static POOL: LazyLock<DescriptorPool> = LazyLock::new(|| {
    DescriptorPool::decode(
        include_bytes!(concat!(env!("OUT_DIR"), "/reflect/kcore.bin")).as_slice(),
    )
    .expect("embedded descriptor set is valid")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceName {
    Controller,
    ControllerAdmin,
    NodeCompute,
    NodeContainer,
    NodeStorage,
    NodeInfo,
    NodeAdmin,
    Provisioning,
}

pub const SERVICE_NAMES: [&str; 8] = [
    "controller",
    "controller-admin",
    "node-compute",
    "node-container",
    "node-storage",
    "node-info",
    "node-admin",
    "provisioning",
];

impl ServiceName {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "controller" => Self::Controller,
            "controller-admin" => Self::ControllerAdmin,
            "node-compute" => Self::NodeCompute,
            "node-container" => Self::NodeContainer,
            "node-storage" => Self::NodeStorage,
            "node-info" => Self::NodeInfo,
            "node-admin" => Self::NodeAdmin,
            "provisioning" => Self::Provisioning,
            _ => return None,
        })
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Controller => "controller",
            Self::ControllerAdmin => "controller-admin",
            Self::NodeCompute => "node-compute",
            Self::NodeContainer => "node-container",
            Self::NodeStorage => "node-storage",
            Self::NodeInfo => "node-info",
            Self::NodeAdmin => "node-admin",
            Self::Provisioning => "provisioning",
        }
    }

    fn full_name(self) -> &'static str {
        match self {
            Self::Controller => "kcore.controller.Controller",
            Self::ControllerAdmin => "kcore.controller.ControllerAdmin",
            Self::NodeCompute => "kcore.node.NodeCompute",
            Self::NodeContainer => "kcore.node.NodeContainer",
            Self::NodeStorage => "kcore.node.NodeStorage",
            Self::NodeInfo => "kcore.node.NodeInfo",
            Self::NodeAdmin => "kcore.node.NodeAdmin",
            Self::Provisioning => "kcore.provisioning.Provisioning",
        }
    }

    pub fn is_node(self) -> bool {
        matches!(
            self,
            Self::NodeCompute
                | Self::NodeContainer
                | Self::NodeStorage
                | Self::NodeInfo
                | Self::NodeAdmin
        )
    }
}

/// Node methods whose handlers always return UNIMPLEMENTED.
const UNIMPLEMENTED: [&str; 14] = [
    "node-compute/createVm",
    "node-compute/updateVm",
    "node-compute/deleteVm",
    "node-compute/rebootVm",
    "node-compute/pullImage",
    "node-compute/createWorkload",
    "node-compute/deleteWorkload",
    "provisioning/issueServerCertificate",
    "provisioning/issueNodeCertificate",
    "provisioning/getCertificate",
    "provisioning/listCertificates",
    "provisioning/revokeCertificate",
    "provisioning/renewCertificate",
    "provisioning/getCaBundle",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcMethod {
    pub service: ServiceName,
    pub method: String,
    pub read_only: bool,
}

pub fn is_read_only_method(method: &str) -> bool {
    ["get", "list", "classify", "check", "plan"]
        .iter()
        .any(|prefix| method.starts_with(prefix))
}

fn lower_first(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn service_methods(service: ServiceName) -> Vec<MethodDescriptor> {
    POOL.get_service_by_name(service.full_name())
        .map(|descriptor| descriptor.methods().collect())
        .unwrap_or_default()
}

pub fn rpc_catalog(service: Option<ServiceName>) -> Vec<RpcMethod> {
    let services: Vec<ServiceName> = match service {
        Some(service) => vec![service],
        None => SERVICE_NAMES
            .iter()
            .filter_map(|n| ServiceName::parse(n))
            .collect(),
    };
    let mut methods = Vec::new();
    for service in services {
        for descriptor in service_methods(service) {
            if descriptor.is_client_streaming() || descriptor.is_server_streaming() {
                continue;
            }
            let method = lower_first(descriptor.name());
            if UNIMPLEMENTED.contains(&format!("{}/{method}", service.id()).as_str()) {
                continue;
            }
            methods.push(RpcMethod {
                service,
                read_only: is_read_only_method(&method),
                method,
            });
        }
    }
    methods.sort_by(|a, b| (a.service.id(), &a.method).cmp(&(b.service.id(), &b.method)));
    methods
}

pub fn require_rpc(service_name: &str, method: &str) -> Result<RpcMethod, InputError> {
    let Some(service) = ServiceName::parse(service_name) else {
        return input_error(format!(
            "unknown gRPC service '{service_name}'. Use one of: {}",
            SERVICE_NAMES.join(", ")
        ));
    };
    rpc_catalog(Some(service))
        .into_iter()
        .find(|item| item.method == method)
        .ok_or_else(|| {
            InputError(format!(
                "{service_name}.{method} is not an implemented unary RPC. Streaming methods and node stubs that always return UNIMPLEMENTED are omitted."
            ))
        })
}

fn find_descriptor(service: ServiceName, method: &str) -> Result<MethodDescriptor, InputError> {
    service_methods(service)
        .into_iter()
        .find(|descriptor| lower_first(descriptor.name()) == method)
        .ok_or_else(|| InputError(format!("The gRPC API has no {method} method.")))
}

struct DynamicCodec {
    output: MessageDescriptor,
}

struct DynamicEncoder;

struct DynamicDecoder {
    output: MessageDescriptor,
}

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder {
            output: self.output.clone(),
        }
    }
}

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(dst)
            .map_err(|error| Status::internal(format!("encoding request: {error}")))
    }
}

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        DynamicMessage::decode(self.output.clone(), src)
            .map(Some)
            .map_err(|error| Status::internal(format!("decoding response: {error}")))
    }
}

/// Where a call goes. Controller and provisioning use the context's connection; node
/// services dial `node` with the same client certificate.
#[derive(Clone)]
pub struct Endpoints {
    pub controller: ConnectionInfo,
    pub provisioning: Option<String>,
    pub provisioning_tls: bool,
    pub timeout: Duration,
}

impl Endpoints {
    /// The cluster CA the controller connection trusts, as PEM.
    pub fn ca_pem(&self) -> Option<String> {
        if let Some(pem) = &self.controller.ca_pem {
            return Some(pem.clone());
        }
        std::fs::read_to_string(self.controller.ca.as_deref()?).ok()
    }

    fn info_for(
        &self,
        service: ServiceName,
        node: Option<&str>,
    ) -> Result<ConnectionInfo, InputError> {
        match service {
            ServiceName::Provisioning => {
                let Some(address) = self.provisioning.clone() else {
                    return input_error("No provisioning controller address. Set connection.provisioning or KCORE_PROVISIONING to host:port.");
                };
                let mut info = if self.provisioning_tls {
                    self.controller.clone()
                } else {
                    ConnectionInfo {
                        insecure: true,
                        ..self.controller.clone()
                    }
                };
                info.address = address.clone();
                info.addresses = vec![address];
                Ok(info)
            }
            service if service.is_node() => {
                let Some(address) = node else {
                    return input_error(
                        "A node service needs connection.node (host:port of the node-agent).",
                    );
                };
                let mut info = self.controller.clone();
                info.address = address.to_string();
                info.addresses = vec![address.to_string()];
                Ok(info)
            }
            _ => Ok(self.controller.clone()),
        }
    }

    pub async fn call(
        &self,
        service: ServiceName,
        node: Option<&str>,
        method: &str,
        request: &Value,
    ) -> Result<Value> {
        let descriptor = find_descriptor(service, method)?;
        let info = self.info_for(service, node)?;
        let message = DynamicMessage::deserialize_with_options(
            descriptor.input(),
            request.clone(),
            &DeserializeOptions::new().deny_unknown_fields(true),
        )
        .map_err(|error| InputError(format!("invalid {method} request: {error}")))?;

        let path =
            PathAndQuery::try_from(format!("/{}/{}", service.full_name(), descriptor.name()))?;
        let channel = client::connect(&info).await?;
        let mut grpc = tonic::client::Grpc::new(channel)
            .max_decoding_message_size(64 * 1024 * 1024)
            .max_encoding_message_size(64 * 1024 * 1024);
        grpc.ready().await.map_err(|error| anyhow!("{error}"))?;
        let mut request = Request::new(message);
        request.set_timeout(self.timeout);
        let response = grpc
            .unary(
                request,
                path,
                DynamicCodec {
                    output: descriptor.output(),
                },
            )
            .await
            .map_err(|status| anyhow!("{}", status.message()))?;
        let options = SerializeOptions::new()
            .stringify_64_bit_integers(true)
            .skip_default_fields(false);
        Ok(response
            .into_inner()
            .serialize_with_options(serde_json::value::Serializer, &options)?)
    }
}

const SENSITIVE_KEY: &str = "key.?pem|private.?key|password|secret";

pub fn redact(value: &Value) -> Value {
    static PATTERN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(&format!("(?i){SENSITIVE_KEY}")).unwrap());
    match value {
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| {
                    let hide = matches!(item, Value::String(text) if !text.is_empty())
                        && PATTERN.is_match(key);
                    (
                        key.clone(),
                        if hide {
                            Value::String("[redacted]".to_string())
                        } else {
                            redact(item)
                        },
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_catalog_lists_implemented_unary_methods() {
        let all = rpc_catalog(None);
        assert!(all
            .iter()
            .any(|m| m.service == ServiceName::Controller && m.method == "listVms" && m.read_only));
        assert!(all.iter().any(|m| m.method == "createVm" && !m.read_only));
        assert!(!all
            .iter()
            .any(|m| m.service == ServiceName::NodeCompute && m.method == "createVm"));
        assert!(!all
            .iter()
            .any(|m| m.service == ServiceName::Provisioning && m.method == "getCaBundle"));
        assert!(all.iter().any(|m| m.service == ServiceName::Provisioning));
    }

    #[test]
    fn unknown_services_and_methods_are_refused() {
        assert!(require_rpc("nope", "listVms").is_err());
        assert!(require_rpc("controller", "nope").is_err());
        assert!(require_rpc("controller", "listVms").is_ok());
    }

    #[test]
    fn read_only_methods_are_recognised_by_prefix() {
        for name in [
            "getVm",
            "listVms",
            "classifyDiskLayout",
            "checkX",
            "planClusterUpdate",
        ] {
            assert!(is_read_only_method(name), "{name}");
        }
        assert!(!is_read_only_method("deleteVm"));
    }

    #[test]
    fn secrets_are_redacted() {
        let out = redact(
            &json!({"name": "a", "privateKeyPem": "abc", "nested": [{"password": "x"}], "empty": ""}),
        );
        assert_eq!(out["privateKeyPem"], "[redacted]");
        assert_eq!(out["nested"][0]["password"], "[redacted]");
        assert_eq!(out["name"], "a");
    }

    #[test]
    fn builders_produce_requests_the_descriptors_accept() {
        use super::super::catalog::find_kind;
        use super::super::requests::{build_apply, Target};
        let spec = json!({
            "name": "web-01", "network": "default", "storageBackend": "ceph", "storageSizeBytes": "40G",
            "imageUrl": "https://example.com/d.qcow2", "imageSha256": "a".repeat(64),
            "sshPublicKeys": ["ssh-ed25519 AAAA operator@host"], "cpu": 2, "desiredState": "running",
        });
        let calls = build_apply(find_kind(Some("vm")).unwrap(), spec.as_object().unwrap()).unwrap();
        for call in calls {
            assert_eq!(call.target, Target::Controller);
            let descriptor = find_descriptor(ServiceName::Controller, call.method).unwrap();
            DynamicMessage::deserialize_with_options(
                descriptor.input(),
                call.request,
                &DeserializeOptions::new().deny_unknown_fields(true),
            )
            .unwrap();
        }
    }

    fn assert_accepted(call: &super::super::requests::RpcCall) {
        use super::super::requests::Target;
        let service = if call.target == Target::Controller {
            ServiceName::Controller
        } else {
            ServiceName::NodeAdmin
        };
        let descriptor = find_descriptor(service, call.method)
            .unwrap_or_else(|e| panic!("{}: {e}", call.method));
        DynamicMessage::deserialize_with_options(
            descriptor.input(),
            call.request.clone(),
            &DeserializeOptions::new().deny_unknown_fields(true),
        )
        .unwrap_or_else(|e| panic!("{} rejects {}: {e}", call.method, call.request));
    }

    #[test]
    fn every_resource_kind_builds_requests_the_descriptors_accept() {
        use super::super::catalog::{find_kind, KINDS};
        use super::super::requests::{build_apply, build_delete, build_plan_call, build_read};
        let key = "ssh-ed25519 AAAA operator@host";
        let specs = json!({
            "vm": { "name": "web-01", "network": "default", "storageBackend": "ceph", "storageSizeBytes": "40G", "imageUrl": "https://example.com/d.qcow2", "imageSha256": "a".repeat(64), "sshPublicKeys": [key], "cpu": 2 },
            "network": { "name": "lan", "type": "nat", "externalIp": "10.0.0.2", "gatewayIp": "10.0.0.1" },
            "ssh-key": { "name": "op", "publicKey": key },
            "container": { "name": "c1", "image": "nginx:latest", "ports": ["8080:80"], "env": { "A": "b" } },
            "postgresql": { "name": "db1" },
            "volume": { "name": "v1", "sizeBytes": "10G" },
            "volume-snapshot": { "volume": "v1", "name": "s1" },
            "snapshot-policy": { "name": "p1", "volume": "v1", "keep": 3 },
            "security-group": { "name": "sg", "rules": ["tcp/22 from 10.0.0.0/8"] },
            "disk-layout": { "name": "dl", "nodeId": "node-1", "layoutNix": "{ disko.devices = {}; }" },
            "ceph-cluster": { "name": "san", "publicNetwork": "192.168.47.0/24", "clusterNetwork": "192.168.47.0/24" },
            "shared-filesystem": { "name": "fs", "cephCluster": "san" },
            "object-store": { "name": "os", "cephCluster": "san", "members": ["n1"] },
            "cluster-update": { "name": "u1", "version": "1.2.3", "flakeRef": "github:example/kcore" },
        });
        for kind in KINDS
            .iter()
            .filter(|k| k.id != "cluster" && k.id != "image")
        {
            let spec = specs[kind.id]
                .as_object()
                .unwrap_or_else(|| panic!("no spec for {}", kind.id));
            for call in build_apply(find_kind(Some(kind.id)).unwrap(), spec)
                .unwrap_or_else(|e| panic!("apply {}: {e}", kind.id))
            {
                assert_accepted(&call);
            }
            assert_accepted(
                &build_plan_call(kind, spec, None)
                    .unwrap_or_else(|e| panic!("plan {}: {e}", kind.id)),
            );
            assert_accepted(&build_read(kind, None).unwrap());
            assert_accepted(&build_read(kind, Some("thing")).unwrap());
            assert_accepted(
                &build_delete(kind, "thing").unwrap_or_else(|e| panic!("delete {}: {e}", kind.id)),
            );
        }
    }

    #[test]
    fn every_operation_builds_requests_the_descriptors_accept() {
        use super::super::requests::build_operation;
        let cases = [
            ("set-vm-state", json!({ "name": "web", "state": "running" })),
            (
                "migrate-vm",
                json!({ "name": "web", "targetNode": "node-2" }),
            ),
            ("drain-node", json!({ "nodeId": "node-1" })),
            ("cordon-node", json!({ "nodeId": "node-1" })),
            ("uncordon-node", json!({ "nodeId": "node-1" })),
            ("approve-node", json!({ "nodeId": "node-1" })),
            ("reject-node", json!({ "nodeId": "node-1" })),
            ("approve-update", json!({ "name": "u1" })),
            ("cancel-update", json!({ "name": "u1" })),
            ("rollback-update", json!({ "name": "u1" })),
        ];
        for (action, spec) in cases {
            let call = build_operation(action, spec.as_object().unwrap())
                .unwrap_or_else(|e| panic!("{action}: {e}"));
            assert_accepted(&call);
        }
    }
}
