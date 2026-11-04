//! `kctl update` — cluster rollouts (see `docs/cluster-updates.md`).

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterUpdateManifest {
    kind: String,
    metadata: Metadata,
    spec: Spec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Spec {
    target: Target,
    #[serde(default)]
    selector: Selector,
    #[serde(default)]
    strategy: Strategy,
    #[serde(default, alias = "drain_vms")]
    drain_vms: bool,
    #[serde(default, alias = "drain_timeout_seconds")]
    drain_timeout_seconds: i32,
    #[serde(default)]
    activation: Activation,
    #[serde(default, alias = "approval_policy")]
    approval_policy: String,
    #[serde(default, alias = "automatic_rollback")]
    automatic_rollback: bool,
    #[serde(default, alias = "update_kind")]
    update_kind: String,
    #[serde(default, alias = "requires_nixpkgs")]
    requires_nixpkgs: bool,
    #[serde(default, alias = "tested_os_pins")]
    tested_os_pins: Vec<TestedOsPin>,
    #[serde(default, alias = "schema_version")]
    schema_version: i32,
    #[serde(default, alias = "previous_schema_version")]
    previous_schema_version: i32,
    #[serde(default, alias = "min_compatible_reader")]
    min_compatible_reader: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    version: String,
    #[serde(alias = "flake_ref")]
    flake_ref: String,
    #[serde(alias = "flake_rev")]
    flake_rev: String,
    #[serde(default, alias = "nixpkgs_rev")]
    nixpkgs_rev: String,
    #[serde(default, alias = "system_profile")]
    system_profile: String,
    #[serde(default)]
    tag: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Selector {
    #[serde(default, alias = "node_ids")]
    node_ids: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    datacenters: Vec<String>,
    #[serde(default, alias = "all_nodes")]
    all_nodes: bool,
    #[serde(default, alias = "controllers_only")]
    controllers_only: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Strategy {
    #[serde(default, alias = "strategy_type")]
    strategy_type: String,
    #[serde(default, alias = "max_unavailable")]
    max_unavailable: i32,
    #[serde(default, alias = "batch_size")]
    batch_size: i32,
    #[serde(default, alias = "canary_soak_seconds")]
    canary_soak_seconds: i32,
    #[serde(default, alias = "canary_node", alias = "canary_node_id")]
    canary_node: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestedOsPin {
    #[serde(default, alias = "nixpkgs_rev")]
    nixpkgs_rev: String,
    #[serde(default, alias = "ceph_rev")]
    ceph_rev: String,
    #[serde(default, alias = "ceph_sha256")]
    ceph_sha256: String,
    #[serde(default, alias = "kubernetes_version")]
    kubernetes_version: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Activation {
    #[serde(default)]
    mode: String,
    #[serde(default, alias = "reboot_policy")]
    reboot_policy: String,
}

fn parse_manifest_yaml(raw: &str) -> Result<ClusterUpdateManifest> {
    let m: ClusterUpdateManifest =
        serde_yaml::from_str(raw).context("parsing ClusterUpdate YAML")?;
    if m.kind != "ClusterUpdate" {
        bail!("manifest kind must be ClusterUpdate, got {}", m.kind);
    }
    Ok(m)
}

fn parse_manifest(path: &str) -> Result<ClusterUpdateManifest> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    parse_manifest_yaml(&raw).with_context(|| format!("parsing YAML {path}"))
}

fn approval_policy_from_str(s: &str) -> i32 {
    match s.trim().to_ascii_lowercase().as_str() {
        "manual" => {
            controller_proto::ClusterUpdateApprovalPolicy::ClusterUpdateApprovalManual as i32
        }
        "auto-non-disruptive" | "auto_non_disruptive" => {
            controller_proto::ClusterUpdateApprovalPolicy::ClusterUpdateApprovalAutoNonDisruptive
                as i32
        }
        "auto" => controller_proto::ClusterUpdateApprovalPolicy::ClusterUpdateApprovalAuto as i32,
        _ => controller_proto::ClusterUpdateApprovalPolicy::ClusterUpdateApprovalUnspecified as i32,
    }
}

fn strategy_type_from_str(s: &str) -> i32 {
    match s.trim().to_ascii_lowercase().as_str() {
        "canary" => controller_proto::ClusterUpdateStrategyType::ClusterUpdateStrategyCanary as i32,
        "one-at-a-time" | "one_at_a_time" => {
            controller_proto::ClusterUpdateStrategyType::ClusterUpdateStrategyOneAtATime as i32
        }
        "batch" => controller_proto::ClusterUpdateStrategyType::ClusterUpdateStrategyBatch as i32,
        "per-dc" | "per_dc" => {
            controller_proto::ClusterUpdateStrategyType::ClusterUpdateStrategyPerDc as i32
        }
        _ => controller_proto::ClusterUpdateStrategyType::ClusterUpdateStrategyUnspecified as i32,
    }
}

fn activation_mode_from_str(s: &str) -> i32 {
    match s.trim().to_ascii_lowercase().as_str() {
        "test" => controller_proto::ClusterUpdateActivationMode::ClusterUpdateActivationTest as i32,
        "switch" => {
            controller_proto::ClusterUpdateActivationMode::ClusterUpdateActivationSwitch as i32
        }
        "boot" => controller_proto::ClusterUpdateActivationMode::ClusterUpdateActivationBoot as i32,
        "auto" => controller_proto::ClusterUpdateActivationMode::ClusterUpdateActivationAuto as i32,
        _ => {
            controller_proto::ClusterUpdateActivationMode::ClusterUpdateActivationUnspecified as i32
        }
    }
}

fn to_proto(m: ClusterUpdateManifest) -> controller_proto::ClusterUpdateSpec {
    controller_proto::ClusterUpdateSpec {
        name: m.metadata.name,
        target: Some(controller_proto::ClusterUpdateTarget {
            version: m.spec.target.version,
            flake_ref: m.spec.target.flake_ref,
            flake_rev: m.spec.target.flake_rev,
            nixpkgs_rev: m.spec.target.nixpkgs_rev,
            system_profile: m.spec.target.system_profile,
            tag: m.spec.target.tag,
        }),
        selector: Some(controller_proto::ClusterUpdateSelector {
            node_ids: m.spec.selector.node_ids,
            labels: m.spec.selector.labels,
            datacenters: m.spec.selector.datacenters,
            all_nodes: m.spec.selector.all_nodes,
            controllers_only: m.spec.selector.controllers_only,
        }),
        strategy: Some(controller_proto::ClusterUpdateStrategy {
            r#type: strategy_type_from_str(&m.spec.strategy.strategy_type),
            max_unavailable: m.spec.strategy.max_unavailable,
            batch_size: m.spec.strategy.batch_size,
            canary_soak_seconds: m.spec.strategy.canary_soak_seconds,
            canary_node_id: m.spec.strategy.canary_node,
        }),
        drain_vms: m.spec.drain_vms,
        drain_timeout_seconds: m.spec.drain_timeout_seconds,
        activation_mode: activation_mode_from_str(&m.spec.activation.mode),
        reboot_policy: m.spec.activation.reboot_policy,
        approval_policy: approval_policy_from_str(&m.spec.approval_policy),
        automatic_rollback: m.spec.automatic_rollback,
        update_kind: m.spec.update_kind.clone(),
        requires_nixpkgs: m.spec.requires_nixpkgs,
        tested_os_pins: m
            .spec
            .tested_os_pins
            .into_iter()
            .map(|pin| controller_proto::TestedOsPin {
                nixpkgs_rev: pin.nixpkgs_rev,
                ceph_rev: pin.ceph_rev,
                ceph_sha256: pin.ceph_sha256,
                kubernetes_version: pin.kubernetes_version,
            })
            .collect(),
        schema_version: m.spec.schema_version,
        previous_schema_version: m.spec.previous_schema_version,
        min_compatible_reader: m.spec.min_compatible_reader,
    }
}

pub async fn plan(info: &ConnectionInfo, file: &str) -> Result<()> {
    let m = parse_manifest(file)?;
    let spec = to_proto(m);
    let mut client = client::controller_client(info).await?;
    let resp = client
        .plan_cluster_update(controller_proto::PlanClusterUpdateRequest { spec: Some(spec) })
        .await
        .context("plan_cluster_update rpc")?
        .into_inner();
    println!("Viable: {}", resp.viable);
    if !resp.detail.is_empty() {
        println!("Detail: {}", resp.detail);
    }
    println!("Likely requires reboot: {}", resp.likely_requires_reboot);
    println!("Target nodes:");
    for id in &resp.target_node_ids {
        println!("  - {id}");
    }
    for issue in &resp.issues {
        println!("  ! {}  {}", issue.node_id, issue.reason);
    }
    Ok(())
}

pub async fn apply(info: &ConnectionInfo, file: &str) -> Result<()> {
    let m = parse_manifest(file)?;
    let spec = to_proto(m);
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_cluster_update(controller_proto::CreateClusterUpdateRequest { spec: Some(spec) })
        .await
        .context("create_cluster_update rpc")?
        .into_inner();
    println!("action={} success={}", resp.action, resp.success);
    if let Some(u) = resp.cluster_update {
        if let Some(s) = u.spec {
            println!("cluster update {}", s.name);
        }
    }
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_cluster_update(controller_proto::GetClusterUpdateRequest { name: name.into() })
        .await
        .context("get_cluster_update rpc")?
        .into_inner();
    let u = resp.cluster_update.context("empty cluster_update")?;
    let spec = u.spec.context("empty spec")?;
    println!("name: {}", spec.name);
    println!("generation: {}", u.generation);
    println!(
        "phase: {}",
        controller_proto::ClusterUpdatePhase::try_from(u.phase)
            .map(|p| p.as_str_name())
            .unwrap_or("UNKNOWN")
    );
    println!(
        "approval: {}",
        controller_proto::ClusterUpdateApprovalStatus::try_from(u.approval_status)
            .map(|p| p.as_str_name())
            .unwrap_or("UNKNOWN")
    );
    for n in &resp.node_statuses {
        let phase = controller_proto::NodeUpdatePhase::try_from(n.phase)
            .map(|p| p.as_str_name())
            .unwrap_or("UNKNOWN");
        println!(
            "  node {}  phase={phase}  gen={}  err={}",
            n.node_id, n.observed_generation, n.last_error
        );
    }
    Ok(())
}

pub async fn list_cmd(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_cluster_updates(controller_proto::ListClusterUpdatesRequest {})
        .await
        .context("list_cluster_updates rpc")?
        .into_inner();
    for u in resp.cluster_updates {
        let name = u.spec.as_ref().map(|s| s.name.as_str()).unwrap_or("?");
        println!("{name}\tphase={}\tgen={}", u.phase, u.generation);
    }
    Ok(())
}

pub async fn approve(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    client
        .approve_cluster_update(controller_proto::ApproveClusterUpdateRequest { name: name.into() })
        .await
        .context("approve_cluster_update rpc")?;
    println!("approved {name}");
    Ok(())
}

pub async fn cancel(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    client
        .cancel_cluster_update(controller_proto::CancelClusterUpdateRequest { name: name.into() })
        .await
        .context("cancel_cluster_update rpc")?;
    println!("cancelled {name}");
    Ok(())
}

pub async fn rollback(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    client
        .rollback_cluster_update(controller_proto::RollbackClusterUpdateRequest {
            name: name.into(),
        })
        .await
        .context("rollback_cluster_update rpc")?;
    println!("rollback requested for {name}");
    Ok(())
}

/// How a canary is chosen and how long it soaks.
pub struct CanaryArgs<'a> {
    pub soak: Option<&'a str>,
    pub node: Option<&'a str>,
    pub disabled: bool,
}

/// `90`, `90s`, `45m`, `2h`, or `1d`, in seconds.
pub fn parse_soak(text: &str) -> Result<i32> {
    let text = text.trim();
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, "s"),
    };
    let scale: i64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("soak {text:?}: use a number with s, m, h, or d, for example 2h"),
    };
    let value: i64 = digits
        .parse()
        .with_context(|| format!("soak {text:?}: not a duration"))?;
    let secs = value
        .checked_mul(scale)
        .filter(|secs| (1..=i64::from(i32::MAX)).contains(secs))
        .with_context(|| format!("soak {text:?} is out of range"))?;
    Ok(i32::try_from(secs)?)
}

pub async fn set_version(
    info: &ConnectionInfo,
    version: &str,
    flake_ref: &str,
    drain_vms: bool,
    dry_run: bool,
    canary: CanaryArgs<'_>,
) -> Result<()> {
    if canary.disabled && (canary.soak.is_some() || canary.node.is_some()) {
        bail!("--no-canary cannot be combined with --soak or --canary");
    }
    let canary_soak_seconds = canary.soak.map(parse_soak).transpose()?.unwrap_or(0);
    let mut client = client::controller_client(info).await?;
    let resp = client
        .set_cluster_version(controller_proto::SetClusterVersionRequest {
            version: version.into(),
            flake_ref: flake_ref.into(),
            drain_vms,
            dry_run,
            canary_soak_seconds,
            no_canary: canary.disabled,
            canary_node_id: canary.node.unwrap_or("").into(),
        })
        .await
        .context("set_cluster_version rpc")?
        .into_inner();
    println!("{}", resp.message);
    if !resp.direction.is_empty() {
        println!("direction: {}", resp.direction);
    }
    println!("version: {}  commit: {}", resp.version, resp.flake_rev);
    for id in &resp.target_node_ids {
        println!("  - {id}");
    }
    if !resp.canary_node_id.is_empty() {
        println!(
            "canary: {}  soak: {}s",
            resp.canary_node_id, resp.canary_soak_seconds
        );
    }
    if !resp.update_name.is_empty() {
        println!(
            "follow it with: kctl update cluster get {}",
            resp.update_name
        );
    }
    Ok(())
}

pub async fn get_version(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_cluster_version(controller_proto::GetClusterVersionRequest {})
        .await
        .context("get_cluster_version rpc")?
        .into_inner();
    if resp.version.is_empty() {
        println!("version: (never set)");
    } else {
        let phase = controller_proto::ClusterUpdatePhase::try_from(resp.phase)
            .map(|p| p.as_str_name())
            .unwrap_or("UNKNOWN");
        println!("version: {}  commit: {}", resp.version, resp.flake_rev);
        println!("update: {}  phase: {phase}", resp.update_name);
    }
    for n in &resp.nodes {
        let phase = controller_proto::NodeUpdatePhase::try_from(n.phase)
            .map(|p| p.as_str_name())
            .unwrap_or("UNKNOWN");
        print!("  {}  {}  {phase}", n.node_id, n.version);
        if !n.last_error.is_empty() {
            print!("  {}", n.last_error);
        }
        println!();
    }
    Ok(())
}

pub async fn promote(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .promote_cluster_update(controller_proto::PromoteClusterUpdateRequest { name: name.into() })
        .await
        .context("promote_cluster_update rpc")?
        .into_inner();
    println!("{}", resp.message);
    Ok(())
}

pub async fn reboot(info: &ConnectionInfo, name: &str, node: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .reboot_cluster_update_node(controller_proto::RebootClusterUpdateNodeRequest {
            name: name.into(),
            node_id: node.into(),
        })
        .await
        .context("reboot_cluster_update_node rpc")?
        .into_inner();
    if !resp.success {
        bail!("reboot refused: {}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_manifest_yaml, parse_soak};

    #[test]
    fn soak_durations_parse_to_seconds() {
        assert_eq!(parse_soak("90").unwrap(), 90);
        assert_eq!(parse_soak("90s").unwrap(), 90);
        assert_eq!(parse_soak("45m").unwrap(), 2700);
        assert_eq!(parse_soak("2h").unwrap(), 7200);
        assert_eq!(parse_soak("1d").unwrap(), 86_400);
        assert!(parse_soak("0").is_err());
        assert!(parse_soak("2w").is_err());
        assert!(parse_soak("h").is_err());
        assert!(parse_soak("99999999d").is_err());
    }

    #[test]
    fn canary_strategy_parses() {
        let m = parse_manifest_yaml(
            r#"
kind: ClusterUpdate
metadata:
  name: canary
spec:
  target:
    version: "0.7.0"
    flakeRef: github:kcorehypervisor/kcore
    flakeRev: "0123456789abcdef0123456789abcdef01234567"
  selector:
    allNodes: true
  strategy:
    strategyType: canary
    maxUnavailable: 1
    canarySoakSeconds: 1800
    canaryNode: node-b
"#,
        )
        .unwrap();
        assert_eq!(m.spec.strategy.strategy_type, "canary");
        assert_eq!(m.spec.strategy.canary_soak_seconds, 1800);
        assert_eq!(m.spec.strategy.canary_node, "node-b");
    }

    const CAMEL: &str = r#"
kind: ClusterUpdate
metadata:
  name: release-0-4-2
spec:
  target:
    version: "0.4.2"
    flakeRef: github:kcorehypervisor/kcore
    flakeRev: "0123456789abcdef0123456789abcdef01234567"
    nixpkgsRev: "abcdefabcdefabcdefabcdefabcdefabcdefabcd"
  selector:
    allNodes: true
  strategy:
    strategyType: one-at-a-time
    maxUnavailable: 1
    batchSize: 1
  drainVms: false
  activation:
    mode: auto
  approvalPolicy: manual
  updateKind: kcore
  schemaVersion: 52
  previousSchemaVersion: 51
  minCompatibleReader: 51
  testedOsPins:
    - nixpkgsRev: "abcdefabcdefabcdefabcdefabcdefabcdefabcd"
      cephRev: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
      cephSha256: "sha"
      kubernetesVersion: "1.32.1"
"#;

    #[test]
    fn release_yaml_parses_camel_case() {
        let m = parse_manifest_yaml(CAMEL).unwrap();
        assert_eq!(m.metadata.name, "release-0-4-2");
        assert_eq!(m.spec.target.flake_ref, "github:kcorehypervisor/kcore");
        assert_eq!(m.spec.target.flake_rev.len(), 40);
        assert!(m.spec.selector.all_nodes);
        assert_eq!(m.spec.strategy.strategy_type, "one-at-a-time");
        assert_eq!(m.spec.strategy.max_unavailable, 1);
        assert_eq!(m.spec.update_kind, "kcore");
        assert_eq!(m.spec.schema_version, 52);
        assert_eq!(m.spec.tested_os_pins.len(), 1);
        assert_eq!(m.spec.activation.mode, "auto");
    }

    #[test]
    fn release_yaml_parses_snake_case_aliases() {
        let snake = CAMEL
            .replace("flakeRef", "flake_ref")
            .replace("flakeRev", "flake_rev")
            .replace("nixpkgsRev", "nixpkgs_rev")
            .replace("allNodes", "all_nodes")
            .replace("strategyType", "strategy_type")
            .replace("maxUnavailable", "max_unavailable")
            .replace("batchSize", "batch_size")
            .replace("drainVms", "drain_vms")
            .replace("approvalPolicy", "approval_policy")
            .replace("updateKind", "update_kind")
            .replace("schemaVersion", "schema_version")
            .replace("previousSchemaVersion", "previous_schema_version")
            .replace("minCompatibleReader", "min_compatible_reader")
            .replace("testedOsPins", "tested_os_pins")
            .replace("cephRev", "ceph_rev")
            .replace("cephSha256", "ceph_sha256")
            .replace("kubernetesVersion", "kubernetes_version");
        let m = parse_manifest_yaml(&snake).unwrap();
        assert_eq!(
            m.spec.target.flake_rev,
            "0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(m.spec.tested_os_pins[0].kubernetes_version, "1.32.1");
        assert_eq!(m.spec.min_compatible_reader, 51);
    }
}

#[cfg(test)]
mod shipped_example_tests {
    use super::*;

    #[test]
    fn shipped_example_parses() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/cluster-update/release.yaml"
        );
        let manifest = parse_manifest(path).unwrap();
        assert_eq!(manifest.spec.target.version, "0.3.0");
        assert_eq!(manifest.spec.strategy.strategy_type, "one-at-a-time");
    }
}
