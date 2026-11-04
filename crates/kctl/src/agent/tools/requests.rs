//! Turn a validated spec into the controller or node-agent RPC that carries it out.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use super::catalog::ResourceKind;
use super::names::{
    assert_address, assert_arg, assert_disk, assert_https_url, assert_name, assert_sha256,
    assert_ssh_public_key, boolish, input_error, opt, required, string_list, InputError, Spec,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Controller,
    Node,
}

#[derive(Debug, Clone)]
pub struct RpcCall {
    pub target: Target,
    pub method: &'static str,
    pub request: Value,
    pub address: Option<String>,
}

type Built<T> = Result<T, InputError>;

fn controller(method: &'static str, request: Value) -> RpcCall {
    RpcCall {
        target: Target::Controller,
        method,
        request,
        address: None,
    }
}

fn backend_enum(value: &str) -> Built<&'static str> {
    match value.trim().to_lowercase().as_str() {
        "filesystem" => Ok("STORAGE_BACKEND_TYPE_FILESYSTEM"),
        "lvm" => Ok("STORAGE_BACKEND_TYPE_LVM"),
        "zfs" => Ok("STORAGE_BACKEND_TYPE_ZFS"),
        "ceph" => Ok("STORAGE_BACKEND_TYPE_CEPH"),
        _ => input_error("storage backend must be filesystem, lvm, zfs, or ceph"),
    }
}

fn desired(value: Option<&Value>) -> Built<&'static str> {
    match value {
        Some(Value::String(text)) if text == "running" => Ok("VM_DESIRED_STATE_RUNNING"),
        Some(Value::String(text)) if text == "stopped" => Ok("VM_DESIRED_STATE_STOPPED"),
        None | Some(Value::Null) => Ok("VM_DESIRED_STATE_UNSPECIFIED"),
        Some(Value::String(text)) if text.is_empty() => Ok("VM_DESIRED_STATE_UNSPECIFIED"),
        _ => input_error("desired state must be running or stopped"),
    }
}

static SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^([1-9]\d*)\s*([KMGT])i?B?$").unwrap());
static RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(tcp|udp)/(\d+)(?:\s+from\s+(\S+))?").unwrap());
static DATABASE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,62}$").unwrap());

/// Byte count as decimal text, since 64-bit fields travel as strings in JSON.
pub fn size_bytes(value: &str, label: &str) -> Built<String> {
    let text = value.trim();
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(text.to_string());
    }
    let Some(captures) = SIZE.captures(text) else {
        return input_error(format!(
            "{label} must be a byte count or a size such as 40G"
        ));
    };
    let count: u128 = captures[1]
        .parse()
        .map_err(|_| InputError(format!("{label} is too large")))?;
    let shift = match captures[2].to_uppercase().as_str() {
        "K" => 10,
        "M" => 20,
        "G" => 30,
        _ => 40,
    };
    let bytes = count
        .checked_mul(1u128 << shift)
        .filter(|bytes| *bytes <= u64::MAX as u128)
        .ok_or_else(|| InputError(format!("{label} is too large")))?;
    Ok(bytes.to_string())
}

fn int(text: &str, label: &str) -> Built<i64> {
    text.trim()
        .parse::<i64>()
        .map_err(|_| InputError(format!("{label} must be a whole number")))
}

fn opt_int(spec: &Spec, key: &str, label: &str, default: i64) -> Built<i64> {
    match opt(spec, key) {
        Some(text) => int(&text, label),
        None => Ok(default),
    }
}

fn opt_name(spec: &Spec, key: &str, label: &str) -> Built<String> {
    match opt(spec, key) {
        Some(value) => assert_name(&value, label),
        None => Ok(String::new()),
    }
}

fn named(spec: &Spec, key: &str, required_label: &str) -> Built<String> {
    assert_name(&required(spec, key, required_label)?, required_label)
}

pub fn build_apply(kind: &ResourceKind, spec: &Spec) -> Built<Vec<RpcCall>> {
    let one = |call: RpcCall| Ok(vec![call]);
    match kind.id {
        "cluster" => input_error(
            "cluster creation writes local certificates and is handled before any controller RPC",
        ),
        "vm" => one(controller("createVm", vm_request(spec)?)),
        "network" => one(controller("createNetwork", network_request(spec)?)),
        "ssh-key" => one(controller("createSshKey", ssh_key_request(spec)?)),
        "container" => one(controller("createWorkload", container_request(spec)?)),
        "postgresql" => one(controller("createPostgresql", postgresql_request(spec)?)),
        "volume" => one(controller("createVolume", volume_request(spec)?)),
        "volume-snapshot" => one(controller("createVolumeSnapshot", snapshot_request(spec)?)),
        "snapshot-policy" => one(controller(
            "createSnapshotPolicy",
            json!({ "policy": policy_message(spec)? }),
        )),
        "security-group" => security_group_calls(spec),
        "disk-layout" => one(controller(
            "createDiskLayout",
            json!({ "diskLayout": disk_layout_message(spec)? }),
        )),
        "ceph-cluster" => one(controller(
            "createCephCluster",
            json!({ "cephCluster": ceph_message(spec)? }),
        )),
        "shared-filesystem" => one(controller(
            "createSharedFilesystem",
            json!({ "sharedFilesystem": filesystem_message(spec)? }),
        )),
        "object-store" => one(controller(
            "createObjectStore",
            json!({ "objectStore": object_store_message(spec)? }),
        )),
        "cluster-update" => one(controller(
            "createClusterUpdate",
            json!({ "spec": cluster_update_message(spec)? }),
        )),
        other => input_error(format!("unsupported kind {other}")),
    }
}

pub fn build_read(kind: &ResourceKind, name: Option<&str>) -> Built<RpcCall> {
    let named = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(value) => assert_name(value, "name")?,
        None => String::new(),
    };
    let has = !named.is_empty();
    let pick = |get: &'static str, list: &'static str, request: Value| {
        if has {
            controller(get, request)
        } else {
            controller(list, json!({}))
        }
    };
    let by_name = json!({ "name": named });
    Ok(match kind.id {
        "cluster" => controller("getClusterHealth", json!({})),
        "vm" => pick("getVm", "listVms", json!({ "vmId": named })),
        "network" => controller("listNetworks", json!({})),
        "ssh-key" => pick("getSshKey", "listSshKeys", by_name),
        "container" => {
            if has {
                controller(
                    "getWorkload",
                    json!({ "kind": "WORKLOAD_KIND_CONTAINER", "workloadId": named }),
                )
            } else {
                controller(
                    "listWorkloads",
                    json!({ "kind": "WORKLOAD_KIND_CONTAINER" }),
                )
            }
        }
        "postgresql" => pick("getPostgresql", "listPostgresqls", by_name),
        "volume" => pick("getVolume", "listVolumes", by_name),
        "volume-snapshot" => controller(
            "listVolumeSnapshots",
            if has {
                json!({ "volume": named })
            } else {
                json!({})
            },
        ),
        "snapshot-policy" => pick("getSnapshotPolicy", "listSnapshotPolicies", by_name),
        "security-group" => pick("getSecurityGroup", "listSecurityGroups", by_name),
        "disk-layout" => pick("getDiskLayout", "listDiskLayouts", by_name),
        "ceph-cluster" => pick("getCephCluster", "listCephClusters", by_name),
        "shared-filesystem" => pick("getSharedFilesystem", "listSharedFilesystems", by_name),
        "object-store" => pick("getObjectStore", "listObjectStores", by_name),
        "cluster-update" => pick("getClusterUpdate", "listClusterUpdates", by_name),
        "image" => return input_error("image reads are collected from each node"),
        other => return input_error(format!("unsupported kind {other}")),
    })
}

pub fn build_delete(kind: &ResourceKind, name: &str) -> Built<RpcCall> {
    let id = assert_name(name, "name")?;
    let by_name = json!({ "name": id });
    Ok(match kind.id {
        "cluster" => {
            return input_error("This server does not delete local certificates. Remove the context from ~/.kcore/config by hand.")
        }
        "vm" => controller("deleteVm", json!({ "vmId": id })),
        "network" => controller("deleteNetwork", by_name),
        "ssh-key" => controller("deleteSshKey", by_name),
        "container" => controller(
            "deleteWorkload",
            json!({ "kind": "WORKLOAD_KIND_CONTAINER", "workloadId": id }),
        ),
        "postgresql" => controller("deletePostgresql", by_name),
        "volume" => controller("deleteVolume", by_name),
        "volume-snapshot" => controller("deleteVolumeSnapshot", by_name),
        "snapshot-policy" => controller("deleteSnapshotPolicy", by_name),
        "security-group" => controller("deleteSecurityGroup", by_name),
        "disk-layout" => controller("deleteDiskLayout", by_name),
        "ceph-cluster" => controller("deleteCephCluster", by_name),
        "shared-filesystem" => controller("deleteSharedFilesystem", by_name),
        "object-store" => controller("deleteObjectStore", by_name),
        "cluster-update" => controller("cancelClusterUpdate", by_name),
        other => return input_error(format!("unsupported kind {other}")),
    })
}

pub fn build_plan_call(kind: &ResourceKind, spec: &Spec, name: Option<&str>) -> Built<RpcCall> {
    if kind.id == "disk-layout" {
        return Ok(controller(
            "classifyDiskLayout",
            json!({ "diskLayout": disk_layout_message(spec)? }),
        ));
    }
    if kind.id == "cluster-update" {
        return Ok(controller(
            "planClusterUpdate",
            json!({ "spec": cluster_update_message(spec)? }),
        ));
    }
    let fallback = spec.get("name").and_then(Value::as_str);
    build_read(kind, name.or(fallback))
}

pub const OPERATION_ACTIONS: &str = "set-vm-state, migrate-vm, drain-node, cordon-node, uncordon-node, approve-node, reject-node, approve-update, cancel-update, rollback-update, or install-node";

pub fn build_operation(action: &str, spec: &Spec) -> Built<RpcCall> {
    let node_id = || named(spec, "nodeId", "node id");
    let update_name = || named(spec, "name", "update name");
    match action {
        "set-vm-state" => {
            let state = required(spec, "state", "state")?;
            if state != "running" && state != "stopped" {
                return input_error("state must be running or stopped");
            }
            Ok(controller(
                "setVmDesiredState",
                json!({
                    "vmId": named(spec, "name", "VM name")?,
                    "desiredState": if state == "running" { "VM_DESIRED_STATE_RUNNING" } else { "VM_DESIRED_STATE_STOPPED" },
                    "targetNode": opt(spec, "targetNode").unwrap_or_default(),
                }),
            ))
        }
        "migrate-vm" => Ok(controller(
            "migrateVm",
            json!({
                "vmId": named(spec, "name", "VM name")?,
                "targetNode": assert_arg(&required(spec, "targetNode", "target node")?, "target node")?,
                "allowColdFallback": boolish(spec.get("allowColdFallback")),
            }),
        )),
        "drain-node" => Ok(controller(
            "drainNode",
            json!({ "nodeId": node_id()?, "targetNode": opt(spec, "targetNode").unwrap_or_default() }),
        )),
        "cordon-node" => Ok(controller("cordonNode", json!({ "nodeId": node_id()? }))),
        "uncordon-node" => Ok(controller("uncordonNode", json!({ "nodeId": node_id()? }))),
        "approve-node" => Ok(controller("approveNode", json!({ "nodeId": node_id()? }))),
        "reject-node" => Ok(controller("rejectNode", json!({ "nodeId": node_id()? }))),
        "approve-update" => Ok(controller(
            "approveClusterUpdate",
            json!({ "name": update_name()? }),
        )),
        "cancel-update" => Ok(controller(
            "cancelClusterUpdate",
            json!({ "name": update_name()? }),
        )),
        "rollback-update" => Ok(controller(
            "rollbackClusterUpdate",
            json!({ "name": update_name()? }),
        )),
        "install-node" => install_call(spec),
        _ => input_error(format!("action must be {OPERATION_ACTIONS}")),
    }
}

pub struct BootstrapRequest {
    pub node_id: String,
    pub node_host: String,
    pub address: String,
}

pub fn bootstrap_cert_request(spec: &Spec) -> Built<BootstrapRequest> {
    let address = assert_address(&required(spec, "node", "node address")?, "node address")?;
    let node_host = host_of(&address);
    let node_id = match opt(spec, "nodeId") {
        Some(id) => assert_name(&id, "node id")?,
        None => format!("kcore-node-{}", node_host.replace('.', "-")),
    };
    Ok(BootstrapRequest {
        node_id,
        node_host,
        address,
    })
}

fn install_call(spec: &Spec) -> Built<RpcCall> {
    let boot = bootstrap_cert_request(spec)?;
    let join = assert_address(
        &required(spec, "joinController", "join controller")?,
        "join controller",
    )?;
    let backend = match opt(spec, "storageBackend") {
        Some(value) => backend_enum(&value)?,
        None => "STORAGE_BACKEND_TYPE_UNSPECIFIED",
    };
    Ok(RpcCall {
        target: Target::Node,
        method: "installToDisk",
        address: Some(boot.address.clone()),
        request: json!({
            "osDisk": assert_disk(&required(spec, "osDisk", "OS disk")?, "OS disk")?,
            "dataDisks": data_disks(spec)?,
            "controller": join,
            "controllers": [join],
            "storageBackend": backend,
            "disableVxlan": boolish(spec.get("disableVxlan")),
            "dcId": opt_name(spec, "dcId", "datacenter")?,
            "nodeId": boot.node_id,
            "hostname": opt(spec, "hostname").unwrap_or_default(),
        }),
    })
}

fn vm_request(spec: &Spec) -> Built<Value> {
    let name = named(spec, "name", "VM name")?;
    let network = named(spec, "network", "network")?;
    let backend = backend_enum(&required(spec, "storageBackend", "storage backend")?)?;
    let mut nics = vec![json!({ "network": network, "model": "virtio", "macAddress": "" })];
    if let Some(Value::Array(extra)) = spec.get("extraNetworks") {
        for item in extra {
            if let Some(text) = item.as_str().filter(|t| !t.trim().is_empty()) {
                nics.push(json!({ "network": assert_name(text, "extra network")?, "model": "virtio", "macAddress": "" }));
            }
        }
    }
    let image = image_fields(spec)?;
    let memory = opt(spec, "memoryBytes").or_else(|| opt(spec, "memory"));
    let ssh_key_names = string_list(spec.get("sshKeys"))
        .iter()
        .map(|item| assert_name(item, "ssh key name"))
        .collect::<Built<Vec<_>>>()?;
    let cpu = match opt(spec, "cpu") {
        Some(value) => int(&assert_arg(&value, "cpu")?, "cpu")?,
        None => 2,
    };
    Ok(json!({
        "targetNode": opt(spec, "targetNode").unwrap_or_default(),
        "targetDc": opt(spec, "dc").unwrap_or_default(),
        "storageBackend": backend,
        "storageSizeBytes": size_bytes(&required(spec, "storageSizeBytes", "storage size")?, "storage size")?,
        "imageUrl": image.0,
        "imageSha256": image.1,
        "imagePath": image.2,
        "imageFormat": image.3,
        "cloudInitUserData": cloud_init(&name, spec)?,
        "sshKeyNames": ssh_key_names,
        "nodeLabels": string_list(spec.get("nodeLabels")),
        "antiAffinity": opt_name(spec, "antiAffinity", "anti-affinity")?,
        "spec": {
            "name": name,
            "cpu": cpu,
            "memoryBytes": size_bytes(memory.as_deref().unwrap_or("2G"), "memory")?,
            "nics": nics,
            "disks": [],
            "desiredState": desired(spec.get("desiredState"))?,
        },
    }))
}

type ImageFields = (String, String, String, String);

fn image_fields(spec: &Spec) -> Built<ImageFields> {
    if let Some(url) = opt(spec, "imageUrl") {
        return Ok((
            assert_https_url(&url)?,
            assert_sha256(&required(spec, "imageSha256", "image SHA256")?)?,
            String::new(),
            opt(spec, "imageFormat").unwrap_or_default(),
        ));
    }
    if let Some(path) = opt(spec, "imagePath") {
        let format = required(spec, "imageFormat", "image format")?;
        if format != "qcow2" && format != "raw" {
            return input_error("image format must be qcow2 or raw");
        }
        return Ok((
            String::new(),
            String::new(),
            assert_arg(&path, "image path")?,
            format,
        ));
    }
    input_error("an https image URL or a node-local image path is required")
}

fn cloud_init(hostname: &str, spec: &Spec) -> Built<String> {
    if opt(spec, "password").is_some() {
        return input_error("Use an SSH public key. Password login stays off unless cloudInitUserData is set and compliant is false.");
    }
    if let Some(user_data) = opt(spec, "cloudInitUserData") {
        return Ok(user_data);
    }
    let keys = string_list(spec.get("sshPublicKeys"))
        .iter()
        .map(|item| assert_ssh_public_key(item))
        .collect::<Built<Vec<_>>>()?;
    if keys.is_empty() {
        return Ok(String::new());
    }
    let username = assert_name(
        &opt(spec, "username").unwrap_or_else(|| "kcore".to_string()),
        "username",
    )?;
    let mut lines = vec![
        "#cloud-config".to_string(),
        format!("hostname: {hostname}"),
        "users:".to_string(),
        "  - default".to_string(),
        format!("  - name: {username}"),
        "    gecos: kcore VM user".to_string(),
        "    groups: [sudo]".to_string(),
        "    shell: /bin/bash".to_string(),
        "    lock_passwd: true".to_string(),
        "    ssh_authorized_keys:".to_string(),
    ];
    for key in keys {
        lines.push(format!(
            "      - \"{}\"",
            key.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    lines.push("ssh_pwauth: false".to_string());
    Ok(lines.join("\n") + "\n")
}

fn network_request(spec: &Spec) -> Built<Value> {
    let kind = required(spec, "type", "network type")?;
    if !["nat", "bridge", "vxlan"].contains(&kind.as_str()) {
        return input_error("network type must be nat, bridge, or vxlan");
    }
    Ok(json!({
        "name": named(spec, "name", "network name")?,
        "externalIp": assert_arg(&required(spec, "externalIp", "external IP")?, "external IP")?,
        "gatewayIp": assert_arg(&required(spec, "gatewayIp", "gateway IP")?, "gateway IP")?,
        "internalNetmask": opt(spec, "internalNetmask").unwrap_or_else(|| "255.255.255.0".to_string()),
        "targetNode": opt(spec, "targetNode").unwrap_or_default(),
        "networkType": kind,
        "vlanId": opt_int(spec, "vlanId", "vlan id", 0)?,
        "enableOutboundNat": if boolish(spec.get("noOutboundNat")) { false } else { kind != "bridge" },
        "ipv6Prefix": opt(spec, "ipv6Prefix").unwrap_or_default(),
        "ipv6Gateway": opt(spec, "ipv6Gateway").unwrap_or_default(),
        "eastWestFirewall": boolish(spec.get("eastWestFirewall")),
    }))
}

fn ssh_key_request(spec: &Spec) -> Built<Value> {
    Ok(json!({
        "name": named(spec, "name", "name")?,
        "publicKey": assert_ssh_public_key(&required(spec, "publicKey", "public key")?)?,
    }))
}

fn container_request(spec: &Spec) -> Built<Value> {
    let mut env = Map::new();
    if let Some(Value::Object(items)) = spec.get("env") {
        for (key, value) in items {
            let text = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            env.insert(key.clone(), Value::String(text));
        }
    }
    Ok(json!({
        "kind": "WORKLOAD_KIND_CONTAINER",
        "containerSpec": {
            "name": named(spec, "name", "container name")?,
            "image": assert_arg(&required(spec, "image", "image")?, "image")?,
            "network": opt_name(spec, "network", "network")?,
            "command": string_list(spec.get("command")),
            "env": env,
            "ports": string_list(spec.get("ports")),
            "desiredState": desired(spec.get("desiredState"))?,
        },
    }))
}

fn volume_request(spec: &Spec) -> Built<Value> {
    Ok(json!({
        "name": named(spec, "name", "volume name")?,
        "sizeBytes": size_bytes(&required(spec, "sizeBytes", "size")?, "size")?,
        "storageClass": "ceph",
        "vm": opt_name(spec, "vm", "vm")?,
        "fromSnapshot": opt_name(spec, "fromSnapshot", "snapshot")?,
        "encrypt": boolish(spec.get("encrypt")),
    }))
}

fn snapshot_request(spec: &Spec) -> Built<Value> {
    Ok(json!({
        "volume": named(spec, "volume", "volume")?,
        "name": named(spec, "name", "snapshot name")?,
        "consistency": "crash",
    }))
}

fn policy_message(spec: &Spec) -> Built<Value> {
    if opt(spec, "vm").is_none() && opt(spec, "volume").is_none() {
        return input_error("a snapshot policy needs a vm or a volume");
    }
    Ok(json!({
        "name": named(spec, "name", "policy name")?,
        "selectorVm": opt_name(spec, "vm", "vm")?,
        "selectorVolume": opt_name(spec, "volume", "volume")?,
        "schedule": assert_arg(&opt(spec, "schedule").unwrap_or_else(|| "@daily".to_string()), "schedule")?,
        "keep": opt_int(spec, "keep", "keep", 7)?,
        "enabled": !boolish(spec.get("disabled")),
    }))
}

fn security_group_calls(spec: &Spec) -> Built<Vec<RpcCall>> {
    let name = named(spec, "name", "name")?;
    let mut calls = vec![controller(
        "createSecurityGroup",
        json!({
            "securityGroup": {
                "name": name,
                "description": opt(spec, "description").unwrap_or_default(),
                "rules": rules(spec.get("rules"))?,
            },
        }),
    )];
    if let Some(Value::Array(attachments)) = spec.get("attachments") {
        for attachment in attachments {
            let Some(row) = attachment.as_object() else {
                continue;
            };
            let text = |keys: &[&str]| -> String {
                keys.iter()
                    .find_map(|key| row.get(*key))
                    .map(|value| match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default()
            };
            let kind = text(&["kind", "targetKind"]);
            calls.push(controller(
                "attachSecurityGroup",
                json!({
                    "securityGroup": name,
                    "targetKind": if kind == "network" { "SECURITY_GROUP_TARGET_KIND_NETWORK" } else { "SECURITY_GROUP_TARGET_KIND_VM" },
                    "targetId": assert_arg(&text(&["target", "targetId"]), "attachment target")?,
                    "targetNode": row.get("node").and_then(Value::as_str).unwrap_or(""),
                }),
            ));
        }
    }
    Ok(calls)
}

fn rules(value: Option<&Value>) -> Built<Vec<Value>> {
    let items: Vec<Value> = match value {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => vec![Value::String(text.clone())],
        _ => Vec::new(),
    };
    if items.is_empty() {
        return input_error("a security group needs at least one rule");
    }
    items
        .iter()
        .map(|item| match item {
            Value::String(text) => {
                let Some(captures) = RULE.captures(text.trim()) else {
                    return input_error(format!(
                        "rule must look like tcp/443 from 10.0.0.0/8, got {text}"
                    ));
                };
                let port: i64 = captures[2]
                    .parse()
                    .map_err(|_| InputError(format!("rule port is out of range in {text}")))?;
                Ok(json!({
                    "protocol": captures[1].to_lowercase(),
                    "hostPort": port,
                    "sourceCidr": captures.get(3).map(|m| m.as_str()).unwrap_or(""),
                }))
            }
            Value::Object(row) => {
                let text = |key: &str| match row.get(key) {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => other.to_string(),
                };
                let number = |key: &str| -> i64 {
                    match row.get(key) {
                        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
                        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
                        _ => 0,
                    }
                };
                Ok(json!({
                    "protocol": text("protocol"),
                    "hostPort": number("hostPort"),
                    "targetPort": number("targetPort"),
                    "sourceCidr": text("sourceCidr"),
                    "targetVm": text("targetVm"),
                    "enableDnat": boolish(row.get("enableDnat")),
                }))
            }
            _ => input_error("a security group rule must be an object or tcp/port text"),
        })
        .collect()
}

fn disk_layout_message(spec: &Spec) -> Built<Value> {
    let layout = match opt(spec, "layoutNix") {
        Some(text) => text,
        None => read_layout_file(opt(spec, "layoutNixFile").as_deref())?,
    };
    if layout.is_empty() && spec.get("diskLayout").is_none_or(|v| v.is_null()) {
        return input_error("a disk layout needs layoutNix, layoutNixFile, or diskLayout");
    }
    Ok(json!({
        "name": named(spec, "name", "name")?,
        "nodeId": named(spec, "nodeId", "node id")?,
        "layoutNix": layout,
    }))
}

fn read_layout_file(path: Option<&str>) -> Built<String> {
    let Some(path) = path else {
        return Ok(String::new());
    };
    let path = assert_arg(path, "layout file")?;
    std::fs::read_to_string(&path)
        .map_err(|error| InputError(format!("cannot read {path}: {error}")))
}

fn ceph_message(spec: &Spec) -> Built<Value> {
    Ok(json!({
        "name": named(spec, "name", "name")?,
        "spec": {
            "fsid": opt(spec, "fsid").unwrap_or_default(),
            "publicNetwork": assert_arg(&required(spec, "publicNetwork", "public network")?, "public network")?,
            "clusterNetwork": assert_arg(&required(spec, "clusterNetwork", "cluster network")?, "cluster network")?,
            "size": opt_int(spec, "size", "size", 0)?,
            "minSize": opt_int(spec, "minSize", "min size", 0)?,
            "forceWipe": boolish(spec.get("forceWipe")),
            "encryptOsds": if spec.contains_key("encryptOsds") { boolish(spec.get("encryptOsds")) } else { true },
            "nodes": match spec.get("nodes") { Some(Value::Array(nodes)) => nodes.clone(), _ => Vec::new() },
        },
    }))
}

fn filesystem_message(spec: &Spec) -> Built<Value> {
    Ok(json!({
        "name": named(spec, "name", "name")?,
        "spec": {
            "cephCluster": named(spec, "cephCluster", "ceph cluster")?,
            "quotaBytes": size_bytes(&opt(spec, "quotaBytes").unwrap_or_else(|| "0".to_string()), "quota")?,
            "clients": match spec.get("clients") { Some(Value::Array(clients)) => clients.clone(), _ => Vec::new() },
            "fsName": opt(spec, "fsName").unwrap_or_default(),
        },
    }))
}

fn object_store_message(spec: &Spec) -> Built<Value> {
    let members = string_list(spec.get("members"))
        .iter()
        .map(|item| assert_name(item, "member"))
        .collect::<Built<Vec<_>>>()?;
    if members.is_empty() {
        return input_error("an object store needs at least one member node");
    }
    Ok(json!({
        "name": named(spec, "name", "name")?,
        "spec": {
            "cephCluster": named(spec, "cephCluster", "ceph cluster")?,
            "members": members,
            "port": opt_int(spec, "port", "port", 0)?,
            "tls": boolish(spec.get("tls")),
        },
    }))
}

fn cluster_update_message(spec: &Spec) -> Built<Value> {
    let strategy = opt(spec, "strategy").unwrap_or_else(|| "one-at-a-time".to_string());
    Ok(json!({
        "name": named(spec, "name", "name")?,
        "target": {
            "version": assert_arg(&required(spec, "version", "version")?, "version")?,
            "flakeRef": assert_arg(&required(spec, "flakeRef", "flake ref")?, "flake ref")?,
            "flakeRev": opt(spec, "flakeRev").unwrap_or_default(),
        },
        "strategy": {
            "type": if strategy == "one-at-a-time" { "CLUSTER_UPDATE_STRATEGY_ONE_AT_A_TIME" } else { "CLUSTER_UPDATE_STRATEGY_UNSPECIFIED" },
            "maxUnavailable": 1,
        },
        "drainVms": if spec.contains_key("drainVms") { boolish(spec.get("drainVms")) } else { true },
    }))
}

fn data_disks(spec: &Spec) -> Built<Vec<String>> {
    let value = spec.get("dataDisks").or_else(|| spec.get("dataDisk"));
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|item| !item.trim().is_empty())
            .map(|item| assert_disk(item, "data disk"))
            .collect(),
        Some(Value::String(text)) if !text.trim().is_empty() => {
            Ok(vec![assert_disk(text, "data disk")?])
        }
        _ => Ok(Vec::new()),
    }
}

fn host_of(address: &str) -> String {
    if address.starts_with('[') {
        return match address.find(']') {
            Some(end) if end > 1 => address[1..end].to_string(),
            _ => address.to_string(),
        };
    }
    match address.rfind(':') {
        Some(index) => address[..index].to_string(),
        None => address.to_string(),
    }
}

const POSTGRESQL_PACKAGES: [&str; 5] = [
    "postgresql",
    "postgresql_14",
    "postgresql_15",
    "postgresql_16",
    "postgresql_17",
];

fn postgresql_request(spec: &Spec) -> Built<Value> {
    let name = named(spec, "name", "name")?;
    let database = opt(spec, "database").unwrap_or_else(|| name.clone());
    if !DATABASE.is_match(&database) {
        return input_error("database must be a PostgreSQL identifier: a letter or underscore, then letters, digits, or underscores");
    }
    let package = opt(spec, "package").unwrap_or_else(|| "postgresql".to_string());
    if !POSTGRESQL_PACKAGES.contains(&package.as_str()) {
        return input_error("package must be postgresql, postgresql_14, postgresql_15, postgresql_16, or postgresql_17");
    }
    let port = match spec.get("port") {
        None | Some(Value::Null) => 5432,
        Some(Value::String(text)) if text.is_empty() => 5432,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(-1),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(-1),
        _ => -1,
    };
    if !(1..=65535).contains(&port) {
        return input_error("port must be an integer from 1 to 65535");
    }
    Ok(json!({
        "name": name,
        "database": database,
        "package": package,
        "port": port,
        "targetNode": opt(spec, "targetNode").unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::catalog::find_kind;
    use super::*;

    fn spec(value: Value) -> Spec {
        value.as_object().unwrap().clone()
    }

    fn vm_spec() -> Spec {
        spec(json!({
            "name": "web-01",
            "network": "default",
            "storageBackend": "ceph",
            "storageSizeBytes": "40G",
            "imageUrl": "https://example.com/d.qcow2",
            "imageSha256": "a".repeat(64),
            "sshPublicKeys": ["ssh-ed25519 AAAA operator@host"],
            "cpu": 4,
            "memory": "4G",
            "desiredState": "running",
        }))
    }

    #[test]
    fn sizes_accept_bytes_and_suffixes() {
        assert_eq!(size_bytes("40G", "x").unwrap(), "42949672960");
        assert_eq!(size_bytes("1TiB", "x").unwrap(), "1099511627776");
        assert_eq!(size_bytes("123", "x").unwrap(), "123");
        assert!(size_bytes("forty", "x").is_err());
        assert!(size_bytes("0G", "x").is_err());
    }

    #[test]
    fn a_vm_becomes_a_create_vm_call_with_cloud_init() {
        let calls = build_apply(find_kind(Some("vm")).unwrap(), &vm_spec()).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "createVm");
        let request = &calls[0].request;
        assert_eq!(request["storageBackend"], "STORAGE_BACKEND_TYPE_CEPH");
        assert_eq!(request["storageSizeBytes"], "42949672960");
        assert_eq!(request["spec"]["cpu"], 4);
        assert_eq!(request["spec"]["memoryBytes"], "4294967296");
        assert!(request["cloudInitUserData"]
            .as_str()
            .unwrap()
            .contains("ssh_pwauth: false"));
    }

    #[test]
    fn password_login_is_refused() {
        let mut vm = vm_spec();
        vm.insert("password".into(), json!("hunter2"));
        assert!(build_apply(find_kind(Some("vm")).unwrap(), &vm).is_err());
    }

    #[test]
    fn security_group_rules_parse_from_text() {
        let group = spec(
            json!({"name": "web", "rules": ["tcp/443 from 10.0.0.0/8"], "attachments": [{"kind": "network", "target": "default"}]}),
        );
        let calls = build_apply(find_kind(Some("security-group")).unwrap(), &group).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].request["securityGroup"]["rules"][0]["hostPort"],
            443
        );
        assert_eq!(calls[1].method, "attachSecurityGroup");
    }

    #[test]
    fn reads_pick_get_or_list_by_name() {
        let kind = find_kind(Some("vm")).unwrap();
        assert_eq!(build_read(kind, None).unwrap().method, "listVms");
        assert_eq!(build_read(kind, Some("web-01")).unwrap().method, "getVm");
        assert!(build_read(find_kind(Some("image")).unwrap(), None).is_err());
    }

    #[test]
    fn operations_validate_their_input() {
        let call = build_operation(
            "set-vm-state",
            &spec(json!({"name": "a", "state": "stopped"})),
        )
        .unwrap();
        assert_eq!(call.request["desiredState"], "VM_DESIRED_STATE_STOPPED");
        assert!(build_operation(
            "set-vm-state",
            &spec(json!({"name": "a", "state": "paused"}))
        )
        .is_err());
        assert!(build_operation("nope", &Spec::new()).is_err());
    }

    #[test]
    fn node_install_targets_the_node_agent() {
        let call = build_operation(
            "install-node",
            &spec(json!({"node": "10.0.0.5:9091", "osDisk": "/dev/sda", "joinController": "10.0.0.1:9090"})),
        )
        .unwrap();
        assert_eq!(call.target, Target::Node);
        assert_eq!(call.request["nodeId"], "kcore-node-10-0-0-5");
    }
}
