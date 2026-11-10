use crate::client::{self, controller_proto};
use crate::commands::{
    ceph_cluster, cluster_update, container, disk_layout, kubernetes, network, node, object_store,
    object_user, postgresql, resource_manifests, security_group, shared_filesystem, slurm, ssh_key,
    vm,
};
use crate::config::ConnectionInfo;
use anyhow::{Context, Result};

/// Manifest kinds that `kctl` handles entirely on the client side, without
/// reaching the controller. Cluster/NodeInstall manifests are bootstrap
/// resources — they configure how to *find* a controller and therefore can't
/// require one to be reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalManifestKind {
    Cluster,
    NodeInstall,
}

/// Outcome of reading and classifying a manifest file once. The whole point
/// of this struct is so callers (e.g. `main.rs`) can avoid reading and YAML-
/// parsing the same file two or three times in a row.
#[derive(Debug, Clone)]
pub struct ClassifiedManifest {
    /// Raw file content, ready to forward to the controller or print as-is.
    pub content: String,
    /// `kind` field value as written in the manifest (case-preserved), or
    /// `None` when the manifest has no `kind:` key.
    pub kind: Option<String>,
    /// `Some` iff `kind` matches a kind handled locally by `kctl`.
    pub local: Option<LocalManifestKind>,
}

/// Read `file`, parse the YAML, and figure out whether the manifest belongs
/// to the controller or to one of the local bootstrap handlers.
///
/// Errors are propagated for both I/O failures (file missing / unreadable)
/// and YAML parse failures, so users see a real diagnostic instead of a
/// silent fallback to the controller path that would then fail with a far
/// less useful message.
pub fn classify_manifest(file: &str) -> Result<ClassifiedManifest> {
    let content = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let kind = detect_manifest_kind(&content)
        .with_context(|| format!("parsing manifest YAML in {file}"))?;
    let local = kind.as_deref().and_then(local_manifest_kind);
    Ok(ClassifiedManifest {
        content,
        kind,
        local,
    })
}

/// Returns `Ok(true)` if the manifest at `file` has a `kind` that is handled
/// locally by `kctl` (i.e. without contacting the controller). I/O errors and
/// YAML parse errors are propagated rather than collapsed to `false`, so
/// callers can surface real manifest problems instead of silently falling
/// back to the controller path.
pub fn is_local_manifest_kind(file: &str) -> Result<bool> {
    Ok(classify_manifest(file)?.local.is_some())
}

fn local_manifest_kind(kind: &str) -> Option<LocalManifestKind> {
    match kind.to_ascii_lowercase().as_str() {
        "cluster" => Some(LocalManifestKind::Cluster),
        "nodeinstall" | "node-install" | "node_install" => Some(LocalManifestKind::NodeInstall),
        _ => None,
    }
}

/// Kinds `kctl apply -f` routes. A manifest with any other `kind:` is refused
/// rather than sent to the controller as a NixOS configuration.
pub const SUPPORTED_KINDS: &[&str] = &[
    "VM",
    "Container",
    "Network",
    "SshKey",
    "SecurityGroup",
    "PostgreSQL",
    "DiskLayout",
    "CephCluster",
    "SharedFilesystem",
    "ObjectStore",
    "ObjectUser",
    "KubernetesCluster",
    "ClusterUpdate",
    "SlurmCluster",
    "PostgresqlCluster",
    "Volume",
    "SnapshotPolicy",
    "Operator",
    "IngressGateway",
    "IngressRoute",
    "WafPolicy",
    "NodeVgpu",
    "Cluster",
    "NodeInstall",
];

pub async fn apply(info: &ConnectionInfo, file: &str, dry_run: bool) -> Result<()> {
    let classified = classify_manifest(file)?;

    if let Some(local) = classified.local {
        anyhow::bail!(
            "internal error: manifest kind {local:?} must be handled locally; do not route through apply()"
        );
    }

    if dry_run {
        println!("--- dry run ---");
        print!("{}", classified.content);
        println!("--- end ---");
        return Ok(());
    }

    if let Some(kind) = classified.kind.as_deref() {
        match kind.to_ascii_lowercase().as_str() {
            "securitygroup" => return security_group::apply_from_file(info, file).await,
            "vm" => return vm::create_from_manifest(info, file).await,
            "network" => return network::create_from_manifest(info, file).await,
            "sshkey" | "ssh-key" | "ssh_key" => {
                return ssh_key::create_from_manifest(info, file).await
            }
            "container" => return container::create_from_manifest(info, file).await,
            "postgresql" | "postgres" => return postgresql::create_from_manifest(info, file).await,
            "slurmcluster" | "slurm-cluster" | "slurm_cluster" => {
                return slurm::apply_from_file(info, file).await
            }
            "postgresqlcluster" | "postgresql-cluster" | "postgresql_cluster" => {
                return crate::commands::pg_cluster::apply_from_file(info, file, true, false).await
            }
            "disklayout" | "disk-layout" | "disk_layout" => {
                return disk_layout::apply_from_file(info, file).await
            }
            "cephcluster" | "ceph-cluster" | "ceph_cluster" => {
                return ceph_cluster::apply_from_file(info, file).await
            }
            "sharedfilesystem" | "shared-filesystem" | "shared_filesystem" => {
                return shared_filesystem::apply_from_file(info, file).await
            }
            k if crate::commands::ingress::canonical_kind(k).is_some() => {
                return crate::commands::ingress::apply_from_file(info, file).await
            }
            "objectstore" | "object-store" | "object_store" => {
                return object_store::apply_from_file(info, file).await
            }
            "nodevgpu" | "node-vgpu" | "node_vgpu" => {
                return node::apply_vgpu_from_file(info, file).await
            }
            "volume" => return resource_manifests::apply_volume(info, file).await,
            "snapshotpolicy" | "snapshot-policy" | "snapshot_policy" => {
                return resource_manifests::apply_snapshot_policy(info, file).await
            }
            "operator" => return resource_manifests::apply_operator(info, file).await,
            "objectuser" | "object-user" | "object_user" => {
                return object_user::apply_from_file(info, file).await
            }
            "kubernetescluster" | "kubernetes-cluster" | "kubernetes_cluster" => {
                return kubernetes::apply_from_file(info, file).await
            }
            "clusterupdate" | "cluster-update" | "cluster_update" => {
                return cluster_update::apply(info, file).await
            }
            other => anyhow::bail!(
                "unsupported manifest kind '{other}' for kctl apply. Supported kinds: {}",
                SUPPORTED_KINDS.join(", ")
            ),
        }
    }

    let mut client = client::controller_admin_client(info).await?;
    let resp = client
        .apply_nix_config(controller_proto::ApplyNixConfigRequest {
            configuration_nix: classified.content,
            rebuild: true,
        })
        .await?
        .into_inner();

    if resp.success {
        println!("{}", resp.message);
        Ok(())
    } else {
        anyhow::bail!("Apply failed: {}", resp.message);
    }
}

/// Split a multi-document YAML file (`---` separators) into one string per
/// document. Empty documents, such as a trailing `---`, are dropped. Comments
/// are not preserved, which is fine because every handler re-parses the text.
pub fn split_documents(content: &str) -> Result<Vec<String>> {
    use serde::Deserialize;
    let mut documents = Vec::new();
    for (index, document) in serde_yaml::Deserializer::from_str(content).enumerate() {
        let value = serde_yaml::Value::deserialize(document)
            .with_context(|| format!("invalid YAML in document {}", index + 1))?;
        if value.is_null() {
            continue;
        }
        documents.push(serde_yaml::to_string(&value).context("re-serialising a YAML document")?);
    }
    Ok(documents)
}

/// What a document of a multi-document file is, checked before anything is
/// applied so a typo in the last document does not leave the first applied.
fn describe_document(index: usize, document: &str) -> Result<(String, String)> {
    let number = index + 1;
    let kind = detect_manifest_kind(document)
        .with_context(|| format!("document {number}"))?
        .ok_or_else(|| anyhow::anyhow!("document {number} has no kind:"))?;
    if local_manifest_kind(&kind).is_some() {
        anyhow::bail!(
            "document {number} is kind {kind}, which is applied on its own. Multi-document apply takes controller resources only"
        );
    }
    if !SUPPORTED_KINDS
        .iter()
        .any(|supported| supported.eq_ignore_ascii_case(&kind) || routed_alias(supported, &kind))
    {
        anyhow::bail!(
            "document {number} has unsupported kind '{kind}'. Supported kinds: {}",
            SUPPORTED_KINDS.join(", ")
        );
    }
    let value: serde_yaml::Value = serde_yaml::from_str(document)?;
    let name = value
        .get("metadata")
        .and_then(|metadata| metadata.get("name"))
        .and_then(|name| name.as_str())
        .unwrap_or("")
        .to_string();
    Ok((kind, name))
}

/// `ssh-key`, `ceph_cluster` and the other spellings the single-file path accepts.
fn routed_alias(supported: &str, kind: &str) -> bool {
    let squash = |text: &str| text.to_ascii_lowercase().replace(['-', '_'], "");
    squash(supported) == squash(kind)
        || (squash(supported) == "postgresql" && squash(kind) == "postgres")
}

fn describe_documents(documents: &[String]) -> Result<Vec<(String, String)>> {
    documents
        .iter()
        .enumerate()
        .map(|(index, document)| describe_document(index, document))
        .collect()
}

/// `--dry-run` for a multi-document file: validate every document, apply none.
pub fn print_documents(documents: &[String]) -> Result<()> {
    let described = describe_documents(documents)?;
    for (document, (kind, name)) in documents.iter().zip(&described) {
        println!("--- dry run: {kind} {name} ---");
        print!("{document}");
    }
    println!("--- end ---");
    Ok(())
}

/// True when `file` holds more than one YAML document. A file that does not
/// parse as YAML, such as a Nix configuration, is not multi-document.
pub fn is_multi_document(file: &str) -> bool {
    read_documents(file)
        .map(|documents| documents.len() > 1)
        .unwrap_or(false)
}

pub fn read_documents(file: &str) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    split_documents(&content).with_context(|| format!("parsing manifest YAML in {file}"))
}

/// Apply every document of a multi-document file in order. All documents are
/// checked first. The first failure stops the run and names the document, so
/// the earlier ones are already applied and a re-run is safe because every
/// apply is an upsert.
pub async fn apply_documents(info: &ConnectionInfo, documents: &[String]) -> Result<()> {
    let described = describe_documents(documents)?;
    let directory = std::env::temp_dir().join(format!("kctl-apply-{}", std::process::id()));
    std::fs::create_dir_all(&directory).context("creating a scratch directory")?;
    let outcome = apply_each(info, documents, &described, &directory).await;
    let _ = std::fs::remove_dir_all(&directory);
    outcome
}

async fn apply_each(
    info: &ConnectionInfo,
    documents: &[String],
    described: &[(String, String)],
    directory: &std::path::Path,
) -> Result<()> {
    let total = documents.len();
    for (index, (document, (kind, name))) in documents.iter().zip(described).enumerate() {
        println!("=== [{}/{total}] {kind} {name} ===", index + 1);
        let path = directory.join(format!("{}.yaml", index + 1));
        std::fs::write(&path, document).context("writing a scratch manifest")?;
        let path = path.to_string_lossy().into_owned();
        apply(info, &path, false)
            .await
            .with_context(|| format!("document {} of {total} ({kind} {name})", index + 1))?;
    }
    Ok(())
}

/// Parse `kind:` out of a YAML manifest.
///
/// Returns:
/// - `Ok(Some(kind))` when a non-empty top-level `kind:` is present.
/// - `Ok(None)` when the YAML parses cleanly but has no `kind:` key (or it
///   is empty / not a string).
/// - `Err(_)` when the YAML itself fails to parse — propagated so callers
///   surface the syntax error instead of silently treating the manifest as
///   "kind-less".
pub fn detect_manifest_kind(content: &str) -> Result<Option<String>> {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(content).context("invalid YAML in manifest")?;
    let Some(map) = doc.as_mapping() else {
        return Ok(None);
    };
    let key = serde_yaml::Value::String("kind".to_string());
    Ok(map
        .get(&key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unknown_kind_is_refused_before_any_connection() {
        let path = std::env::temp_dir().join("kctl-test-unknown-kind.yaml");
        std::fs::write(&path, "kind: Bogus\nmetadata:\n  name: x\n").unwrap();
        let info = ConnectionInfo {
            address: "127.0.0.1:1".to_string(),
            addresses: vec![],
            insecure: true,
            tls_server_name: None,
            cert_pem: None,
            key_pem: None,
            ca_pem: None,
            cert: None,
            key: None,
            ca: None,
        };
        let err = apply(&info, path.to_str().unwrap(), false)
            .await
            .expect_err("an unknown kind must not be applied");
        let text = format!("{err:#}");
        assert!(
            text.contains("unsupported manifest kind 'bogus'")
                && text.contains("KubernetesCluster"),
            "{text}"
        );
    }

    #[test]
    fn supported_kinds_name_every_routed_family() {
        for kind in [
            "ObjectUser",
            "KubernetesCluster",
            "ClusterUpdate",
            "SlurmCluster",
            "NodeVgpu",
        ] {
            assert!(SUPPORTED_KINDS.contains(&kind), "{kind}");
        }
    }

    #[test]
    fn split_documents_keeps_order_and_drops_empty_ones() {
        let text = "---\nkind: Network\nmetadata:\n  name: a\n---\n---\nkind: SshKey\nmetadata:\n  name: b\n---\n";
        let documents = split_documents(text).unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(unwrap_kind(&documents[0]).as_deref(), Some("Network"));
        assert_eq!(unwrap_kind(&documents[1]).as_deref(), Some("SshKey"));
    }

    #[test]
    fn a_nix_file_and_a_missing_file_are_not_multi_document() {
        let dir = std::env::temp_dir().join(format!("kctl-multi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let nix = dir.join("host.nix");
        std::fs::write(&nix, "{ pkgs, ... }: {\n  services.foo.enable = true;\n}\n").unwrap();
        assert!(!is_multi_document(nix.to_str().unwrap()));
        assert!(!is_multi_document(
            dir.join("missing.yaml").to_str().unwrap()
        ));
        let two = dir.join("two.yaml");
        std::fs::write(&two, "kind: Network\n---\nkind: SshKey\n").unwrap();
        assert!(is_multi_document(two.to_str().unwrap()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn split_documents_of_a_single_manifest_is_one_document() {
        assert_eq!(
            split_documents("kind: VM\nmetadata:\n  name: a\n")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_document_is_checked_before_anything_is_applied() {
        let ok = describe_document(0, "kind: NodeVgpu\nmetadata:\n  name: n1\n").unwrap();
        assert_eq!(ok, ("NodeVgpu".to_string(), "n1".to_string()));
        assert!(describe_document(0, "kind: ssh-key\nmetadata:\n  name: k\n").is_ok());
        assert!(describe_document(0, "kind: postgres\nmetadata:\n  name: k\n").is_ok());
        let missing = describe_document(1, "metadata:\n  name: x\n").unwrap_err();
        assert!(format!("{missing:#}").contains("document 2 has no kind"));
        let local = describe_document(0, "kind: Cluster\nmetadata:\n  name: x\n").unwrap_err();
        assert!(format!("{local:#}").contains("applied on its own"));
        let unknown = describe_document(2, "kind: Bogus\nmetadata:\n  name: x\n").unwrap_err();
        assert!(format!("{unknown:#}").contains("document 3 has unsupported kind 'Bogus'"));
    }

    #[test]
    fn every_supported_kind_passes_the_document_check() {
        for kind in SUPPORTED_KINDS {
            if local_manifest_kind(kind).is_some() {
                continue;
            }
            let text = format!("kind: {kind}\nmetadata:\n  name: x\n");
            assert!(describe_document(0, &text).is_ok(), "{kind}");
        }
    }

    fn unwrap_kind(content: &str) -> Option<String> {
        detect_manifest_kind(content).expect("YAML should parse")
    }

    #[test]
    fn detect_manifest_kind_reads_top_level_kind() {
        let manifest = r#"
kind: SecurityGroup
metadata:
  name: expose-nginx-host
"#;
        assert_eq!(unwrap_kind(manifest).as_deref(), Some("SecurityGroup"));
    }

    #[test]
    fn detect_manifest_kind_returns_none_without_kind() {
        let manifest = r#"
metadata:
  name: no-kind
"#;
        assert_eq!(unwrap_kind(manifest), None);
    }

    #[test]
    fn detect_manifest_kind_vm() {
        let manifest = "kind: VM\nmetadata:\n  name: test\n";
        assert_eq!(unwrap_kind(manifest).as_deref(), Some("VM"));
    }

    #[test]
    fn detect_manifest_kind_network() {
        let manifest = "kind: Network\nmetadata:\n  name: net1\n";
        assert_eq!(unwrap_kind(manifest).as_deref(), Some("Network"));
    }

    #[test]
    fn detect_manifest_kind_sshkey() {
        let manifest = "kind: SshKey\nmetadata:\n  name: k1\n";
        assert_eq!(unwrap_kind(manifest).as_deref(), Some("SshKey"));
    }

    #[test]
    fn detect_manifest_kind_container() {
        let manifest = "kind: Container\nmetadata:\n  name: c1\n";
        assert_eq!(unwrap_kind(manifest).as_deref(), Some("Container"));
    }

    #[test]
    fn detect_manifest_kind_cluster() {
        let manifest =
            "kind: Cluster\nmetadata:\n  name: prod\nspec:\n  controller: 1.2.3.4:9090\n";
        assert_eq!(unwrap_kind(manifest).as_deref(), Some("Cluster"));
    }

    #[test]
    fn detect_manifest_kind_nodeinstall() {
        let manifest = "kind: NodeInstall\nmetadata:\n  name: node1\nspec:\n  node: 1.2.3.4:9091\n  osDisk: /dev/sda\n";
        assert_eq!(unwrap_kind(manifest).as_deref(), Some("NodeInstall"));
    }

    #[test]
    fn detect_manifest_kind_propagates_yaml_errors() {
        // Unterminated YAML mapping: the parser must reject this and we must
        // surface the error instead of silently returning Ok(None).
        let manifest = "kind: VM\n  bad: : :";
        let err = detect_manifest_kind(manifest).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid YAML in manifest"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn is_local_manifest_kind_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cluster.yaml");
        std::fs::write(
            &path,
            "kind: Cluster\nmetadata:\n  name: test\nspec:\n  controller: 1.2.3.4:9090\n",
        )
        .unwrap();
        assert!(is_local_manifest_kind(path.to_str().unwrap()).unwrap());
    }

    #[test]
    fn is_local_manifest_kind_vm_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.yaml");
        std::fs::write(&path, "kind: VM\nmetadata:\n  name: test\n").unwrap();
        assert!(!is_local_manifest_kind(path.to_str().unwrap()).unwrap());
    }

    #[test]
    fn detect_manifest_kind_reads_cephcluster() {
        assert_eq!(
            unwrap_kind("kind: CephCluster\nmetadata:\n  name: lab\n").as_deref(),
            Some("CephCluster")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ceph.yaml");
        std::fs::write(&path, "kind: CephCluster\nmetadata:\n  name: lab\n").unwrap();
        assert!(
            !is_local_manifest_kind(path.to_str().unwrap()).unwrap(),
            "CephCluster is a controller resource, not a local bootstrap kind"
        );
    }

    #[test]
    fn is_local_manifest_kind_missing_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.yaml");
        let err = is_local_manifest_kind(path.to_str().unwrap()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("reading"), "unexpected error: {msg}");
    }

    #[test]
    fn classify_manifest_marks_cluster_as_local() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cluster.yaml");
        std::fs::write(
            &path,
            "kind: Cluster\nmetadata:\n  name: test\nspec:\n  controller: 1.2.3.4:9090\n",
        )
        .unwrap();
        let classified = classify_manifest(path.to_str().unwrap()).unwrap();
        assert_eq!(classified.kind.as_deref(), Some("Cluster"));
        assert_eq!(classified.local, Some(LocalManifestKind::Cluster));
        assert!(classified.content.contains("Cluster"));
    }

    #[test]
    fn classify_manifest_marks_vm_as_remote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.yaml");
        std::fs::write(&path, "kind: VM\nmetadata:\n  name: test\n").unwrap();
        let classified = classify_manifest(path.to_str().unwrap()).unwrap();
        assert_eq!(classified.kind.as_deref(), Some("VM"));
        assert_eq!(classified.local, None);
    }

    #[test]
    fn classify_manifest_propagates_yaml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.yaml");
        std::fs::write(&path, "kind: VM\n  bad: : :").unwrap();
        let err = classify_manifest(path.to_str().unwrap()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parsing manifest"), "unexpected error: {msg}");
    }
}

/// Property-based tests (Phase 2) — manifest classification.
#[cfg(test)]
mod proptests {
    use super::{detect_manifest_kind, local_manifest_kind, LocalManifestKind};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 2_000,
            .. ProptestConfig::default()
        })]

        /// `local_manifest_kind` is **case-insensitive** on its input
        /// and recognises all three documented `nodeinstall` aliases.
        #[test]
        fn local_manifest_kind_aliases(
            base in prop::sample::select(vec![
                "cluster", "nodeinstall", "node-install", "node_install",
            ]),
            uppercase in any::<bool>(),
        ) {
            let s = if uppercase { base.to_uppercase() } else { base.to_string() };
            let expected = match base {
                "cluster" => Some(LocalManifestKind::Cluster),
                _ => Some(LocalManifestKind::NodeInstall),
            };
            prop_assert_eq!(local_manifest_kind(&s), expected);
        }

        /// Anything outside the alias set returns `None`.
        #[test]
        fn local_manifest_kind_none_for_unknown(s in "[a-zA-Z0-9_-]{1,16}") {
            let lower = s.to_ascii_lowercase();
            if !matches!(lower.as_str(), "cluster" | "nodeinstall" | "node-install" | "node_install") {
                prop_assert_eq!(local_manifest_kind(&s), None);
            }
        }

        /// `detect_manifest_kind` returns `Ok(Some(kind))` when given a
        /// well-formed YAML map with a `kind:` string entry.
        #[test]
        fn detect_manifest_kind_extracts_top_level_key(
            kind in "[a-zA-Z][a-zA-Z0-9]{0,16}",
            name in "[a-z][a-z0-9-]{0,16}",
        ) {
            let yaml = format!("kind: {kind}\nmetadata:\n  name: {name}\n");
            let got = detect_manifest_kind(&yaml).expect("YAML parses");
            prop_assert_eq!(got, Some(kind));
        }

        /// Empty `kind:` is treated as missing (`Ok(None)`).
        #[test]
        fn detect_manifest_kind_empty_kind_is_none(_seed in any::<u8>()) {
            let yaml = "kind: \"\"\nmetadata:\n  name: x\n";
            prop_assert_eq!(detect_manifest_kind(yaml).expect("parses"), None);
        }
    }
}
