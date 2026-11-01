//! `kctl apply -f` for the kinds that were imperative commands only:
//! `Volume`, `SnapshotPolicy` and `Operator`.
//!
//! Volumes are immutable, so applying one that exists reports it and changes
//! nothing. A snapshot policy is an upsert. An operator is created when missing
//! and its roles are reconciled to the list in the manifest.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::client::{self, controller_proto as proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Deserialize)]
struct Metadata {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Size {
    Bytes(i64),
    Text(String),
}

impl Size {
    fn bytes(&self) -> Result<i64> {
        match self {
            Size::Bytes(value) => Ok(*value),
            Size::Text(text) => client::parse_size_bytes(text).map_err(|e| anyhow::anyhow!(e)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VolumeSpec {
    #[serde(default)]
    size_bytes: Option<Size>,
    #[serde(default)]
    storage_class: Option<String>,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    volume_name: Option<String>,
    #[serde(default)]
    vm: Option<String>,
    #[serde(default)]
    from_snapshot: Option<String>,
    #[serde(default)]
    encrypt: bool,
}

#[derive(Debug, Deserialize)]
struct VolumeManifest {
    kind: String,
    metadata: Metadata,
    spec: VolumeSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotPolicySpec {
    #[serde(default)]
    vm: Option<String>,
    #[serde(default)]
    volume: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    keep: Option<i32>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct SnapshotPolicyManifest {
    kind: String,
    metadata: Metadata,
    spec: SnapshotPolicySpec,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperatorSpec {
    #[serde(default)]
    roles: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct OperatorManifest {
    kind: String,
    metadata: Metadata,
    #[serde(default)]
    spec: OperatorSpec,
}

fn is_kind(kind: &str, expected: &str) -> bool {
    let squash = |text: &str| text.trim().to_ascii_lowercase().replace(['-', '_'], "");
    squash(kind) == squash(expected)
}

fn parse_volume(data: &str) -> Result<VolumeManifest> {
    let manifest: VolumeManifest = serde_yaml::from_str(data).context("parsing Volume YAML")?;
    if !is_kind(&manifest.kind, "Volume") {
        bail!("expected kind: Volume, got {}", manifest.kind);
    }
    if manifest.metadata.name.trim().is_empty() {
        bail!("metadata.name is required");
    }
    let storage_class = manifest.spec.storage_class.as_deref().unwrap_or("ceph");
    if !matches!(storage_class, "ceph" | "lvm" | "zfs") {
        bail!("spec.storageClass must be ceph, lvm, or zfs, got {storage_class}");
    }
    if storage_class != "ceph" && manifest.spec.node.as_deref().unwrap_or("").is_empty() {
        bail!("spec.node is required for {storage_class} volumes");
    }
    let from_snapshot = manifest.spec.from_snapshot.as_deref().unwrap_or("");
    let size = match &manifest.spec.size_bytes {
        Some(size) => size.bytes().context("spec.sizeBytes")?,
        None => 0,
    };
    if size < 0 || (size == 0 && from_snapshot.is_empty()) {
        bail!("spec.sizeBytes must be positive unless spec.fromSnapshot is set");
    }
    Ok(manifest)
}

fn parse_snapshot_policy(data: &str) -> Result<SnapshotPolicyManifest> {
    let manifest: SnapshotPolicyManifest =
        serde_yaml::from_str(data).context("parsing SnapshotPolicy YAML")?;
    if !is_kind(&manifest.kind, "SnapshotPolicy") {
        bail!("expected kind: SnapshotPolicy, got {}", manifest.kind);
    }
    if manifest.metadata.name.trim().is_empty() {
        bail!("metadata.name is required");
    }
    let vm = manifest.spec.vm.as_deref().unwrap_or("");
    let volume = manifest.spec.volume.as_deref().unwrap_or("");
    if vm.is_empty() == volume.is_empty() {
        bail!("set exactly one of spec.vm and spec.volume");
    }
    if manifest.spec.keep.is_some_and(|keep| keep <= 0) {
        bail!("spec.keep must be positive");
    }
    Ok(manifest)
}

fn parse_operator(data: &str) -> Result<OperatorManifest> {
    let manifest: OperatorManifest = serde_yaml::from_str(data).context("parsing Operator YAML")?;
    if !is_kind(&manifest.kind, "Operator") {
        bail!("expected kind: Operator, got {}", manifest.kind);
    }
    if manifest.metadata.name.trim().is_empty() {
        bail!("metadata.name is required");
    }
    for role in manifest.spec.roles.iter().flatten() {
        role_from_text(role)?;
    }
    Ok(manifest)
}

fn role_from_text(text: &str) -> Result<proto::OperatorRoleKind> {
    match text.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "read-only" => Ok(proto::OperatorRoleKind::ReadOnly),
        "vm-admin" => Ok(proto::OperatorRoleKind::VmAdmin),
        "cluster-admin" => Ok(proto::OperatorRoleKind::ClusterAdmin),
        "backup-operator" => Ok(proto::OperatorRoleKind::BackupOperator),
        other => bail!(
            "unknown role '{other}'. Use read-only, vm-admin, cluster-admin, or backup-operator"
        ),
    }
}

fn not_found(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::NotFound
}

pub async fn apply_volume(info: &ConnectionInfo, file: &str) -> Result<()> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest = parse_volume(&data).with_context(|| format!("in {file}"))?;
    let name = manifest.metadata.name.trim().to_string();
    let mut client = client::controller_client(info).await?;
    match client
        .get_volume(proto::GetVolumeRequest { name: name.clone() })
        .await
    {
        Ok(response) => {
            if response.into_inner().volume.is_some() {
                println!(
                    "volume '{name}' already exists; volumes are immutable, nothing to change"
                );
                return Ok(());
            }
        }
        Err(status) if not_found(&status) => {}
        Err(status) => return Err(status.into()),
    }
    let spec = manifest.spec;
    let size = match &spec.size_bytes {
        Some(size) => size.bytes()?,
        None => 0,
    };
    let response = client
        .create_volume(proto::CreateVolumeRequest {
            name,
            size_bytes: size,
            storage_class: spec.storage_class.unwrap_or_else(|| "ceph".into()),
            vm: spec.vm.unwrap_or_default(),
            guest_format_json: String::new(),
            from_snapshot: spec.from_snapshot.unwrap_or_default(),
            encrypt: spec.encrypt,
            node: spec.node.unwrap_or_default(),
            volume_name: spec.volume_name.unwrap_or_default(),
        })
        .await?
        .into_inner();
    if !response.success {
        bail!("{}", response.message);
    }
    println!("{}", response.message);
    Ok(())
}

pub async fn apply_snapshot_policy(info: &ConnectionInfo, file: &str) -> Result<()> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest = parse_snapshot_policy(&data).with_context(|| format!("in {file}"))?;
    let spec = manifest.spec;
    crate::commands::volume::create_snapshot_policy(
        info,
        manifest.metadata.name.trim(),
        spec.vm,
        spec.volume,
        spec.schedule.unwrap_or_else(|| "@daily".into()),
        spec.keep.unwrap_or(7),
        !spec.disabled,
    )
    .await
}

/// Roles to grant and to revoke so that `current` becomes `wanted`.
fn role_changes(
    current: &[i32],
    wanted: &[proto::OperatorRoleKind],
) -> (Vec<proto::OperatorRoleKind>, Vec<proto::OperatorRoleKind>) {
    let grant = wanted
        .iter()
        .copied()
        .filter(|role| !current.contains(&(*role as i32)))
        .collect();
    let revoke = current
        .iter()
        .filter_map(|value| proto::OperatorRoleKind::try_from(*value).ok())
        .filter(|role| *role != proto::OperatorRoleKind::Unspecified && !wanted.contains(role))
        .collect();
    (grant, revoke)
}

pub async fn apply_operator(info: &ConnectionInfo, file: &str) -> Result<()> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest = parse_operator(&data).with_context(|| format!("in {file}"))?;
    let name = manifest.metadata.name.trim().to_string();
    let mut client = client::controller_client(info).await?;
    let current = match client
        .get_operator(proto::GetOperatorRequest { name: name.clone() })
        .await
    {
        Ok(response) => response.into_inner().operator,
        Err(status) if not_found(&status) => None,
        Err(status) => return Err(status.into()),
    };
    let existing = current.is_some();
    if !existing {
        client
            .create_operator(proto::CreateOperatorRequest { name: name.clone() })
            .await?;
        println!("operator '{name}' created");
    }
    let Some(roles) = manifest.spec.roles else {
        if existing {
            println!(
                "operator '{name}' already exists; spec.roles is not set, roles left as they are"
            );
        }
        return Ok(());
    };
    let wanted = roles
        .iter()
        .map(|role| role_from_text(role))
        .collect::<Result<Vec<_>>>()?;
    let held = current.map(|operator| operator.roles).unwrap_or_default();
    let (grant, revoke) = role_changes(&held, &wanted);
    for role in grant {
        client
            .grant_operator_role(proto::GrantOperatorRoleRequest {
                operator_name: name.clone(),
                role: role as i32,
            })
            .await?;
        println!("granted {role:?} to '{name}'");
    }
    for role in revoke {
        client
            .revoke_operator_role(proto::RevokeOperatorRoleRequest {
                operator_name: name.clone(),
                role: role as i32,
            })
            .await?;
        println!("revoked {role:?} from '{name}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_takes_a_size_as_text_or_a_number() {
        let text =
            parse_volume("kind: Volume\nmetadata: {name: d}\nspec: {sizeBytes: 40G}\n").unwrap();
        assert_eq!(text.spec.size_bytes.unwrap().bytes().unwrap(), 40 << 30);
        let number =
            parse_volume("kind: Volume\nmetadata: {name: d}\nspec: {sizeBytes: 1048576}\n")
                .unwrap();
        assert_eq!(number.spec.size_bytes.unwrap().bytes().unwrap(), 1 << 20);
    }

    #[test]
    fn a_volume_is_checked_before_it_is_sent() {
        let bad = |yaml: &str| parse_volume(yaml).unwrap_err().to_string();
        assert!(bad("kind: Volume\nmetadata: {name: d}\nspec: {}\n").contains("sizeBytes"));
        assert!(bad(
            "kind: Volume\nmetadata: {name: d}\nspec: {sizeBytes: 1G, storageClass: nfs}\n"
        )
        .contains("ceph, lvm, or zfs"));
        assert!(bad(
            "kind: Volume\nmetadata: {name: d}\nspec: {sizeBytes: 1G, storageClass: lvm}\n"
        )
        .contains("spec.node"));
        assert!(
            bad("kind: Network\nmetadata: {name: d}\nspec: {}\n").contains("expected kind: Volume")
        );
        assert!(
            parse_volume("kind: Volume\nmetadata: {name: d}\nspec: {fromSnapshot: nightly}\n")
                .is_ok()
        );
        assert!(parse_volume("kind: Volume\nmetadata: {name: d}\nspec: {sizeBytes: 1G, storageClass: lvm, node: n1}\n").is_ok());
    }

    #[test]
    fn a_snapshot_policy_names_one_selector() {
        let ok = parse_snapshot_policy(
            "kind: SnapshotPolicy\nmetadata: {name: p}\nspec: {vm: web, keep: 3}\n",
        )
        .unwrap();
        assert_eq!(ok.spec.keep, Some(3));
        let both = parse_snapshot_policy(
            "kind: SnapshotPolicy\nmetadata: {name: p}\nspec: {vm: web, volume: d}\n",
        );
        assert!(both.unwrap_err().to_string().contains("exactly one"));
        let none = parse_snapshot_policy("kind: SnapshotPolicy\nmetadata: {name: p}\nspec: {}\n");
        assert!(none.unwrap_err().to_string().contains("exactly one"));
        let keep = parse_snapshot_policy(
            "kind: SnapshotPolicy\nmetadata: {name: p}\nspec: {volume: d, keep: 0}\n",
        );
        assert!(keep.unwrap_err().to_string().contains("keep"));
    }

    #[test]
    fn operator_roles_are_validated_and_optional() {
        let with = parse_operator(
            "kind: Operator\nmetadata: {name: alice}\nspec: {roles: [vm-admin, read_only]}\n",
        )
        .unwrap();
        assert_eq!(with.spec.roles.unwrap().len(), 2);
        assert!(parse_operator("kind: Operator\nmetadata: {name: alice}\n")
            .unwrap()
            .spec
            .roles
            .is_none());
        let bad =
            parse_operator("kind: Operator\nmetadata: {name: alice}\nspec: {roles: [root]}\n");
        assert!(bad.unwrap_err().to_string().contains("unknown role"));
    }

    #[test]
    fn role_changes_grant_what_is_missing_and_revoke_what_is_not_listed() {
        use proto::OperatorRoleKind::*;
        let (grant, revoke) =
            role_changes(&[ReadOnly as i32, VmAdmin as i32], &[VmAdmin, ClusterAdmin]);
        assert_eq!(grant, vec![ClusterAdmin]);
        assert_eq!(revoke, vec![ReadOnly]);
        let (grant, revoke) = role_changes(&[], &[]);
        assert!(grant.is_empty() && revoke.is_empty());
        let (_, revoke) = role_changes(&[0, ReadOnly as i32], &[ReadOnly]);
        assert!(revoke.is_empty());
    }

    #[test]
    fn shipped_examples_parse() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
        let read = |path: &str| std::fs::read_to_string(format!("{dir}/{path}")).unwrap();
        parse_volume(&read("storage/volume.yaml")).unwrap();
        parse_snapshot_policy(&read("storage/snapshot-policy.yaml")).unwrap();
        parse_operator(&read("operator/alice.yaml")).unwrap();
    }
}
