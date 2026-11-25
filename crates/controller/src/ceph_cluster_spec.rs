use std::collections::HashSet;
use std::path::Path;

use crate::controller_proto::{
    CephClusterNodeSpec, CephClusterSpec, CephExternalSpec, CephMirroringSpec, CephPoolSpec,
    CephRecoverySpec,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSpec {
    fsid: String,
    public_network: String,
    cluster_network: String,
    size: i32,
    min_size: i32,
    #[serde(default)]
    force_wipe: bool,
    /// OSD dm-crypt. Missing in pre-E1 JSON → false (legacy plain OSDs).
    /// New clusters set true via kctl / create path.
    #[serde(default)]
    encrypt_osds: bool,
    nodes: Vec<StoredNode>,
    #[serde(default)]
    pools: Vec<StoredPool>,
    #[serde(default)]
    recovery: Option<StoredRecovery>,
    #[serde(default)]
    ms_mode: String,
    #[serde(default)]
    image_features: String,
    #[serde(default)]
    cache_policy: String,
    #[serde(default)]
    lost_members: Vec<String>,
    #[serde(default)]
    external: Option<StoredExternal>,
    /// host (default), rack, or room.
    #[serde(default)]
    failure_domain: String,
    #[serde(default)]
    stretch: bool,
    #[serde(default)]
    mirroring: Option<StoredMirroring>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredNode {
    node_id: String,
    mon_addr: String,
    cluster_addr: String,
    public_iface: String,
    cluster_iface: String,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredMirroring {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    peer_cluster: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPool {
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredRecovery {
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredExternal {
    mon_hosts: String,
    pool: String,
    #[serde(default)]
    client_keyring: String,
    #[serde(default)]
    client_name: String,
}

/// The external-cluster settings, when the spec names any mons.
pub fn active_external(spec: &CephClusterSpec) -> Option<&CephExternalSpec> {
    spec.external
        .as_ref()
        .filter(|e| !e.mon_hosts.trim().is_empty())
}

pub fn validate_spec(spec: &CephClusterSpec) -> Result<(), String> {
    let external = active_external(spec);
    if let Some(ext) = external {
        if !spec.nodes.is_empty() {
            return Err("spec.external cannot be mixed with spec.nodes".into());
        }
        if ext.pool.trim().is_empty() {
            return Err("spec.external.pool is required".into());
        }
        return Ok(());
    }
    if spec.nodes.is_empty() {
        return Err("spec.nodes must contain at least one node".into());
    }
    if spec.public_network.trim().is_empty() || spec.cluster_network.trim().is_empty() {
        return Err("publicNetwork and clusterNetwork are required".into());
    }
    if spec.size <= 0 || spec.min_size <= 0 || spec.size < spec.min_size {
        return Err("size must be >= minSize and both must be positive".into());
    }
    let ms = spec.ms_mode.trim();
    if !ms.is_empty() && ms != "secure" && ms != "prefer-crc" {
        return Err("msMode must be empty, secure, or prefer-crc".into());
    }
    let cache = spec.cache_policy.trim();
    if !cache.is_empty() && cache != "writeback" && cache != "writethrough" {
        return Err("cachePolicy must be empty, writeback, or writethrough".into());
    }
    let mut ids = HashSet::new();
    for node in &spec.nodes {
        if node.node_id.trim().is_empty() || !ids.insert(node.node_id.trim()) {
            return Err(format!(
                "nodeId must be non-empty and unique: {}",
                node.node_id
            ));
        }
        let role = normalize_role(&node.role);
        if role != "full" && role != "mon" && role != "osd" {
            return Err(format!(
                "role for {} must be full, mon, or osd",
                node.node_id
            ));
        }
        let devices = osd_devices(node);
        if role != "mon" && devices.is_empty() {
            return Err(format!("osdDevice for {} is required", node.node_id));
        }
        for device in &devices {
            if !Path::new(device).is_absolute() {
                return Err(format!(
                    "osdDevice for {} must be absolute: {device}",
                    node.node_id
                ));
            }
        }
        if node.public_iface.trim().is_empty() || node.cluster_iface.trim().is_empty() {
            return Err(format!(
                "publicIface and clusterIface are required for {}",
                node.node_id
            ));
        }
    }
    for pool in &spec.pools {
        if pool.name.trim().is_empty() {
            return Err("pools[].name is required".into());
        }
        let pool_type = if pool.pool_type.trim().is_empty() {
            "replicated"
        } else {
            pool.pool_type.trim()
        };
        if pool_type != "replicated" && pool_type != "erasure" {
            return Err(format!(
                "pools[{}].poolType must be replicated or erasure",
                pool.name
            ));
        }
        if pool_type == "erasure" && pool.erasure_profile.trim().is_empty() {
            return Err(format!(
                "pools[{}].erasureProfile is required for erasure pools",
                pool.name
            ));
        }
        if pool.size > 0 && pool.min_size > 0 && pool.size < pool.min_size {
            return Err(format!("pools[{}].size must be >= minSize", pool.name));
        }
    }
    for lost in &spec.lost_members {
        if lost.trim().is_empty() {
            return Err("lostMembers entries must be non-empty".into());
        }
    }
    let domain = failure_domain(spec);
    if domain == "rack" {
        for node in &spec.nodes {
            if node.rack.trim().is_empty() {
                return Err(format!(
                    "rack is required on {} when failureDomain is rack",
                    node.node_id
                ));
            }
        }
    }
    if domain == "room" {
        for node in &spec.nodes {
            if node.room.trim().is_empty() {
                return Err(format!(
                    "room is required on {} when failureDomain is room",
                    node.node_id
                ));
            }
        }
    }
    if spec.stretch {
        if domain != "room" {
            return Err("stretch requires failureDomain: room".into());
        }
        let tiebreaker = spec
            .nodes
            .iter()
            .find(|n| normalize_role(&n.role) == "mon" && osd_devices(n).is_empty());
        if tiebreaker.is_none() {
            return Err(
                "stretch requires a role=mon member with no OSD devices as tiebreaker".into(),
            );
        }
    }
    if let Some(m) = &spec.mirroring {
        if m.enabled && m.peer_cluster.trim().is_empty() {
            return Err("mirroring.peerCluster is required when mirroring.enabled".into());
        }
        let mode = m.mode.trim();
        if !mode.is_empty() && mode != "pool" && mode != "image" {
            return Err("mirroring.mode must be empty, pool, or image".into());
        }
    }
    Ok(())
}

/// Loud plan notice for encryptOsds (immutable after create).
pub fn encrypt_osds_notice(encrypt: bool) -> String {
    if encrypt {
        "encryptOsds=true (DEFAULT): every OSD will be LUKS-encrypted via ceph-volume --dmcrypt. \
This is immutable after create — set encryptOsds: false explicitly only for labs."
            .into()
    } else {
        "encryptOsds=false: OSD disks will NOT be encrypted. Immutable after create.".into()
    }
}

pub fn normalize_role(role: &str) -> String {
    let role = role.trim().to_ascii_lowercase();
    if role.is_empty() {
        "full".into()
    } else {
        role
    }
}

pub fn default_image_features(spec: &CephClusterSpec) -> String {
    let features = spec.image_features.trim();
    if features.is_empty() {
        "layering,exclusive-lock".into()
    } else {
        features.to_string()
    }
}

pub fn default_cache_policy(spec: &CephClusterSpec) -> String {
    let policy = spec.cache_policy.trim();
    if policy.is_empty() {
        "writeback".into()
    } else {
        policy.to_string()
    }
}

/// Kernel paths this node should have OSDs on.
///
/// `osd_device` is the original single-disk field. `osd_devices` adds more
/// without dropping it, so a manifest can grow a second disk in place.
pub fn osd_devices(node: &CephClusterNodeSpec) -> Vec<String> {
    let mut out = Vec::new();
    let primary = node.osd_device.trim();
    if !primary.is_empty() {
        out.push(primary.to_string());
    }
    for device in &node.osd_devices {
        let device = device.trim();
        if !device.is_empty() && !out.iter().any(|existing| existing == device) {
            out.push(device.to_string());
        }
    }
    out
}

fn store_node(n: &CephClusterNodeSpec) -> StoredNode {
    StoredNode {
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
    }
}

fn load_node(n: StoredNode) -> CephClusterNodeSpec {
    CephClusterNodeSpec {
        node_id: n.node_id,
        mon_addr: n.mon_addr,
        cluster_addr: n.cluster_addr,
        public_iface: n.public_iface,
        cluster_iface: n.cluster_iface,
        osd_device: n.osd_device,
        osd_devices: n.osd_devices,
        role: n.role,
        weight: n.weight,
        device_class: n.device_class,
        rack: n.rack,
        room: n.room,
    }
}

fn store_mirroring(m: &CephMirroringSpec) -> StoredMirroring {
    StoredMirroring {
        enabled: m.enabled,
        mode: m.mode.clone(),
        peer_cluster: m.peer_cluster.clone(),
    }
}

fn load_mirroring(m: StoredMirroring) -> CephMirroringSpec {
    CephMirroringSpec {
        enabled: m.enabled,
        mode: m.mode,
        peer_cluster: m.peer_cluster,
    }
}

/// Normalized failure domain: host | rack | room.
pub fn failure_domain(spec: &CephClusterSpec) -> &str {
    match spec.failure_domain.trim().to_ascii_lowercase().as_str() {
        "rack" => "rack",
        "room" => "room",
        _ => "host",
    }
}

/// Rule name pools use when crush_rule is empty and failure_domain is not host.
pub fn default_crush_rule(spec: &CephClusterSpec) -> String {
    let domain = failure_domain(spec);
    if domain == "host" {
        String::new()
    } else {
        format!("kcore-{domain}")
    }
}

fn store_pool(p: &CephPoolSpec) -> StoredPool {
    StoredPool {
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
    }
}

fn load_pool(p: StoredPool) -> CephPoolSpec {
    CephPoolSpec {
        name: p.name,
        application: p.application,
        size: p.size,
        min_size: p.min_size,
        pg_num: p.pg_num,
        crush_rule: p.crush_rule,
        compression_mode: p.compression_mode,
        quota_bytes: p.quota_bytes,
        pool_type: p.pool_type,
        erasure_profile: p.erasure_profile,
    }
}

fn store_recovery(r: &CephRecoverySpec) -> StoredRecovery {
    StoredRecovery {
        max_backfills: r.max_backfills,
        recovery_max_active: r.recovery_max_active,
        scrub_begin_hour: if r.scrub_begin_hour == 0 {
            -1
        } else {
            r.scrub_begin_hour
        },
        scrub_end_hour: if r.scrub_end_hour == 0 {
            -1
        } else {
            r.scrub_end_hour
        },
        nearfull_ratio_bp: r.nearfull_ratio_bp,
    }
}

fn load_recovery(r: StoredRecovery) -> CephRecoverySpec {
    CephRecoverySpec {
        max_backfills: r.max_backfills,
        recovery_max_active: r.recovery_max_active,
        scrub_begin_hour: r.scrub_begin_hour,
        scrub_end_hour: r.scrub_end_hour,
        nearfull_ratio_bp: r.nearfull_ratio_bp,
    }
}

pub fn spec_to_json(spec: &CephClusterSpec) -> Result<String, serde_json::Error> {
    // Do not persist the external client keyring after the first write would
    // already have stored it; callers that need a redacted get clear it.
    serde_json::to_string(&StoredSpec {
        fsid: spec.fsid.clone(),
        public_network: spec.public_network.clone(),
        cluster_network: spec.cluster_network.clone(),
        size: spec.size,
        min_size: spec.min_size,
        force_wipe: spec.force_wipe,
        encrypt_osds: spec.encrypt_osds,
        nodes: spec.nodes.iter().map(store_node).collect(),
        pools: spec.pools.iter().map(store_pool).collect(),
        recovery: spec.recovery.as_ref().map(store_recovery),
        ms_mode: spec.ms_mode.clone(),
        image_features: spec.image_features.clone(),
        cache_policy: spec.cache_policy.clone(),
        lost_members: spec.lost_members.clone(),
        external: spec.external.as_ref().map(|e| StoredExternal {
            mon_hosts: e.mon_hosts.clone(),
            pool: e.pool.clone(),
            client_keyring: e.client_keyring.clone(),
            client_name: e.client_name.clone(),
        }),
        failure_domain: spec.failure_domain.clone(),
        stretch: spec.stretch,
        mirroring: spec.mirroring.as_ref().map(store_mirroring),
    })
}

pub fn spec_from_json(json: &str) -> Result<CephClusterSpec, serde_json::Error> {
    let s: StoredSpec = serde_json::from_str(json)?;
    Ok(CephClusterSpec {
        fsid: s.fsid,
        public_network: s.public_network,
        cluster_network: s.cluster_network,
        size: s.size,
        min_size: s.min_size,
        force_wipe: s.force_wipe,
        encrypt_osds: s.encrypt_osds,
        nodes: s.nodes.into_iter().map(load_node).collect(),
        pools: s.pools.into_iter().map(load_pool).collect(),
        recovery: s.recovery.map(load_recovery),
        ms_mode: s.ms_mode,
        image_features: s.image_features,
        cache_policy: s.cache_policy,
        lost_members: s.lost_members,
        external: s.external.map(|e| CephExternalSpec {
            mon_hosts: e.mon_hosts,
            pool: e.pool,
            client_keyring: e.client_keyring,
            client_name: e.client_name,
        }),
        failure_domain: s.failure_domain,
        stretch: s.stretch,
        mirroring: s.mirroring.map(load_mirroring),
    })
}

/// Node ids present in `old` but not in `new`: the members an apply retires.
pub fn removed_members(old: &CephClusterSpec, new: &CephClusterSpec) -> Vec<String> {
    old.nodes
        .iter()
        .filter(|n| !new.nodes.iter().any(|m| m.node_id == n.node_id))
        .map(|n| n.node_id.clone())
        .collect()
}

/// What the cluster looked like when the operator asked to retire a member.
pub struct RemovalContext<'a> {
    /// `ceph -s` health of the cluster right now; `None` when no survivor
    /// could be asked (treated as not healthy: a blind removal is refused).
    pub health_status: Option<&'a str>,
    /// Members whose earlier removal the reconciler has not finished.
    pub pending_removals: &'a [String],
}

/// Verdict on retiring the members that `new` drops from `old`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RemovalPlan {
    pub removed: Vec<String>,
    /// Blocking: the apply is refused while any of these hold.
    pub issues: Vec<String>,
    /// Shown to the operator; the apply proceeds.
    pub warnings: Vec<String>,
}

/// Each replica lives on a distinct host (the default CRUSH rule), so the
/// survivors must still number at least `size`; the cluster must be
/// HEALTH_OK so every PG is fully replicated before one copy leaves; and
/// removals go one at a time so the drain of one host finishes before the
/// next starts.
pub fn plan_member_removal(
    old: &CephClusterSpec,
    new: &CephClusterSpec,
    ctx: &RemovalContext<'_>,
) -> RemovalPlan {
    let removed = removed_members(old, new);
    if removed.is_empty() {
        return RemovalPlan::default();
    }
    let mut issues = Vec::new();
    let mut warnings = Vec::new();
    let leaving = removed.join(", ");
    if removed.len() > 1 {
        issues.push(format!(
            "remove one member per apply so each host drains before the next leaves; this apply removes {}: {leaving}",
            removed.len()
        ));
    }
    let remaining = new.nodes.len();
    if (remaining as i64) < i64::from(new.size) {
        issues.push(format!(
            "removing {leaving} leaves {remaining} host(s) for size {}: every replica needs its own host, so lower size (and minSize) first or add a member",
            new.size
        ));
    }
    if remaining == 2 {
        warnings.push(
            "2 monitors cannot survive losing either one (quorum needs a majority); plan to go back to 3+ members or down to 1"
                .to_string(),
        );
    }
    match ctx.health_status {
        Some("HEALTH_OK") => {}
        Some(status) => issues.push(format!(
            "cluster is {status}; wait for HEALTH_OK so every placement group is fully replicated before a host leaves"
        )),
        None => issues.push(
            "could not read ceph health from a surviving member; refusing to remove a host blind".to_string(),
        ),
    }
    let still_pending: Vec<&str> = ctx
        .pending_removals
        .iter()
        .filter(|id| !removed.contains(id))
        .map(String::as_str)
        .collect();
    if !still_pending.is_empty() {
        issues.push(format!(
            "removal of {} is still in progress; wait for it to finish",
            still_pending.join(", ")
        ));
    }
    let lost: Vec<&str> = removed
        .iter()
        .filter(|id| new.lost_members.iter().any(|l| l == *id))
        .map(String::as_str)
        .collect();
    if lost.is_empty() {
        warnings.push(format!(
            "{leaving} will be drained (OSDs marked out, wait for every PG to be active+clean), then its OSDs purged and its monitor removed"
        ));
    } else {
        warnings.push(format!(
            "{} is in lostMembers: survivors will purge its OSDs and mon after the drain; TeardownCephNode will not be dialed on that host",
            lost.join(", ")
        ));
    }
    RemovalPlan {
        removed,
        issues,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_node(id: &str) -> CephClusterNodeSpec {
        CephClusterNodeSpec {
            node_id: id.into(),
            mon_addr: "10.10.0.11:6789".into(),
            cluster_addr: "10.20.0.11".into(),
            public_iface: "eth1".into(),
            cluster_iface: "eth2".into(),
            osd_device: "/dev/nvme0n1".into(),
            osd_devices: Vec::new(),
            role: String::new(),
            weight: 0.0,
            device_class: String::new(),
            rack: String::new(),
            room: String::new(),
        }
    }

    fn valid_spec() -> CephClusterSpec {
        CephClusterSpec {
            fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            public_network: "10.10.0.0/24".into(),
            cluster_network: "10.20.0.0/24".into(),
            size: 3,
            min_size: 2,
            force_wipe: false,
            encrypt_osds: true,
            nodes: vec![valid_node("n1"), valid_node("n2"), valid_node("n3")],
            pools: Vec::new(),
            recovery: None,
            ms_mode: String::new(),
            image_features: String::new(),
            cache_policy: String::new(),
            lost_members: Vec::new(),
            external: None,
            failure_domain: String::new(),
            stretch: false,
            mirroring: None,
        }
    }

    fn ctx<'a>(health: Option<&'a str>, pending: &'a [String]) -> RemovalContext<'a> {
        RemovalContext {
            health_status: health,
            pending_removals: pending,
        }
    }

    #[test]
    fn encrypt_osds_notice_is_loud_for_default_true() {
        let notice = encrypt_osds_notice(true);
        assert!(notice.contains("DEFAULT"));
        assert!(notice.contains("immutable"));
        let plain = encrypt_osds_notice(false);
        assert!(plain.contains("NOT be encrypted"));
    }

    #[test]
    fn osd_devices_keeps_the_primary_and_appends_more() {
        let mut node = valid_node("n1");
        node.osd_devices = vec!["/dev/nvme1n1".into(), "/dev/nvme0n1".into()];
        assert_eq!(
            osd_devices(&node),
            vec!["/dev/nvme0n1".to_string(), "/dev/nvme1n1".to_string()]
        );
    }

    #[test]
    fn round_trip_preserves_new_fields() {
        let mut spec = valid_spec();
        spec.nodes[0].role = "mon".into();
        spec.nodes[0].osd_device.clear();
        spec.nodes[1].weight = 1.5;
        spec.nodes[1].device_class = "ssd".into();
        spec.pools.push(CephPoolSpec {
            name: "kcore-vms".into(),
            application: "rbd".into(),
            size: 3,
            min_size: 2,
            pg_num: 0,
            crush_rule: "ssd".into(),
            compression_mode: "passive".into(),
            quota_bytes: 0,
            pool_type: "replicated".into(),
            erasure_profile: String::new(),
        });
        spec.recovery = Some(CephRecoverySpec {
            max_backfills: 1,
            recovery_max_active: 2,
            scrub_begin_hour: 2,
            scrub_end_hour: 5,
            nearfull_ratio_bp: 8500,
        });
        spec.ms_mode = "secure".into();
        spec.image_features = "layering,exclusive-lock".into();
        spec.cache_policy = "writethrough".into();
        spec.lost_members = vec!["n3".into()];
        let back = spec_from_json(&spec_to_json(&spec).unwrap()).unwrap();
        assert_eq!(back.nodes[0].role, "mon");
        assert_eq!(back.nodes[1].weight, 1.5);
        assert_eq!(back.pools[0].crush_rule, "ssd");
        assert_eq!(back.recovery.as_ref().unwrap().max_backfills, 1);
        assert_eq!(back.ms_mode, "secure");
        assert_eq!(back.lost_members, vec!["n3".to_string()]);
    }

    #[test]
    fn external_refuses_nodes() {
        let mut spec = valid_spec();
        spec.external = Some(CephExternalSpec {
            mon_hosts: "10.0.0.1:6789".into(),
            pool: "vms".into(),
            client_keyring: "[client.kcore]\nkey = ABC".into(),
            client_name: "client.kcore".into(),
        });
        assert!(validate_spec(&spec).unwrap_err().contains("mixed"));
        spec.nodes.clear();
        assert!(validate_spec(&spec).is_ok());
    }

    #[test]
    fn mon_role_allows_empty_osd_device() {
        let mut spec = valid_spec();
        spec.nodes[0].role = "mon".into();
        spec.nodes[0].osd_device.clear();
        assert!(validate_spec(&spec).is_ok());
    }

    #[test]
    fn json_defaults_encrypt_osds_false_for_legacy() {
        let json = r#"{"fsid":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","publicNetwork":"10.10.0.0/24","clusterNetwork":"10.20.0.0/24","size":3,"minSize":2,"nodes":[{"nodeId":"n1","monAddr":"10.10.0.11:6789","clusterAddr":"10.20.0.11","publicIface":"eth1","clusterIface":"eth2","osdDevice":"/dev/nvme0n1"}]}"#;
        let spec = spec_from_json(json).expect("decode");
        assert!(!spec.encrypt_osds);
        assert!(spec.pools.is_empty());
        assert!(spec.lost_members.is_empty());
    }

    #[test]
    fn json_preserves_encrypt_osds() {
        let mut spec = valid_spec();
        spec.encrypt_osds = true;
        let back = spec_from_json(&spec_to_json(&spec).unwrap()).unwrap();
        assert!(back.encrypt_osds);
        spec.encrypt_osds = false;
        let back = spec_from_json(&spec_to_json(&spec).unwrap()).unwrap();
        assert!(!back.encrypt_osds);
    }

    fn without(spec: &CephClusterSpec, id: &str) -> CephClusterSpec {
        let mut next = spec.clone();
        next.nodes.retain(|n| n.node_id != id);
        next
    }

    #[test]
    fn plan_member_removal_refuses_when_not_healthy() {
        let old = valid_spec();
        let new = without(&old, "n3");
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_WARN"), &[]));
        assert!(!plan.issues.is_empty());
    }

    #[test]
    fn plan_member_removal_ok_when_healthy() {
        let mut old = valid_spec();
        old.size = 2;
        old.min_size = 1;
        let new = without(&old, "n3");
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_OK"), &[]));
        assert!(
            plan.issues.is_empty(),
            "unexpected issues: {:?}",
            plan.issues
        );
        assert_eq!(plan.removed, vec!["n3".to_string()]);
    }

    #[test]
    fn plan_member_removal_mentions_lost_members() {
        let old = valid_spec();
        let mut new = without(&old, "n3");
        new.lost_members = vec!["n3".into()];
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_OK"), &[]));
        assert!(plan.warnings.iter().any(|w| w.contains("lostMembers")));
    }

    #[test]
    fn plan_member_removal_refuses_size() {
        let mut old = valid_spec();
        old.size = 3;
        let shrunk = without(&old, "n3");
        let plan = plan_member_removal(&old, &shrunk, &ctx(Some("HEALTH_OK"), &[]));
        assert!(plan.issues.iter().any(|i| i.contains("leaves 2 host")));
    }

    #[test]
    fn plan_member_removal_refuses_pending() {
        let old = valid_spec();
        let new = without(&old, "n3");
        let pending = vec!["n2".to_string()];
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_OK"), &pending));
        assert!(plan.issues.iter().any(|i| i.contains("still in progress")));
    }

    #[test]
    fn plan_member_removal_allows_same_pending_member() {
        let mut old = valid_spec();
        old.size = 2;
        old.min_size = 1;
        let new = without(&old, "n3");
        let pending = vec!["n3".to_string()];
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_OK"), &pending));
        assert!(
            plan.issues.is_empty(),
            "unexpected issues: {:?}",
            plan.issues
        );
    }

    #[test]
    fn default_image_features_enables_exclusive_lock() {
        let spec = valid_spec();
        assert_eq!(default_image_features(&spec), "layering,exclusive-lock");
    }
}
