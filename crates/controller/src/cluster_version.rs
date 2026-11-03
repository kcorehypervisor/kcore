//! Cluster version updates. The operator names a release tag; the controller
//! resolves it to a commit, checks that its controller can read this
//! database, and rolls every node that runs another version to it, upgrading
//! or downgrading.

use std::time::Duration;

use tokio::process::Command;

use crate::controller_proto::{
    ClusterUpdateActivationMode, ClusterUpdateApprovalPolicy, ClusterUpdateSelector,
    ClusterUpdateSpec, ClusterUpdateStrategy, ClusterUpdateStrategyType, ClusterUpdateTarget,
};

pub const DEFAULT_FLAKE_REF: &str = "github:kcorehypervisor/kcore";
/// Update names start with this; the newest such update is the cluster version.
pub const UPDATE_PREFIX: &str = "version-";
const NIX_TIMEOUT: Duration = Duration::from_secs(600);

/// What a release tag says about itself (`lib.kcore` in its flake).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub flake_rev: String,
    pub schema_version: i32,
    pub min_compatible_reader: i32,
}

type Semver = (u64, u64, u64);

/// `v1.2.3` or `1.2.3` as `v1.2.3`.
pub fn normalize_tag(input: &str) -> Result<String, String> {
    let input = input.trim();
    let bare = input.strip_prefix('v').unwrap_or(input);
    let parts: Vec<&str> = bare.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return Err(format!("{input} is not a release tag like v1.2.3"));
    }
    Ok(format!("v{bare}"))
}

/// The release a node reports in `/etc/kcore/version`. Build suffixes are
/// ignored.
pub fn parse_version(version: &str) -> Option<Semver> {
    let v = version.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    let mut parts = v.splitn(3, '.');
    let mut next = |last: bool| -> Option<u64> {
        let part = parts.next()?;
        let digits: String = if last {
            part.chars().take_while(char::is_ascii_digit).collect()
        } else {
            part.to_string()
        };
        digits.parse().ok()
    };
    Some((next(false)?, next(false)?, next(true)?))
}

pub fn update_name(tag: &str) -> String {
    format!("{UPDATE_PREFIX}{}", tag.replace('.', "-"))
}

/// `flake_ref` at a commit or tag, the same form the node agent builds.
pub fn flake_url_at(flake_ref: &str, at: &str) -> String {
    let flake_ref = flake_ref.trim().trim_end_matches('/');
    if ["github:", "gitlab:", "sourcehut:"]
        .iter()
        .any(|scheme| flake_ref.starts_with(scheme))
    {
        return format!("{flake_ref}/{at}");
    }
    let is_rev = at.len() == 40 && at.chars().all(|c| c.is_ascii_hexdigit());
    let param = if is_rev {
        format!("rev={at}")
    } else {
        format!("ref=refs/tags/{at}")
    };
    let sep = if flake_ref.contains('?') { '&' } else { '?' };
    format!("{flake_ref}{sep}{param}")
}

/// Look the tag up and read its `lib.kcore`. Needs `nix` and network access
/// to `flake_ref` (or a local mirror).
pub async fn resolve_release(flake_ref: &str, tag: &str) -> Result<Release, String> {
    let url = flake_url_at(flake_ref, tag);
    let meta = nix_json(&["flake", "metadata", "--json", "--refresh", &url])
        .await
        .map_err(|e| format!("{tag} was not found in {flake_ref}: {e}"))?;
    let rev = meta
        .pointer("/locked/rev")
        .or_else(|| meta.get("revision"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if rev.len() != 40 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{url} did not resolve to a commit"));
    }
    let pinned = format!("{}#lib.kcore", flake_url_at(flake_ref, &rev));
    let info = nix_json(&["eval", "--json", &pinned])
        .await
        .map_err(|e| format!("{tag} has no lib.kcore, so it predates tag updates: {e}"))?;
    parse_release(tag, &rev, &info)
}

pub fn parse_release(tag: &str, rev: &str, info: &serde_json::Value) -> Result<Release, String> {
    let text = |key: &str| info.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let int = |key: &str| {
        info.get(key)
            .and_then(|v| v.as_i64())
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0)
    };
    let version = text("version").to_string();
    if format!("v{version}") != tag {
        return Err(format!(
            "{tag} carries VERSION {version}; the tag and VERSION must match"
        ));
    }
    let schema_version = int("schemaVersion");
    if schema_version <= 0 {
        return Err(format!("{tag} does not report its schemaVersion"));
    }
    Ok(Release {
        tag: tag.to_string(),
        version,
        flake_rev: rev.to_string(),
        schema_version,
        min_compatible_reader: int("minCompatibleSchemaReader"),
    })
}

/// A downgrade must land on a controller that can read this database.
pub fn check_schema(release: &Release, min_reader: i32) -> Result<(), String> {
    if release.schema_version < min_reader {
        return Err(format!(
            "{} reads schema {}, but this database needs a reader of schema {min_reader} or later",
            release.tag, release.schema_version
        ));
    }
    Ok(())
}

/// Nodes not on `target` and the direction they move in. A node whose
/// version is unknown is updated and counts as an upgrade.
pub fn plan(target: &str, nodes: &[(String, String)]) -> (&'static str, Vec<String>) {
    let want = parse_version(target);
    let (mut up, mut down) = (false, false);
    let mut ids = Vec::new();
    for (id, version) in nodes {
        let have = parse_version(version);
        if have.is_some() && have == want {
            continue;
        }
        if have > want {
            down = true;
        } else {
            up = true;
        }
        ids.push(id.clone());
    }
    ids.sort();
    let direction = match (up, down) {
        (false, false) => "unchanged",
        (true, true) => "mixed",
        (false, true) => "downgrade",
        (true, false) => "upgrade",
    };
    (direction, ids)
}

/// One node at a time, approved, and rebooted by the controller when the
/// node staged a new kernel.
pub fn update_spec(
    name: &str,
    flake_ref: &str,
    release: &Release,
    node_ids: Vec<String>,
    drain_vms: bool,
) -> ClusterUpdateSpec {
    ClusterUpdateSpec {
        name: name.to_string(),
        target: Some(ClusterUpdateTarget {
            version: release.version.clone(),
            flake_ref: flake_ref.to_string(),
            flake_rev: release.flake_rev.clone(),
            tag: release.tag.clone(),
            ..Default::default()
        }),
        selector: Some(ClusterUpdateSelector {
            node_ids,
            ..Default::default()
        }),
        strategy: Some(ClusterUpdateStrategy {
            r#type: ClusterUpdateStrategyType::ClusterUpdateStrategyOneAtATime as i32,
            max_unavailable: 1,
            batch_size: 0,
        }),
        drain_vms,
        activation_mode: ClusterUpdateActivationMode::ClusterUpdateActivationAuto as i32,
        reboot_policy: "auto".into(),
        approval_policy: ClusterUpdateApprovalPolicy::ClusterUpdateApprovalAuto as i32,
        automatic_rollback: true,
        update_kind: "kcore".into(),
        schema_version: release.schema_version,
        previous_schema_version: crate::db::SCHEMA_VERSION,
        min_compatible_reader: release.min_compatible_reader,
        ..Default::default()
    }
}

async fn nix_json(args: &[&str]) -> Result<serde_json::Value, String> {
    let mut cmd = Command::new("nix");
    cmd.args(["--extra-experimental-features", "nix-command flakes"])
        .args(args)
        .kill_on_drop(true);
    // systemd starts the controller without HOME; nix keeps its fetcher
    // cache there.
    if std::env::var_os("HOME").is_none() {
        cmd.env("HOME", "/var/lib/kcore");
    }
    let out = tokio::time::timeout(NIX_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("nix {} timed out", args.join(" ")))?
        .map_err(|e| format!("nix: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err
            .trim()
            .lines()
            .last()
            .unwrap_or("nix failed")
            .to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("nix output: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_normalized_to_a_leading_v() {
        assert_eq!(normalize_tag("0.6.2").unwrap(), "v0.6.2");
        assert_eq!(normalize_tag(" v1.10.0 ").unwrap(), "v1.10.0");
        assert!(normalize_tag("v1.2").is_err());
        assert!(normalize_tag("v1.2.3-rc1").is_err());
        assert!(normalize_tag("main").is_err());
    }

    #[test]
    fn node_versions_compare_numerically() {
        assert_eq!(parse_version("0.6.1"), Some((0, 6, 1)));
        assert_eq!(parse_version("v0.10.0-dirty"), Some((0, 10, 0)));
        assert_eq!(parse_version(""), None);
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }

    #[test]
    fn plan_reports_direction_and_skips_nodes_on_target() {
        let nodes = |vs: &[(&str, &str)]| -> Vec<(String, String)> {
            vs.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        let (dir, ids) = plan("0.7.0", &nodes(&[("b", "0.6.1"), ("a", "0.7.0")]));
        assert_eq!((dir, ids), ("upgrade", vec!["b".to_string()]));
        let (dir, ids) = plan("0.6.0", &nodes(&[("a", "0.6.1"), ("b", "0.6.1")]));
        assert_eq!(dir, "downgrade");
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        let (dir, _) = plan("0.6.1", &nodes(&[("a", "0.6.0"), ("b", "0.7.0")]));
        assert_eq!(dir, "mixed");
        let (dir, ids) = plan("0.6.1", &nodes(&[("a", "0.6.1")]));
        assert_eq!((dir, ids.len()), ("unchanged", 0));
        let (dir, ids) = plan("0.6.1", &nodes(&[("a", "")]));
        assert_eq!((dir, ids), ("upgrade", vec!["a".to_string()]));
    }

    #[test]
    fn release_must_match_its_tag_and_report_a_schema() {
        let rev = "a".repeat(40);
        let info = serde_json::json!({
            "version": "0.7.0",
            "tag": "v0.7.0",
            "schemaVersion": 72,
            "minCompatibleSchemaReader": 56,
        });
        let release = parse_release("v0.7.0", &rev, &info).unwrap();
        assert_eq!(release.schema_version, 72);
        assert_eq!(release.min_compatible_reader, 56);
        assert!(parse_release("v0.7.1", &rev, &info).is_err());
        let no_schema = serde_json::json!({ "version": "0.7.0" });
        assert!(parse_release("v0.7.0", &rev, &no_schema).is_err());
    }

    #[test]
    fn downgrade_needs_a_controller_that_reads_this_schema() {
        let release = Release {
            tag: "v0.5.0".into(),
            version: "0.5.0".into(),
            flake_rev: "a".repeat(40),
            schema_version: 55,
            min_compatible_reader: 40,
        };
        assert!(check_schema(&release, 56).is_err());
        assert!(check_schema(&release, 55).is_ok());
    }

    #[test]
    fn flake_urls_match_the_node_agent() {
        assert_eq!(
            flake_url_at(DEFAULT_FLAKE_REF, "v0.7.0"),
            "github:kcorehypervisor/kcore/v0.7.0"
        );
        assert_eq!(
            flake_url_at("git+https://git.example/kcore", "v0.7.0"),
            "git+https://git.example/kcore?ref=refs/tags/v0.7.0"
        );
        let rev = "b".repeat(40);
        assert_eq!(
            flake_url_at("git+file:///srv/kcore?shallow=1", &rev),
            format!("git+file:///srv/kcore?shallow=1&rev={rev}")
        );
    }
}
