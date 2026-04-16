use crate::client::{self, controller_proto};
use crate::commands::{container, network, security_group, ssh_key, vm};
use crate::config::ConnectionInfo;
use anyhow::{Context, Result};

/// Returns `Ok(true)` if the manifest at `file` has a `kind` that is handled
/// locally by `kctl` (i.e. without contacting the controller). I/O errors and
/// YAML parse errors are propagated rather than collapsed to `false`, so
/// callers can surface real manifest problems instead of silently falling
/// back to the controller path.
pub fn is_local_manifest_kind(file: &str) -> Result<bool> {
    let content = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let Some(kind) = detect_manifest_kind(&content) else {
        return Ok(false);
    };
    Ok(matches!(
        kind.to_ascii_lowercase().as_str(),
        "cluster" | "nodeinstall" | "node-install" | "node_install"
    ))
}

pub async fn apply(info: &ConnectionInfo, file: &str, dry_run: bool) -> Result<()> {
    let content = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;

    if dry_run {
        println!("--- dry run ---");
        print!("{content}");
        println!("--- end ---");
        return Ok(());
    }

    if let Some(kind) = detect_manifest_kind(&content) {
        match kind.to_ascii_lowercase().as_str() {
            "securitygroup" => return security_group::apply_from_file(info, file).await,
            "vm" => return vm::create_from_manifest(info, file).await,
            "network" => return network::create_from_manifest(info, file).await,
            "sshkey" | "ssh-key" | "ssh_key" => {
                return ssh_key::create_from_manifest(info, file).await
            }
            "container" => return container::create_from_manifest(info, file).await,
            _ => {}
        }
    }

    let mut client = client::controller_admin_client(info).await?;
    let resp = client
        .apply_nix_config(controller_proto::ApplyNixConfigRequest {
            configuration_nix: content,
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

pub fn detect_manifest_kind(content: &str) -> Option<String> {
    let doc = serde_yaml::from_str::<serde_yaml::Value>(content).ok()?;
    let map = doc.as_mapping()?;
    let key = serde_yaml::Value::String("kind".to_string());
    map.get(&key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_manifest_kind_reads_top_level_kind() {
        let manifest = r#"
kind: SecurityGroup
metadata:
  name: expose-nginx-host
"#;
        assert_eq!(
            detect_manifest_kind(manifest).as_deref(),
            Some("SecurityGroup")
        );
    }

    #[test]
    fn detect_manifest_kind_returns_none_without_kind() {
        let manifest = r#"
metadata:
  name: no-kind
"#;
        assert_eq!(detect_manifest_kind(manifest), None);
    }

    #[test]
    fn detect_manifest_kind_vm() {
        let manifest = "kind: VM\nmetadata:\n  name: test\n";
        assert_eq!(detect_manifest_kind(manifest).as_deref(), Some("VM"));
    }

    #[test]
    fn detect_manifest_kind_network() {
        let manifest = "kind: Network\nmetadata:\n  name: net1\n";
        assert_eq!(detect_manifest_kind(manifest).as_deref(), Some("Network"));
    }

    #[test]
    fn detect_manifest_kind_sshkey() {
        let manifest = "kind: SshKey\nmetadata:\n  name: k1\n";
        assert_eq!(detect_manifest_kind(manifest).as_deref(), Some("SshKey"));
    }

    #[test]
    fn detect_manifest_kind_container() {
        let manifest = "kind: Container\nmetadata:\n  name: c1\n";
        assert_eq!(detect_manifest_kind(manifest).as_deref(), Some("Container"));
    }

    #[test]
    fn detect_manifest_kind_cluster() {
        let manifest =
            "kind: Cluster\nmetadata:\n  name: prod\nspec:\n  controller: 1.2.3.4:9090\n";
        assert_eq!(detect_manifest_kind(manifest).as_deref(), Some("Cluster"));
    }

    #[test]
    fn detect_manifest_kind_nodeinstall() {
        let manifest = "kind: NodeInstall\nmetadata:\n  name: node1\nspec:\n  node: 1.2.3.4:9091\n  osDisk: /dev/sda\n";
        assert_eq!(
            detect_manifest_kind(manifest).as_deref(),
            Some("NodeInstall")
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
    fn is_local_manifest_kind_missing_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.yaml");
        let err = is_local_manifest_kind(path.to_str().unwrap()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("reading"), "unexpected error: {msg}");
    }
}
