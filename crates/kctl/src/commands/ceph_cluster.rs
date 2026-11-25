//! `kctl` subcommands for the [`CephCluster`] controller resource (kcore SAN).

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::apply_summary::render_apply_summary;
use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CephClusterManifest {
    kind: String,
    metadata: ManifestMetadata,
    spec: ManifestSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestMetadata {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestSpec {
    #[serde(default)]
    fsid: String,
    #[serde(default)]
    public_network: String,
    #[serde(default)]
    cluster_network: String,
    #[serde(default)]
    replication: ManifestReplication,
    #[serde(default)]
    size: Option<i32>,
    #[serde(default)]
    min_size: Option<i32>,
    #[serde(default)]
    force_wipe: bool,
    #[serde(default = "default_encrypt_osds")]
    encrypt_osds: bool,
    #[serde(default)]
    nodes: Vec<ManifestNode>,
    #[serde(default)]
    pools: Vec<ManifestPool>,
    #[serde(default)]
    recovery: Option<ManifestRecovery>,
    #[serde(default)]
    ms_mode: String,
    #[serde(default)]
    image_features: String,
    #[serde(default)]
    cache_policy: String,
    #[serde(default)]
    lost_members: Vec<String>,
    #[serde(default)]
    external: Option<ManifestExternal>,
    #[serde(default)]
    failure_domain: String,
    #[serde(default)]
    stretch: bool,
    #[serde(default)]
    mirroring: Option<ManifestMirroring>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestMirroring {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    peer_cluster: String,
}

fn default_encrypt_osds() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestReplication {
    #[serde(default = "default_size")]
    size: i32,
    #[serde(default = "default_min_size")]
    min_size: i32,
}

impl Default for ManifestReplication {
    fn default() -> Self {
        Self {
            size: default_size(),
            min_size: default_min_size(),
        }
    }
}

fn default_size() -> i32 {
    3
}
fn default_min_size() -> i32 {
    2
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestNode {
    node_id: String,
    #[serde(default)]
    mon_addr: String,
    #[serde(default)]
    cluster_addr: String,
    #[serde(default)]
    public_iface: String,
    #[serde(default)]
    cluster_iface: String,
    #[serde(default)]
    osd_device: String,
    #[serde(default)]
    osd_devices: Vec<String>,
    #[serde(default)]
    role: String,
    #[serde(default)]
    weight: f64,
    #[serde(default)]
    device_class: String,
    #[serde(default)]
    rack: String,
    #[serde(default)]
    room: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestPool {
    name: String,
    #[serde(default)]
    application: String,
    #[serde(default)]
    size: i32,
    #[serde(default)]
    min_size: i32,
    #[serde(default)]
    pg_num: i32,
    #[serde(default)]
    crush_rule: String,
    #[serde(default)]
    compression_mode: String,
    #[serde(default)]
    quota_bytes: i64,
    #[serde(default)]
    pool_type: String,
    #[serde(default)]
    erasure_profile: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestRecovery {
    #[serde(default)]
    max_backfills: i32,
    #[serde(default)]
    recovery_max_active: i32,
    #[serde(default = "scrub_unset")]
    scrub_begin_hour: i32,
    #[serde(default = "scrub_unset")]
    scrub_end_hour: i32,
    #[serde(default)]
    nearfull_ratio_bp: i32,
}

fn scrub_unset() -> i32 {
    -1
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestExternal {
    mon_hosts: String,
    pool: String,
    #[serde(default)]
    client_keyring: String,
    #[serde(default)]
    client_name: String,
}

fn parse_manifest(file: &str) -> Result<CephClusterManifest> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest: CephClusterManifest =
        serde_yaml::from_str(&data).with_context(|| format!("parsing YAML in {file}"))?;
    let kind = manifest.kind.trim().to_ascii_lowercase();
    if kind != "cephcluster" && kind != "ceph-cluster" && kind != "ceph_cluster" {
        bail!("expected kind: CephCluster, got {}", manifest.kind);
    }
    Ok(manifest)
}

fn to_proto_spec(spec: &ManifestSpec) -> controller_proto::CephClusterSpec {
    let size = spec.size.unwrap_or(spec.replication.size);
    let min_size = spec.min_size.unwrap_or(spec.replication.min_size);
    controller_proto::CephClusterSpec {
        fsid: spec.fsid.clone(),
        public_network: spec.public_network.clone(),
        cluster_network: spec.cluster_network.clone(),
        size,
        min_size,
        force_wipe: spec.force_wipe,
        encrypt_osds: spec.encrypt_osds,
        nodes: spec
            .nodes
            .iter()
            .map(|n| controller_proto::CephClusterNodeSpec {
                node_id: n.node_id.clone(),
                mon_addr: n.mon_addr.clone(),
                cluster_addr: n.cluster_addr.clone(),
                public_iface: n.public_iface.clone(),
                cluster_iface: n.cluster_iface.clone(),
                osd_device: n.osd_device.clone(),
                osd_devices: n.osd_devices.clone(),
                role: n.role.clone(),
                weight: n.weight,
                device_class: n.device_class.clone(),
                rack: n.rack.clone(),
                room: n.room.clone(),
            })
            .collect(),
        pools: spec
            .pools
            .iter()
            .map(|p| controller_proto::CephPoolSpec {
                name: p.name.clone(),
                application: p.application.clone(),
                size: p.size,
                min_size: p.min_size,
                pg_num: p.pg_num,
                crush_rule: p.crush_rule.clone(),
                compression_mode: p.compression_mode.clone(),
                quota_bytes: p.quota_bytes,
                pool_type: p.pool_type.clone(),
                erasure_profile: p.erasure_profile.clone(),
            })
            .collect(),
        recovery: spec
            .recovery
            .as_ref()
            .map(|r| controller_proto::CephRecoverySpec {
                max_backfills: r.max_backfills,
                recovery_max_active: r.recovery_max_active,
                scrub_begin_hour: r.scrub_begin_hour,
                scrub_end_hour: r.scrub_end_hour,
                nearfull_ratio_bp: r.nearfull_ratio_bp,
            }),
        ms_mode: spec.ms_mode.clone(),
        image_features: spec.image_features.clone(),
        cache_policy: spec.cache_policy.clone(),
        lost_members: spec.lost_members.clone(),
        external: spec
            .external
            .as_ref()
            .map(|e| controller_proto::CephExternalSpec {
                mon_hosts: e.mon_hosts.clone(),
                pool: e.pool.clone(),
                client_keyring: e.client_keyring.clone(),
                client_name: e.client_name.clone(),
            }),
        failure_domain: spec.failure_domain.clone(),
        stretch: spec.stretch,
        mirroring: spec
            .mirroring
            .as_ref()
            .map(|m| controller_proto::CephMirroringSpec {
                enabled: m.enabled,
                mode: m.mode.clone(),
                peer_cluster: m.peer_cluster.clone(),
            }),
    }
}

fn print_plan(name: &str, plan: &controller_proto::PlanCephClusterResponse) {
    println!("=== CephCluster plan: {name} ===");
    println!();
    println!("!!! {} !!!", plan.encrypt_osds_notice);
    println!();
    for node in &plan.nodes {
        let mark = if node.ok { "ok" } else { "FAIL" };
        println!("node {} [{mark}]", node.node_id);
        for issue in &node.issues {
            println!("  - {issue}");
        }
    }
    for issue in &plan.cluster_issues {
        println!("cluster: {issue}");
    }
    for member in &plan.removing_members {
        println!("removing member: {member}");
    }
    for warning in &plan.cluster_warnings {
        println!("warning: {warning}");
    }
}

pub async fn apply_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;
    let cluster = controller_proto::CephCluster {
        name: manifest.metadata.name.clone(),
        generation: 0,
        spec: Some(to_proto_spec(&manifest.spec)),
        status: None,
        created_at: None,
        updated_at: None,
    };

    let plan = client
        .plan_ceph_cluster(controller_proto::PlanCephClusterRequest {
            ceph_cluster: Some(cluster.clone()),
        })
        .await
        .context("plan_ceph_cluster rpc")?
        .into_inner();

    print_plan(&manifest.metadata.name, &plan);
    if !plan.ok {
        bail!("CephCluster preflight failed; refuse to apply (fix the issues above)");
    }
    println!();
    println!("preflight ok — applying");

    let resp = client
        .create_ceph_cluster(controller_proto::CreateCephClusterRequest {
            ceph_cluster: Some(cluster),
        })
        .await
        .context("create_ceph_cluster rpc")?
        .into_inner();

    let label = format!("ceph cluster '{}'", manifest.metadata.name);
    println!(
        "{}",
        render_apply_summary(resp.action, &resp.changed_fields, &label)
    );
    Ok(())
}

pub async fn plan_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;
    let plan = client
        .plan_ceph_cluster(controller_proto::PlanCephClusterRequest {
            ceph_cluster: Some(controller_proto::CephCluster {
                name: manifest.metadata.name.clone(),
                generation: 0,
                spec: Some(to_proto_spec(&manifest.spec)),
                status: None,
                created_at: None,
                updated_at: None,
            }),
        })
        .await
        .context("plan_ceph_cluster rpc")?
        .into_inner();
    print_plan(&manifest.metadata.name, &plan);
    if !plan.ok {
        bail!("CephCluster preflight failed");
    }
    Ok(())
}

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_ceph_clusters(controller_proto::ListCephClustersRequest {})
        .await?
        .into_inner();
    if resp.ceph_clusters.is_empty() {
        println!("No Ceph clusters found");
        return Ok(());
    }
    println!("{:<24}  {:>4}  {:<14}  HEALTH", "NAME", "GEN", "PHASE");
    for cluster in resp.ceph_clusters {
        let status = cluster.status.unwrap_or_default();
        let phase = controller_proto::CephClusterPhase::try_from(status.phase)
            .unwrap_or(controller_proto::CephClusterPhase::Unspecified);
        println!(
            "{:<24}  {:>4}  {:<14}  {}",
            cluster.name,
            cluster.generation,
            format!("{phase:?}").replace("CephClusterPhase", ""),
            status.health_message
        );
    }
    Ok(())
}

fn print_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_ceph_cluster(controller_proto::GetCephClusterRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    let cluster = resp
        .ceph_cluster
        .ok_or_else(|| anyhow::anyhow!("empty response"))?;
    let status = cluster.status.unwrap_or_default();
    let spec = cluster.spec.unwrap_or_default();
    println!("Name:       {}", cluster.name);
    println!("Generation: {}", cluster.generation);
    println!("FSID:       {}", spec.fsid);
    println!("Public:     {}", spec.public_network);
    println!("Cluster:    {}", spec.cluster_network);
    println!("Size/min:   {}/{}", spec.size, spec.min_size);
    println!("Encrypt OSDs: {}", spec.encrypt_osds);
    if !spec.ms_mode.is_empty() {
        println!("msMode:     {}", spec.ms_mode);
    }
    if !spec.image_features.is_empty() {
        println!("Image features: {}", spec.image_features);
    }
    if !spec.cache_policy.is_empty() {
        println!("Cache policy: {}", spec.cache_policy);
    }
    if let Some(ext) = &spec.external {
        println!("External:   monHosts={} pool={}", ext.mon_hosts, ext.pool);
    }
    if !spec.lost_members.is_empty() {
        println!("Lost members: {}", spec.lost_members.join(", "));
    }
    println!(
        "Phase:      {:?}",
        controller_proto::CephClusterPhase::try_from(status.phase)
            .unwrap_or(controller_proto::CephClusterPhase::Unspecified)
    );
    println!("Health:     {}", status.health_message);
    if !status.operation_message.is_empty() {
        println!("Operation:  {}", status.operation_message);
    }

    if let Some(h) = status.typed_health.as_ref() {
        println!();
        println!("=== Health ===");
        println!("Status:     {}", h.health_status);
        if !h.mon_quorum.is_empty() {
            println!(
                "Mon quorum: {} ({}/{})",
                h.mon_quorum.join(","),
                h.mon_quorum.len(),
                h.mon_count
            );
        }
        println!(
            "OSDs:       {} total, {} up, {} in",
            h.osd_total, h.osd_up, h.osd_in
        );
        println!(
            "PGs:        {} total, {} not active+clean, {} backfill",
            h.pgs_total, h.pgs_not_clean, h.backfill_pgs
        );
        for pg in &h.pgs_by_state {
            println!("  {:<40} {}", pg.state, pg.count);
        }
        if !h.flags.is_empty() {
            println!("Flags:      {}", h.flags.join(", "));
        }
        if h.bytes_total > 0 {
            println!(
                "Capacity:   {} used / {} total ({} avail)",
                print_bytes(h.bytes_used),
                print_bytes(h.bytes_total),
                print_bytes(h.bytes_avail)
            );
        }
        for check in &h.checks {
            println!("  [{}] {}: {}", check.severity, check.r#type, check.summary);
        }
    }

    if !spec.pools.is_empty() {
        println!();
        println!("=== Pools ===");
        for p in &spec.pools {
            println!(
                "  - {} app={} size={}/{} crush={} compression={} type={}",
                p.name,
                if p.application.is_empty() {
                    "rbd"
                } else {
                    &p.application
                },
                p.size,
                p.min_size,
                if p.crush_rule.is_empty() {
                    "(default)"
                } else {
                    &p.crush_rule
                },
                if p.compression_mode.is_empty() {
                    "(default)"
                } else {
                    &p.compression_mode
                },
                if p.pool_type.is_empty() {
                    "replicated"
                } else {
                    &p.pool_type
                }
            );
        }
    }

    if let Some(r) = spec.recovery.as_ref() {
        println!();
        println!("=== Recovery ===");
        if r.max_backfills > 0 {
            println!("maxBackfills: {}", r.max_backfills);
        }
        if r.recovery_max_active > 0 {
            println!("recoveryMaxActive: {}", r.recovery_max_active);
        }
        if r.scrub_begin_hour >= 0 || r.scrub_end_hour >= 0 {
            println!("scrubHours: {}-{}", r.scrub_begin_hour, r.scrub_end_hour);
        }
        if r.nearfull_ratio_bp > 0 {
            println!("nearfullRatio: {:.2}%", r.nearfull_ratio_bp as f64 / 100.0);
        }
    }

    println!();
    println!("=== Members ===");
    for n in &spec.nodes {
        let mut devices = vec![n.osd_device.clone()];
        for extra in &n.osd_devices {
            if !extra.is_empty() && !devices.iter().any(|have| have == extra) {
                devices.push(extra.clone());
            }
        }
        let role = if n.role.is_empty() { "full" } else { &n.role };
        let mut extras = Vec::new();
        if n.weight > 0.0 {
            extras.push(format!("weight={}", n.weight));
        }
        if !n.device_class.is_empty() {
            extras.push(format!("class={}", n.device_class));
        }
        let extra = if extras.is_empty() {
            String::new()
        } else {
            format!(" {}", extras.join(" "))
        };
        println!(
            "  - {} role={} mon={} cluster={} osd={}{extra}",
            n.node_id,
            role,
            n.mon_addr,
            n.cluster_addr,
            devices.join(",")
        );
    }
    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str, force: bool) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_ceph_cluster(controller_proto::DeleteCephClusterRequest {
            name: name.to_string(),
            force,
        })
        .await?
        .into_inner();
    if resp.success {
        println!("Deleted Ceph cluster '{name}'");
    } else {
        bail!("failed to delete Ceph cluster '{name}'");
    }
    Ok(())
}

pub async fn finalize_osd_release(
    info: &ConnectionInfo,
    name: &str,
    yes: bool,
    release: Option<String>,
) -> Result<()> {
    if !yes {
        bail!("pass --yes to run require-osd-release");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .finalize_ceph_osd_release(controller_proto::FinalizeCephOsdReleaseRequest {
            name: name.to_string(),
            confirm: true,
            release: release.unwrap_or_default(),
        })
        .await
        .context("finalize_ceph_osd_release rpc")?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    if !resp.release.is_empty() {
        println!("release: {}", resp.release);
    }
    Ok(())
}

pub async fn rotate_mon_key(info: &ConnectionInfo, name: &str, yes: bool) -> Result<()> {
    if !yes {
        bail!("pass --yes to rotate the mon key");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .rotate_ceph_mon_key(controller_proto::RotateCephMonKeyRequest {
            name: name.to_string(),
            confirm: true,
        })
        .await
        .context("rotate_ceph_mon_key rpc")?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn rotate_osd_dmcrypt(
    info: &ConnectionInfo,
    name: &str,
    osd_id: i32,
    yes: bool,
) -> Result<()> {
    if !yes {
        bail!("pass --yes to drain, zap, and recreate an encrypted OSD");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .rotate_ceph_osd_dmcrypt(controller_proto::RotateCephOsdDmcryptRequest {
            name: name.to_string(),
            osd_id,
            confirm: true,
        })
        .await
        .context("rotate_ceph_osd_dmcrypt rpc")?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_proto_spec_defaults_encrypt_osds_true() {
        let manifest = r#"
kind: CephCluster
metadata:
  name: lab
spec:
  publicNetwork: "10.10.0.0/24"
  clusterNetwork: "10.20.0.0/24"
  nodes:
    - nodeId: n1
      monAddr: "10.10.0.11:6789"
      clusterAddr: "10.20.0.11"
      publicIface: eth1
      clusterIface: eth2
      osdDevice: /dev/nvme0n1
"#;
        let m: CephClusterManifest = serde_yaml::from_str(manifest).unwrap();
        let proto = to_proto_spec(&m.spec);
        assert!(proto.encrypt_osds, "new manifests default encryptOsds true");
        assert_eq!(proto.image_features, "");
        assert!(proto.pools.is_empty());
    }

    #[test]
    fn to_proto_spec_reads_pools_and_roles() {
        let manifest = r#"
kind: CephCluster
metadata:
  name: lab
spec:
  publicNetwork: "10.10.0.0/24"
  clusterNetwork: "10.20.0.0/24"
  msMode: secure
  cachePolicy: writethrough
  imageFeatures: layering,exclusive-lock
  lostMembers: [dead-1]
  recovery:
    maxBackfills: 1
  pools:
    - name: kcore-vms
      application: rbd
      size: 3
      minSize: 2
      crushRule: ssd
  nodes:
    - nodeId: n1
      role: mon
      monAddr: "10.10.0.11:6789"
      clusterAddr: "10.20.0.11"
      publicIface: eth1
      clusterIface: eth2
"#;
        let m: CephClusterManifest = serde_yaml::from_str(manifest).unwrap();
        let proto = to_proto_spec(&m.spec);
        assert_eq!(proto.ms_mode, "secure");
        assert_eq!(proto.cache_policy, "writethrough");
        assert_eq!(proto.pools[0].crush_rule, "ssd");
        assert_eq!(proto.nodes[0].role, "mon");
        assert_eq!(proto.lost_members, vec!["dead-1".to_string()]);
        assert_eq!(proto.recovery.as_ref().unwrap().max_backfills, 1);
    }
}
