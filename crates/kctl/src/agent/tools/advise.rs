//! What the operator has to answer before a change, and how answers become a spec.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::catalog::{find_kind, kind_ids, ResourceKind};
use super::names::{is_blank, str_of, Spec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OptionSource {
    #[serde(rename = "networks")]
    Networks,
    #[serde(rename = "ssh-keys")]
    SshKeys,
    #[serde(rename = "nodes")]
    Nodes,
    #[serde(rename = "vms")]
    Vms,
    #[serde(rename = "node-images")]
    NodeImages,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub prompt: String,
    pub why: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice_labels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<OptionSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restrict: Option<bool>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
}

impl Question {
    pub fn new(id: &str, prompt: &str, why: &str, required: bool) -> Self {
        Self {
            id: id.to_string(),
            prompt: prompt.to_string(),
            why: why.to_string(),
            required,
            choices: None,
            choice_labels: None,
            example: None,
            source: None,
            restrict: None,
            kind: None,
            min: None,
        }
    }

    fn choices(mut self, choices: &[&str]) -> Self {
        self.choices = Some(choices.iter().map(|c| c.to_string()).collect());
        self
    }

    fn example(mut self, example: &str) -> Self {
        self.example = Some(example.to_string());
        self
    }

    fn cluster_options(mut self, source: OptionSource, restrict: bool) -> Self {
        self.source = Some(source);
        if restrict {
            self.restrict = Some(true);
        }
        self
    }

    fn integer(mut self, min: i64) -> Self {
        self.kind = Some("integer".to_string());
        self.min = Some(min);
        self
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Advice {
    pub kind: Option<String>,
    pub ready: bool,
    pub blocking: Vec<Question>,
    pub recommended: Vec<Question>,
    pub warnings: Vec<String>,
    pub ask_the_user: String,
    pub next_tool: &'static str,
}

pub struct Desired {
    pub kind: &'static ResourceKind,
    pub spec: Spec,
}

#[derive(Default, Clone)]
pub struct AdviseInput {
    pub kind: Option<String>,
    pub intent: Option<String>,
    pub spec: Spec,
    pub manifest: Option<String>,
    pub accept_defaults: bool,
}

impl AdviseInput {
    pub fn from_args(args: &Value) -> Self {
        let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            kind: text("kind"),
            intent: text("intent"),
            spec: args
                .get("spec")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            manifest: text("manifest"),
            accept_defaults: args.get("acceptDefaults").and_then(Value::as_bool) == Some(true),
        }
    }
}

static INTENT: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let table: [(&str, &str); 15] = [
        (r"(?i)\bvirtual machines?\b|\bvms?\b", "vm"),
        (r"(?i)\bnetworks?\b|\bvxlan\b|\bbridge\b", "network"),
        (r"(?i)\bssh keys?\b", "ssh-key"),
        (r"(?i)\bpostgres(?:ql)?\b", "postgresql"),
        (r"(?i)\bcontainers?\b", "container"),
        (r"(?i)\bvolumes?\b", "volume"),
        (r"(?i)\bsnapshot polic", "snapshot-policy"),
        (r"(?i)\bsnapshots?\b", "volume-snapshot"),
        (r"(?i)\bsecurity groups?\b|\bfirewall\b", "security-group"),
        (r"(?i)\bdisk layouts?\b|\bdisko\b", "disk-layout"),
        (r"(?i)\bceph\b", "ceph-cluster"),
        (
            r"(?i)\bcephfs\b|\bshared filesystems?\b",
            "shared-filesystem",
        ),
        (r"(?i)\bobject stores?\b|\brgw\b|\bs3\b", "object-store"),
        (
            r"(?i)\bcluster updates?\b|\brolling update\b",
            "cluster-update",
        ),
        (r"(?i)\bclusters?\b|\bcontext\b|\bbootstrap\b", "cluster"),
    ];
    table
        .into_iter()
        .map(|(pattern, id)| (Regex::new(pattern).unwrap(), id))
        .collect()
});

pub fn infer_kind(intent: Option<&str>) -> Option<&'static str> {
    let intent = intent?;
    let mut hits: Vec<&'static str> = Vec::new();
    for (pattern, id) in INTENT.iter() {
        if pattern.is_match(intent) && !hits.contains(id) {
            hits.push(id);
        }
    }
    if hits.len() == 1 {
        Some(hits[0])
    } else {
        None
    }
}

fn missing(spec: &Spec, key: &str) -> bool {
    is_blank(spec.get(key))
}

fn keep_needed(items: Vec<Question>, spec: &Spec) -> Vec<Question> {
    items
        .into_iter()
        .filter(|item| item.required || missing(spec, &item.id))
        .collect()
}

fn only_required(items: Vec<Question>) -> Vec<Question> {
    items.into_iter().filter(|item| item.required).collect()
}

pub fn questions_for(kind: &ResourceKind, spec: &Spec) -> Vec<Question> {
    use OptionSource::*;
    let q = Question::new;
    match kind.id {
        "cluster" => keep_needed(
            vec![
                q(
                    "controller",
                    "What is the controller address (host:port)?",
                    "kctl generates a CA and a client certificate for this address, then dials it over mTLS.",
                    missing(spec, "controller"),
                )
                .example("10.0.0.10:9090"),
                q(
                    "name",
                    "What should the local context be called?",
                    "The context name selects this cluster in ~/.kcore/config. 'default' is fine for a single cluster.",
                    false,
                )
                .example("default"),
                q(
                    "force",
                    "Overwrite existing certificates for this context?",
                    "Overwriting replaces the trust root. Existing controllers and nodes that use the current certificates will need new material.",
                    false,
                )
                .choices(&["no", "yes"]),
            ],
            spec,
        ),
        "vm" => vm_questions(spec),
        "network" => keep_needed(
            vec![
                q("name", "What should the network be called?", "VMs attach to this name.", missing(spec, "name")).example("frontend"),
                q(
                    "type",
                    "Which network type: nat, bridge, or vxlan?",
                    "nat hides guests behind one address. bridge puts them on a physical LAN. vxlan is the cross-host overlay.",
                    missing(spec, "type"),
                )
                .choices(&["nat", "bridge", "vxlan"]),
                q(
                    "externalIp",
                    "Which external IP should this network use?",
                    "NAT and DNAT use this address. Bridge networks use an address on the LAN.",
                    missing(spec, "externalIp"),
                )
                .example("203.0.113.10"),
                q(
                    "gatewayIp",
                    "Which gateway IP should the bridge use?",
                    "This is the address guests use as their default gateway.",
                    missing(spec, "gatewayIp"),
                )
                .example("10.240.10.1"),
                q(
                    "internalNetmask",
                    "What is the internal netmask?",
                    "Defaults to 255.255.255.0 when you accept defaults.",
                    false,
                )
                .example("255.255.255.0"),
                q(
                    "eastWestFirewall",
                    "Drop VM-to-VM traffic on this bridge?",
                    "Leaves the gateway and DHCP reachable and drops the rest unless a security group allows it.",
                    false,
                )
                .choices(&["no", "yes"]),
            ],
            spec,
        ),
        "ssh-key" => keep_needed(
            vec![
                q("name", "What should this SSH key be called?", "VMs refer to the key by this name.", missing(spec, "name")),
                q(
                    "publicKey",
                    "Paste the SSH public key.",
                    "The controller stores the public key and can inject it into cloud-init. The private key stays with the operator.",
                    missing(spec, "publicKey"),
                )
                .example("ssh-ed25519 AAAA... operator@host")
                .cluster_options(SshKeys, false),
            ],
            spec,
        ),
        "postgresql" => keep_needed(
            vec![
                q(
                    "name",
                    "What should this PostgreSQL instance be called?",
                    "The name identifies the resource. It is also the database name when you leave database empty and the name is a PostgreSQL identifier.",
                    missing(spec, "name"),
                )
                .example("app"),
                q(
                    "database",
                    "What is the single database name?",
                    "NixOS ensureDatabases creates this database. Use letters, digits, and underscores. Defaults to the instance name.",
                    false,
                )
                .example("app"),
                q(
                    "package",
                    "Which NixOS PostgreSQL package should it use?",
                    "postgresql tracks the nixpkgs default. Pin postgresql_16 when the major version must stay put. This choice is immutable.",
                    false,
                )
                .choices(&["postgresql", "postgresql_14", "postgresql_15", "postgresql_16", "postgresql_17"]),
                q(
                    "port",
                    "Which port should PostgreSQL use?",
                    "Defaults to 5432. v1 still serves clients on the local Unix socket.",
                    false,
                )
                .example("5432"),
                q(
                    "targetNode",
                    "Pin it to a node, or let the scheduler choose a free one?",
                    "v1 runs one PostgreSQL database on a node. targetNode is immutable.",
                    false,
                )
                .cluster_options(Nodes, true),
            ],
            spec,
        ),
        "container" => keep_needed(
            vec![
                q("name", "What should the container be called?", "This is the workload name.", missing(spec, "name")),
                q(
                    "image",
                    "Which OCI image should it run?",
                    "Example: nginx:alpine. The image reference is immutable after create.",
                    missing(spec, "image"),
                )
                .example("nginx:alpine"),
                q(
                    "network",
                    "Which kcore network should it join?",
                    "Optional. Leave empty only if the container should use the runtime default.",
                    false,
                )
                .cluster_options(Networks, true),
            ],
            spec,
        ),
        "volume" => keep_needed(
            vec![
                q("name", "What should the volume be called?", "The name is how VMs and snapshots refer to it.", missing(spec, "name")),
                q(
                    "sizeBytes",
                    "How large should the volume be, in bytes?",
                    "Ceph data volume size. This size is fixed at create.",
                    missing(spec, "sizeBytes"),
                )
                .example("10737418240"),
                q(
                    "encrypt",
                    "Encrypt the volume with host-side LUKS?",
                    "Encryption at rest wraps the data key with the cluster master key. Say yes for data volumes.",
                    false,
                )
                .choices(&["yes", "no"]),
                q(
                    "vm",
                    "Attach it to a stopped VM now, or leave it detached?",
                    "Attach only works while that VM is stopped.",
                    false,
                )
                .cluster_options(Vms, true),
            ],
            spec,
        ),
        "volume-snapshot" => only_required(vec![
            q("volume", "Which volume should be snapshotted?", "Name or id of an existing volume.", missing(spec, "volume")),
            q("name", "What should the snapshot be called?", "A name makes later restore unambiguous.", missing(spec, "name")),
        ]),
        "snapshot-policy" => only_required(vec![
            q("name", "What should the snapshot policy be called?", "Policies are reconciled by name.", missing(spec, "name")),
            q(
                "target",
                "Should this policy cover a VM or a single volume?",
                "Provide vm or volume. One selector is enough.",
                missing(spec, "vm") && missing(spec, "volume"),
            )
            .choices(&["vm", "volume"]),
            q(
                "schedule",
                "How often should it snapshot?",
                "@hourly, @daily, or every:<seconds>.",
                missing(spec, "schedule"),
            )
            .choices(&["@hourly", "@daily"])
            .example("@daily"),
            q(
                "keep",
                "How many snapshots should each volume retain?",
                "Older snapshots beyond this count are removed.",
                missing(spec, "keep"),
            )
            .example("7")
            .integer(1),
        ]),
        "security-group" => only_required(vec![
            q("name", "What should the security group be called?", "Attachments refer to this name.", missing(spec, "name")),
            q(
                "rules",
                "Which ports and protocols should it allow?",
                "Each rule needs protocol and hostPort. sourceCidr limits who can connect.",
                missing(spec, "rules"),
            )
            .example("tcp/443 from 0.0.0.0/0"),
        ]),
        "disk-layout" => only_required(vec![
            q("name", "What should the disk layout be called?", "The controller stores the layout under this name.", missing(spec, "name")),
            q(
                "nodeId",
                "Which node id should receive this layout?",
                "The reconciler pushes the layout only to that node.",
                missing(spec, "nodeId"),
            )
            .cluster_options(Nodes, true),
            q(
                "layout",
                "Paste the layout: layoutNix, layoutNixFile, or diskLayout.",
                "Exactly one of those three. Run plan first so the classifier can refuse a disk that still backs a VM.",
                missing(spec, "layoutNix") && missing(spec, "layoutNixFile") && missing(spec, "diskLayout"),
            ),
        ]),
        "ceph-cluster" => only_required(vec![
            q("name", "What should the Ceph cluster be called?", "Volumes and filesystems refer to this name.", missing(spec, "name")),
            q(
                "publicNetwork",
                "What is the Ceph public network CIDR?",
                "Clients and monitors use this network.",
                missing(spec, "publicNetwork"),
            )
            .example("10.0.0.0/24"),
            q(
                "clusterNetwork",
                "What is the Ceph cluster network CIDR?",
                "Replication traffic uses this network. It can match the public network on a small cluster.",
                missing(spec, "clusterNetwork"),
            )
            .example("10.0.0.0/24"),
        ]),
        "shared-filesystem" => only_required(vec![
            q("name", "What should the shared filesystem be called?", "Clients mount it by this name.", missing(spec, "name")),
            q(
                "cephCluster",
                "Which Ceph cluster should back it?",
                "The filesystem is created inside that cluster.",
                missing(spec, "cephCluster"),
            ),
            q(
                "quotaBytes",
                "What quota, in bytes, should the filesystem have?",
                "0 means no quota. A positive quota caps the filesystem.",
                missing(spec, "quotaBytes"),
            )
            .example("0"),
        ]),
        "object-store" => only_required(vec![
            q("name", "What should the object store be called?", "The RGW service is reconciled under this name.", missing(spec, "name")),
            q("cephCluster", "Which Ceph cluster should back it?", "RGW stores buckets in that cluster.", missing(spec, "cephCluster")),
            q(
                "members",
                "Which node ids should run the gateway?",
                "At least one node id.",
                missing(spec, "members"),
            )
            .example("node-a"),
        ]),
        "cluster-update" => keep_needed(
            vec![
                q("name", "What should the cluster update be called?", "Approve, cancel, and rollback use this name.", missing(spec, "name")),
                q(
                    "version",
                    "Which kcore version is the target?",
                    "The rolling update converges nodes to this version.",
                    missing(spec, "version"),
                )
                .example("0.3.0"),
                q(
                    "flakeRef",
                    "Which Nix flake ref should nodes build?",
                    "Plan resolves this ref before any node reboots.",
                    missing(spec, "flakeRef"),
                ),
                q(
                    "strategy",
                    "One node at a time, or a wider batch?",
                    "one-at-a-time keeps a quorum while a node reboots.",
                    missing(spec, "strategy"),
                )
                .choices(&["one-at-a-time"])
                .example("one-at-a-time"),
            ],
            spec,
        ),
        _ => Vec::new(),
    }
}

fn vm_questions(spec: &Spec) -> Vec<Question> {
    use OptionSource::*;
    let q = Question::new;
    let has_url = !missing(spec, "imageUrl");
    let has_path = !missing(spec, "imagePath");
    let has_key_name = !missing(spec, "sshKeys");
    let has_public_key = !missing(spec, "sshPublicKeys");
    let has_cloud_init = !missing(spec, "cloudInitUserData");
    let password = str_of(spec, "password");
    let compliant = !is_false(spec.get("compliant"));
    let authed =
        has_key_name || has_public_key || has_cloud_init || (!password.is_empty() && !compliant);

    let mut items = vec![
        q(
            "name",
            "What should the VM be called?",
            "The name is the resource id you will start, stop, and migrate.",
            missing(spec, "name"),
        )
        .example("web-01"),
        q(
            "image",
            "Which boot image should it use: an https URL plus SHA256, or a qcow2/raw path already on the node?",
            "kcore checks the SHA256 for URL images. ISO uploads are unsupported.",
            !has_url && !has_path,
        )
        .example("https://cloud.debian.org/.../debian-12-genericcloud-amd64.qcow2")
        .cluster_options(NodeImages, false),
    ];
    if has_url && missing(spec, "imageSha256") {
        items.push(q(
            "imageSha256",
            "What is the SHA256 of that image URL?",
            "The controller refuses a URL image without a 64-character checksum.",
            true,
        ));
    }
    if has_path && missing(spec, "imageFormat") {
        items.push(
            q(
                "imageFormat",
                "Is the node-local image raw or qcow2?",
                "The node boots the path with this format.",
                true,
            )
            .choices(&["qcow2", "raw"]),
        );
    }
    items.push(
        q(
            "network",
            "Which network should the first NIC join?",
            "Create the network first if it does not exist yet.",
            missing(spec, "network"),
        )
        .example("default")
        .cluster_options(Networks, true),
    );
    items.push(
        q(
            "storageBackend",
            "Which storage backend: filesystem, lvm, zfs, or ceph?",
            "The backend is fixed for the life of the VM. ceph is what live migration uses.",
            missing(spec, "storageBackend"),
        )
        .choices(&["filesystem", "lvm", "zfs", "ceph"]),
    );
    items.push(
        q(
            "storageSizeBytes",
            "How large should the VM disk be?",
            "Bytes, or a size such as 40G. The size is fixed after create.",
            missing(spec, "storageSizeBytes"),
        )
        .example("40G"),
    );
    items.push(
        q(
            "auth",
            "Which SSH public key should log in, or which stored ssh-key name should the VM use?",
            "Key login is the compliant default. Password login requires an explicit acknowledgement that it is non-compliant.",
            !authed,
        )
        .example("ssh-ed25519 AAAA... operator@host")
        .cluster_options(SshKeys, false),
    );
    items.push(
        q(
            "cpu",
            "How many vCPUs?",
            "Mutable later. 2 is a reasonable lab default if you accept defaults.",
            false,
        )
        .example("2")
        .integer(1),
    );
    items.push(
        q(
            "memory",
            "How much memory?",
            "Mutable later. A size such as 4G.",
            false,
        )
        .example("4G"),
    );
    items.push(
        q(
            "desiredState",
            "Should the VM be running or stopped after apply?",
            "This is the desired power state. Omit it to leave an existing VM's power state alone.",
            false,
        )
        .choices(&["running", "stopped"]),
    );
    items.push(
        q(
            "placement",
            "Pin it to a node or datacenter, or let the scheduler choose?",
            "targetNode is immutable. Leaving it empty lets the controller place the VM.",
            false,
        )
        .cluster_options(Nodes, false),
    );
    items
        .into_iter()
        .filter(|item| {
            if item.required {
                return true;
            }
            match item.id.as_str() {
                "cpu" => missing(spec, "cpu"),
                "memory" => missing(spec, "memory") && missing(spec, "memoryBytes"),
                "desiredState" => missing(spec, "desiredState"),
                "placement" => missing(spec, "targetNode") && missing(spec, "dc"),
                _ => true,
            }
        })
        .collect()
}

fn is_false(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(false)) => true,
        Some(Value::String(text)) => text == "false",
        _ => false,
    }
}

pub fn warnings_for(kind: &ResourceKind, spec: &Spec) -> Vec<String> {
    let mut warnings = Vec::new();
    if kind.id == "vm" {
        let password = str_of(spec, "password");
        let compliant = !is_false(spec.get("compliant"));
        if !password.is_empty() && compliant {
            warnings.push("Password login is non-compliant. Apply will be refused until compliant is false and the operator has accepted that risk.".to_string());
        }
        if !password.is_empty() && !compliant {
            warnings.push(
                "This VM will allow password login. Prefer an SSH key for anything beyond a lab."
                    .to_string(),
            );
        }
        if str_of(spec, "storageBackend") == "ceph" {
            warnings.push("Ceph-backed VMs can live-migrate. Confirm the Ceph cluster is healthy before relying on that.".to_string());
        }
    }
    let forced =
        matches!(spec.get("force"), Some(Value::Bool(true))) || str_of(spec, "force") == "yes";
    if kind.id == "cluster" && forced {
        warnings.push("force replaces the local trust root for this context.".to_string());
    }
    if kind.id == "disk-layout" {
        warnings.push("Plan this layout before apply. The node refuses a disk that still backs a VM, an LVM PV, or a ZFS pool member.".to_string());
    }
    if kind.id == "postgresql" {
        warnings.push("v1 is one database per node, from the NixOS postgresql package, on the local Unix socket. Deleting the resource removes it from the node config and leaves /var/lib/postgresql on disk.".to_string());
    }
    if kind.id == "cluster-update" {
        warnings.push("A cluster update can reboot nodes. Plan it, read the blockers, and apply only after the operator agrees.".to_string());
    }
    if !kind.immutable.is_empty() {
        warnings.push(format!(
            "Immutable after create: {}. Changing those later means delete and create, which the operator must agree to.",
            kind.immutable.join(", ")
        ));
    }
    warnings
}

pub struct Normalized {
    pub desired: Option<Desired>,
    pub kind_error: Option<String>,
}

fn as_map(value: Option<&Value>) -> Option<&Spec> {
    value.and_then(Value::as_object)
}

pub fn normalize_desired(input: &AdviseInput) -> Normalized {
    let mut doc: Option<Spec> = None;
    if let Some(manifest) = input.manifest.as_deref().filter(|m| !m.trim().is_empty()) {
        match serde_yaml::from_str::<Value>(manifest) {
            Ok(Value::Object(map)) => doc = Some(map),
            _ => {
                return Normalized {
                    desired: None,
                    kind_error: Some("manifest must be a YAML mapping".to_string()),
                }
            }
        }
    }

    let yaml_kind = doc
        .as_ref()
        .and_then(|d| d.get("kind"))
        .and_then(Value::as_str);
    let kind = find_kind(input.kind.as_deref())
        .or_else(|| find_kind(yaml_kind))
        .or_else(|| find_kind(infer_kind(input.intent.as_deref())));
    let Some(kind) = kind else {
        return Normalized {
            desired: None,
            kind_error: Some(format!(
                "Which resource? One of: {}.",
                kind_ids().join(", ")
            )),
        };
    };

    let metadata_name = as_map(doc.as_ref().and_then(|d| d.get("metadata")))
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut spec: Spec = as_map(doc.as_ref().and_then(|d| d.get("spec")))
        .cloned()
        .unwrap_or_default();
    for (key, value) in &input.spec {
        spec.insert(key.clone(), value.clone());
    }
    if let Some(name) = metadata_name {
        if is_blank(spec.get("name")) {
            spec.insert("name".to_string(), Value::String(name));
        }
    }
    flatten_manifest(kind.id, &mut spec);
    Normalized {
        desired: Some(Desired { kind, spec }),
        kind_error: None,
    }
}

fn flatten_manifest(kind_id: &str, spec: &mut Spec) {
    if kind_id != "vm" {
        return;
    }
    if let Some(disk) = spec
        .get("disks")
        .and_then(Value::as_array)
        .and_then(|disks| disks.first())
        .and_then(Value::as_object)
        .cloned()
    {
        for (from, to) in [
            ("image", "imageUrl"),
            ("sha256", "imageSha256"),
            ("path", "imagePath"),
            ("format", "imageFormat"),
        ] {
            if let Some(text) = disk.get(from).and_then(Value::as_str) {
                if is_blank(spec.get(to)) {
                    spec.insert(to.to_string(), Value::String(text.to_string()));
                }
            }
        }
    }
    if let Some(nics) = spec.get("nics").and_then(Value::as_array).cloned() {
        let networks: Vec<String> = nics
            .iter()
            .filter_map(|nic| {
                nic.as_object()
                    .and_then(|n| n.get("network"))
                    .and_then(Value::as_str)
            })
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string)
            .collect();
        if let Some(first) = networks.first() {
            if is_blank(spec.get("network")) {
                spec.insert("network".to_string(), json!(first));
            }
        }
        if networks.len() > 1 && is_blank(spec.get("extraNetworks")) {
            spec.insert("extraNetworks".to_string(), json!(networks[1..]));
        }
    }
}

pub fn advise(input: &AdviseInput) -> Advice {
    let normalized = normalize_desired(input);
    let Some(desired) = normalized.desired else {
        let prompt = normalized
            .kind_error
            .unwrap_or_else(|| "Which kcore resource should this be?".to_string());
        let mut question = Question::new(
            "kind",
            &prompt,
            "Each resource has its own required questions and immutable fields.",
            true,
        );
        question.choices = Some(kind_ids().into_iter().map(str::to_string).collect());
        let blocking = vec![question];
        return Advice {
            kind: None,
            ready: false,
            ask_the_user: format_ask(&blocking, &[]),
            blocking,
            recommended: Vec::new(),
            warnings: Vec::new(),
            next_tool: "kcore_advise",
        };
    };

    let all = questions_for(desired.kind, &desired.spec);
    let blocking: Vec<Question> = all.iter().filter(|q| q.required).cloned().collect();
    let recommended: Vec<Question> = if input.accept_defaults {
        Vec::new()
    } else {
        all.iter().filter(|q| !q.required).cloned().collect()
    };
    let ready = blocking.is_empty();
    Advice {
        kind: Some(desired.kind.id.to_string()),
        ready,
        ask_the_user: format_ask(&blocking, &recommended),
        warnings: warnings_for(desired.kind, &desired.spec),
        blocking,
        recommended,
        next_tool: if ready { "kcore_plan" } else { "kcore_advise" },
    }
}

fn format_ask(blocking: &[Question], recommended: &[Question]) -> String {
    if blocking.is_empty() && recommended.is_empty() {
        return "The required answers are present. Show the plan and wait for an explicit yes before apply.".to_string();
    }
    let mut lines = Vec::new();
    if !blocking.is_empty() {
        lines.push(
            "Ask the operator these questions and wait for the answers before planning:"
                .to_string(),
        );
        for item in blocking {
            lines.push(format!("- {} ({})", item.prompt, item.why));
        }
    }
    if !recommended.is_empty() {
        lines.push("Also ask these, unless the operator already said to use defaults:".to_string());
        for item in recommended {
            lines.push(format!("- {} ({})", item.prompt, item.why));
        }
    }
    lines.join("\n")
}

pub fn merge_answers(spec: &Spec, answers: &Spec) -> Spec {
    const SKIPPED: [&str; 6] = ["kind", "image", "auth", "placement", "target", "layout"];
    let mut next = spec.clone();
    for (key, value) in answers {
        if SKIPPED.contains(&key.as_str()) || is_blank(Some(value)) {
            continue;
        }
        next.insert(key.clone(), value.clone());
    }
    let text = |key: &str| answers.get(key).and_then(Value::as_str).map(str::trim);
    if let Some(image) = text("image") {
        if image.starts_with("https://") && is_blank(next.get("imageUrl")) {
            next.insert("imageUrl".to_string(), json!(image));
        }
        if image.starts_with('/') && is_blank(next.get("imagePath")) {
            next.insert("imagePath".to_string(), json!(image));
        }
    }
    if let Some(auth) = text("auth") {
        if auth.starts_with("ssh-") && is_blank(next.get("sshPublicKeys")) {
            next.insert("sshPublicKeys".to_string(), json!([auth]));
        } else if !auth.is_empty() && is_blank(next.get("sshKeys")) && !auth.starts_with("ssh-") {
            next.insert("sshKeys".to_string(), json!([auth]));
        }
    }
    if let Some(placement) = answers.get("placement").and_then(Value::as_str) {
        if placement.contains(':') && is_blank(next.get("targetNode")) {
            next.insert("targetNode".to_string(), json!(placement.trim()));
        }
    }
    if let Some(memory) = text("memory") {
        if !memory.is_empty() && is_blank(next.get("memoryBytes")) {
            next.insert("memoryBytes".to_string(), json!(memory));
        }
    }
    if text("target") == Some("vm") {
        if let Some(vm) = answers.get("vm").and_then(Value::as_str) {
            next.insert("vm".to_string(), json!(vm));
        }
    }
    if text("target") == Some("volume") {
        if let Some(volume) = answers.get("volume").and_then(Value::as_str) {
            next.insert("volume".to_string(), json!(volume));
        }
    }
    next
}

/// Distinct sources the questions need, in first-seen order.
pub fn sources_of(questions: &[Question]) -> Vec<OptionSource> {
    let mut seen = HashSet::new();
    questions
        .iter()
        .filter_map(|q| q.source)
        .filter(|source| seen.insert(*source))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_vm() -> Spec {
        json!({
            "name": "web-01",
            "imageUrl": "https://example.com/debian.qcow2",
            "imageSha256": "a".repeat(64),
            "network": "default",
            "storageBackend": "filesystem",
            "storageSizeBytes": "40G",
            "sshPublicKeys": ["ssh-ed25519 AAAA operator@host"],
            "cpu": "2",
            "memoryBytes": "4G",
            "desiredState": "running",
            "targetNode": "node-a",
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn input(kind: &str, spec: Spec) -> AdviseInput {
        AdviseInput {
            kind: Some(kind.to_string()),
            spec,
            ..AdviseInput::default()
        }
    }

    #[test]
    fn a_blank_vm_request_asks_for_the_decisions_that_change_the_machine() {
        let advice = advise(&input("vm", Spec::new()));
        assert!(!advice.ready);
        let ids: Vec<&str> = advice.blocking.iter().map(|q| q.id.as_str()).collect();
        for wanted in [
            "name",
            "image",
            "network",
            "storageBackend",
            "storageSizeBytes",
            "auth",
        ] {
            assert!(ids.contains(&wanted), "{wanted} missing from {ids:?}");
        }
        assert!(advice.ask_the_user.contains("Ask the operator"));
    }

    #[test]
    fn a_complete_vm_spec_is_ready_to_plan() {
        let advice = advise(&input("vm", complete_vm()));
        assert!(advice.ready);
        assert!(advice.blocking.is_empty());
        assert_eq!(advice.next_tool, "kcore_plan");
    }

    #[test]
    fn an_ambiguous_request_asks_which_resource() {
        let advice = advise(&AdviseInput {
            intent: Some("make something".to_string()),
            ..AdviseInput::default()
        });
        assert!(advice.kind.is_none());
        assert_eq!(advice.blocking[0].id, "kind");
    }

    #[test]
    fn intent_can_select_a_single_resource() {
        assert_eq!(
            infer_kind(Some("create a virtual machine for the web tier")),
            Some("vm")
        );
        assert_eq!(infer_kind(Some("bootstrap the cluster")), Some("cluster"));
        assert_eq!(infer_kind(Some("a vm on a network")), None);
    }

    #[test]
    fn password_login_is_called_out_before_apply() {
        let mut spec = complete_vm();
        spec.insert("password".into(), json!("secret"));
        spec.insert("compliant".into(), json!(true));
        let advice = advise(&input("vm", spec));
        assert!(advice.warnings.iter().any(|w| w.contains("non-compliant")));
    }

    #[test]
    fn elicited_answers_fill_image_and_ssh_key_fields() {
        let answers = json!({
            "image": "https://example.com/debian.qcow2",
            "auth": "ssh-ed25519 AAAA operator@host",
            "memory": "4G",
        });
        let spec = merge_answers(&Spec::new(), answers.as_object().unwrap());
        assert_eq!(spec["imageUrl"], "https://example.com/debian.qcow2");
        assert_eq!(
            spec["sshPublicKeys"],
            json!(["ssh-ed25519 AAAA operator@host"])
        );
        assert_eq!(spec["memoryBytes"], "4G");
    }

    #[test]
    fn a_stored_key_name_goes_to_ssh_keys() {
        let answers = json!({"auth": "ops-key"});
        let spec = merge_answers(&Spec::new(), answers.as_object().unwrap());
        assert_eq!(spec["sshKeys"], json!(["ops-key"]));
    }

    #[test]
    fn a_vm_manifest_answers_the_image_and_network_questions() {
        let advice = advise(&AdviseInput {
            kind: Some("vm".to_string()),
            manifest: Some(
                "kind: VM\nmetadata:\n  name: web-01\nspec:\n  storageBackend: filesystem\n  disks:\n    - image: https://example.com/a.qcow2\n      sha256: abc\n  nics:\n    - network: default\n"
                    .to_string(),
            ),
            ..AdviseInput::default()
        });
        let ids: Vec<&str> = advice.blocking.iter().map(|q| q.id.as_str()).collect();
        assert!(!ids.contains(&"name"));
        assert!(!ids.contains(&"image"));
        assert!(!ids.contains(&"network"));
        assert!(!ids.contains(&"imageSha256"));
    }

    #[test]
    fn a_bad_manifest_is_reported() {
        let advice = advise(&AdviseInput {
            manifest: Some("- just\n- a list".to_string()),
            ..AdviseInput::default()
        });
        assert!(advice.blocking[0].prompt.contains("YAML mapping"));
    }

    #[test]
    fn accept_defaults_drops_the_recommended_questions() {
        let mut spec = complete_vm();
        spec.remove("cpu");
        let with = advise(&input("vm", spec.clone()));
        assert!(!with.recommended.is_empty());
        let mut skipping = input("vm", spec);
        skipping.accept_defaults = true;
        assert!(advise(&skipping).recommended.is_empty());
    }
}
