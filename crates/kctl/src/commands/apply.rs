use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;
use crate::commands::security_group;
use anyhow::{Context, Result};

pub async fn apply(info: &ConnectionInfo, file: &str, dry_run: bool) -> Result<()> {
    let content = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;

    if dry_run {
        println!("--- dry run ---");
        print!("{content}");
        println!("--- end ---");
        return Ok(());
    }

    if let Some(kind) = detect_manifest_kind(&content) {
        if kind.eq_ignore_ascii_case("SecurityGroup") {
            return security_group::apply_from_file(info, file).await;
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

fn detect_manifest_kind(content: &str) -> Option<String> {
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
    use super::detect_manifest_kind;

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
}
