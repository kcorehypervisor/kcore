//! Ingress gateway planning and reconciliation.
//!
//! [`plan_gateway`] is pure over an [`IngressState`] snapshot: it validates the
//! documents, resolves backends to addresses and renders the Caddy JSON. The
//! gRPC `PlanIngress` handler and the background reconciler share it, so what
//! `kctl ingress plan` prints is what the reconciler pushes.
//!
//! The reconciler wakes on a timer, recomputes each gateway's configuration and
//! loads it on every gateway node whose reported hash differs. Nix owns the
//! service itself (see `modules/kcore-ingress.nix`); until a node has enabled it
//! the push simply retries.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::time;
use tracing::{debug, info, warn};

use crate::db::{
    Database, IngressGatewayStatusRow, IngressRouteStatusRow, NetworkRow, NodeRow, VmRow,
};
use crate::ingress_render::{render_caddy, NodePaths, RenderInput, Rendered};
use crate::ingress_spec::{
    self, check_host_conflicts, is_wildcard_host, validate_gateway, validate_route,
    validate_waf_policy, Backend, GatewaySpec, RouteKind, RouteSpec, WafPolicySpec,
};
use crate::node_client::NodeClients;
use crate::node_proto;

const RECONCILE_TICK: Duration = Duration::from_secs(10);
/// WAF events older than this are dropped.
const WAF_EVENT_RETENTION_SECS: i64 = 14 * 86_400;
/// Let's Encrypt allows 50 certificates per registered domain per week; the
/// plan warns before a gateway gets near it.
const LE_HOSTNAME_WARN: usize = 50;

/// Everything planning needs, loaded once so a plan is consistent.
#[derive(Debug, Clone, Default)]
pub struct IngressState {
    pub gateways: BTreeMap<String, GatewaySpec>,
    pub routes: BTreeMap<String, RouteSpec>,
    pub policies: BTreeMap<String, WafPolicySpec>,
    pub vms: Vec<VmRow>,
    pub nodes: Vec<NodeRow>,
    pub networks: Vec<NetworkRow>,
    /// Stored documents that no longer parse; reported by every plan.
    pub corrupt: Vec<String>,
}

pub fn load_state(db: &Database) -> Result<IngressState, String> {
    let mut st = IngressState::default();
    for row in db.list_ingress_gateways().map_err(|e| e.to_string())? {
        match ingress_spec::parse_gateway(&row.spec_json) {
            Ok(spec) => {
                st.gateways.insert(row.name, spec);
            }
            Err(e) => st.corrupt.push(format!("gateway {}: {e}", row.name)),
        }
    }
    for row in db.list_ingress_routes("").map_err(|e| e.to_string())? {
        match ingress_spec::parse_route(&row.spec_json) {
            Ok(spec) => {
                st.routes.insert(row.name, spec);
            }
            Err(e) => st.corrupt.push(format!("route {}: {e}", row.name)),
        }
    }
    for row in db.list_waf_policies().map_err(|e| e.to_string())? {
        match ingress_spec::parse_waf_policy(&row.spec_json) {
            Ok(spec) => {
                st.policies.insert(row.name, spec);
            }
            Err(e) => st.corrupt.push(format!("waf policy {}: {e}", row.name)),
        }
    }
    st.vms = db.list_vms().map_err(|e| e.to_string())?;
    st.nodes = db.list_nodes().map_err(|e| e.to_string())?;
    st.networks = db.list_networks().map_err(|e| e.to_string())?;
    Ok(st)
}

/// Overlay one `(kind, name, spec_json)` document onto the state.
pub fn apply_document(
    st: &mut IngressState,
    kind: &str,
    name: &str,
    spec_json: &str,
) -> Result<(), String> {
    match kind {
        "IngressGateway" => {
            st.gateways
                .insert(name.to_string(), ingress_spec::parse_gateway(spec_json)?);
        }
        "IngressRoute" => {
            st.routes
                .insert(name.to_string(), ingress_spec::parse_route(spec_json)?);
        }
        "WafPolicy" => {
            st.policies
                .insert(name.to_string(), ingress_spec::parse_waf_policy(spec_json)?);
        }
        other => return Err(format!("unknown ingress document kind {other:?}")),
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayNode {
    pub id: String,
    pub address: String,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub nodes: Vec<GatewayNode>,
    pub rendered: Option<Rendered>,
    /// Resolved dial addresses per route, for status reporting.
    pub route_dials: BTreeMap<String, Vec<String>>,
}

/// Finds a gateway's node by id or hostname.
fn find_node<'a>(nodes: &'a [NodeRow], name: &str) -> Option<&'a NodeRow> {
    nodes
        .iter()
        .find(|n| n.id == name)
        .or_else(|| nodes.iter().find(|n| n.hostname == name))
}

pub fn plan_gateway(st: &IngressState, name: &str) -> Plan {
    let mut plan = Plan::default();
    plan.errors.extend(st.corrupt.iter().cloned());
    let Some(gateway) = st.gateways.get(name) else {
        plan.errors
            .push(format!("IngressGateway {name} does not exist"));
        return plan;
    };
    if let Err(e) = validate_gateway(gateway) {
        plan.errors.push(format!("gateway {name}: {e}"));
        return plan;
    }
    let mut node_ids = BTreeSet::new();
    for n in &gateway.nodes {
        match find_node(&st.nodes, n) {
            Some(row) => {
                node_ids.insert(row.id.clone());
                plan.nodes.push(GatewayNode {
                    id: row.id.clone(),
                    address: row.address.clone(),
                });
            }
            None => plan
                .errors
                .push(format!("gateway {name}: node {n} is not registered")),
        }
    }

    let routes: BTreeMap<String, RouteSpec> = st
        .routes
        .iter()
        .filter(|(_, r)| r.gateway == name)
        .map(|(n, r)| (n.clone(), r.clone()))
        .collect();
    for (rname, route) in &routes {
        if let Err(e) = validate_route(route) {
            plan.errors.push(format!("route {rname}: {e}"));
        }
    }
    if let Err(e) = check_host_conflicts(&routes) {
        plan.errors.push(e);
    }
    for (rname, r) in &routes {
        match r.kind {
            RouteKind::Http => {
                if r.hosts.iter().any(|h| is_wildcard_host(h)) && gateway.tls.acme.dns01.is_none() {
                    plan.errors.push(format!(
                        "route {rname}: wildcard hosts need tls.acme.dns01 on gateway {name}"
                    ));
                }
            }
            RouteKind::Tcp => {
                let port = r.port.unwrap_or(0);
                if !gateway.listeners.iter().any(|l| l.ports.contains(&port)) {
                    plan.errors.push(format!(
                        "route {rname}: no listener of gateway {name} carries tcp port {port}"
                    ));
                }
            }
        }
    }

    // WAF policies named by the gateway default or a route must exist and be valid.
    let mut used_policies = BTreeSet::new();
    if let Some(p) = &gateway.default_waf_policy {
        used_policies.insert(p.clone());
    }
    for r in routes.values().filter(|r| r.kind == RouteKind::Http) {
        if r.waf.mode.as_deref() == Some("off") {
            continue;
        }
        if let Some(p) = &r.waf.policy {
            used_policies.insert(p.clone());
        }
    }
    let mut policies = BTreeMap::new();
    for p in &used_policies {
        match st.policies.get(p) {
            Some(spec) => {
                if let Err(e) = validate_waf_policy(spec) {
                    plan.errors.push(format!("WafPolicy {p}: {e}"));
                }
                policies.insert(p.clone(), spec.clone());
            }
            None => plan.errors.push(format!("WafPolicy {p} does not exist")),
        }
    }
    for (rname, r) in routes.iter().filter(|(_, r)| r.kind == RouteKind::Tcp) {
        let _ = r;
        plan.warnings.push(format!(
            "route {rname} is a TCP route: the WAF does not inspect it"
        ));
    }
    for (rname, r) in routes.iter().filter(|(_, r)| r.kind == RouteKind::Http) {
        let covered = r.waf.mode.as_deref() == Some("off")
            || r.waf.policy.is_some()
            || gateway.default_waf_policy.is_some();
        if !covered {
            plan.warnings.push(format!(
                "route {rname} has no WAF policy; traffic is not inspected"
            ));
        }
        if r.waf.mode.as_deref() == Some("off") {
            plan.warnings.push(format!("route {rname}: WAF is off"));
        }
    }

    let hostnames: usize = routes
        .values()
        .filter(|r| r.kind == RouteKind::Http)
        .map(|r| r.hosts.len())
        .sum();
    if gateway.tls.acme.ca == "letsencrypt" && hostnames > LE_HOSTNAME_WARN {
        plan.warnings.push(format!(
            "{hostnames} hostnames on Let's Encrypt production exceeds the 50 per week certificate limit"
        ));
    }

    // Backends.
    let mut backends: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (rname, r) in &routes {
        let mut dials = Vec::new();
        let mut groups: Vec<(String, &Vec<Backend>)> = r
            .paths
            .iter()
            .map(|p| (format!("path {}", p.prefix), &p.backends))
            .collect();
        if r.kind == RouteKind::Tcp {
            groups.push(("tcp".to_string(), &r.backends));
        }
        for (what, list) in groups {
            for b in list {
                match resolve_backend(st, &node_ids, b) {
                    Ok(addrs) => {
                        dials.extend(addrs.iter().cloned());
                        backends.insert(b.key(), addrs);
                    }
                    Err(e) => plan.errors.push(format!("route {rname} {what}: {e}")),
                }
                if let Backend::External { address } = b {
                    dials.push(address.clone());
                }
            }
        }
        dials.sort();
        dials.dedup();
        plan.route_dials.insert(rname.clone(), dials);
    }

    if !plan.errors.is_empty() {
        return plan;
    }
    let paths = NodePaths::default();
    match render_caddy(&RenderInput {
        gateway,
        routes: &routes,
        waf_policies: &policies,
        backends: &backends,
        paths: &paths,
    }) {
        Ok(r) => plan.rendered = Some(r),
        Err(e) => plan.errors.push(e),
    }
    plan
}

/// Resolves a backend to `ip:port` dial addresses. `external` is handled by the
/// renderer and resolves to nothing here.
fn resolve_backend(
    st: &IngressState,
    gateway_nodes: &BTreeSet<String>,
    b: &Backend,
) -> Result<Vec<String>, String> {
    match b {
        Backend::External { .. } => Ok(Vec::new()),
        Backend::Container { name, .. } => Err(format!(
            "container backend {name}: node agents do not report container addresses yet; \
             use an external backend with the container's address"
        )),
        Backend::Vm { name, port } => {
            let vm = st
                .vms
                .iter()
                .find(|v| &v.name == name)
                .ok_or_else(|| format!("VM {name} does not exist"))?;
            if vm.vm_ip.trim().is_empty() {
                return Err(format!("VM {name} has no recorded address yet"));
            }
            let members: BTreeSet<&str> = st
                .networks
                .iter()
                .filter(|n| n.name == vm.network)
                .map(|n| n.node_id.as_str())
                .collect();
            let kind = st
                .networks
                .iter()
                .find(|n| n.name == vm.network)
                .map(|n| n.network_type.as_str())
                .unwrap_or("nat");
            match kind {
                "vxlan" => {
                    if !gateway_nodes.iter().all(|n| members.contains(n.as_str())) {
                        return Err(format!(
                            "VM {name} is on vxlan network {}, which every gateway node must join; \
                             the gateway does not join overlays by itself yet",
                            vm.network
                        ));
                    }
                }
                "bridge" => {}
                _ => {
                    if !gateway_nodes.contains(&vm.node_id) {
                        return Err(format!(
                            "VM {name} is on NAT network {} of node {}, which is not a gateway node; \
                             move the VM, use a vxlan network, or add an external backend",
                            vm.network, vm.node_id
                        ));
                    }
                }
            }
            let ip = vm.vm_ip.trim();
            let host = if ip.contains(':') {
                format!("[{ip}]")
            } else {
                ip.to_string()
            };
            Ok(vec![format!("{host}:{port}")])
        }
    }
}

// ---------------------------------------------------------------- node Nix

/// The `kcore.ingress` values for one node, derived from the gateways that
/// name it.
pub fn ingress_nix_for_node(
    st: &IngressState,
    node: &NodeRow,
) -> Option<crate::nixgen::IngressNix> {
    for (gw_name, gw) in &st.gateways {
        let Some(idx) = gw
            .nodes
            .iter()
            .position(|n| n == &node.id || n == &node.hostname)
        else {
            continue;
        };
        let ip_of = |n: &NodeRow| {
            n.address
                .rsplit_once(':')
                .map(|(h, _)| h.to_string())
                .unwrap_or_else(|| n.address.clone())
        };
        let vip = gw.high_availability.as_ref().map(|ha| {
            let peers: Vec<String> = gw
                .nodes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .filter_map(|(_, n)| find_node(&st.nodes, n))
                .map(ip_of)
                .collect();
            crate::nixgen::IngressVipNix {
                address: ha.vip_address.clone(),
                interface: ha.vip_interface.clone(),
                virtual_router_id: ha.virtual_router_id,
                priority: if idx == 0 { 100 } else { 50 },
                source_address: ip_of(node),
                peers,
            }
        });
        let mut tcp_listeners = Vec::new();
        for r in st
            .routes
            .values()
            .filter(|r| &r.gateway == gw_name && r.kind == RouteKind::Tcp)
        {
            let Some(port) = r.port else { continue };
            for l in gw.listeners.iter().filter(|l| l.ports.contains(&port)) {
                let entry = (l.address.clone(), port);
                if !tcp_listeners.contains(&entry) {
                    tcp_listeners.push(entry);
                }
            }
        }
        tcp_listeners.sort();
        return Some(crate::nixgen::IngressNix {
            listen_addresses: gw.listeners.iter().map(|l| l.address.clone()).collect(),
            http3: gw.listeners.iter().any(|l| l.http3),
            acme_email: gw.tls.acme.email.clone(),
            vip,
            tcp_listeners,
            dns_credentials: gw.tls.acme.dns01.is_some(),
        });
    }
    None
}

// -------------------------------------------------------------- reconciler

pub fn spawn_ingress_reconciler(db: Database, clients: NodeClients) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) = reconcile_once(&db, &clients).await {
                warn!(%error, "ingress reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(db: &Database, clients: &NodeClients) -> Result<(), String> {
    let st = load_state(db)?;
    for name in st.gateways.keys() {
        if let Err(error) = reconcile_gateway(db, clients, &st, name).await {
            warn!(gateway = %name, %error, "ingress gateway reconcile failed");
        }
    }
    let cutoff = now_unix() - WAF_EVENT_RETENTION_SECS;
    if let Err(e) = db.prune_waf_events(cutoff) {
        debug!(error = %e, "waf event prune failed");
    }
    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NodeStatusJson {
    pub node_id: String,
    pub desired_hash: String,
    pub applied_hash: String,
    pub healthy: bool,
    pub message: String,
    pub vip_holder: String,
    pub certs: Vec<CertJson>,
    pub upstreams: Vec<UpstreamJson>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CertJson {
    pub host: String,
    pub issuer: String,
    pub not_after: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UpstreamJson {
    pub address: String,
    pub healthy: bool,
}

async fn reconcile_gateway(
    db: &Database,
    clients: &NodeClients,
    st: &IngressState,
    name: &str,
) -> Result<(), String> {
    let generation = db
        .get_ingress_gateway(name)
        .map_err(|e| e.to_string())?
        .map(|g| g.generation)
        .unwrap_or(0);
    let plan = plan_gateway(st, name);
    let Some(rendered) = plan.rendered.as_ref().filter(|_| plan.errors.is_empty()) else {
        db.upsert_ingress_gateway_status(&IngressGatewayStatusRow {
            name: name.into(),
            observed_generation: generation,
            phase: "failed".into(),
            message: plan.errors.join("; "),
            nodes_json: "[]".into(),
        })
        .map_err(|e| e.to_string())?;
        mark_routes(db, st, name, "pending", &plan.errors.join("; "), &[], &plan);
        return Ok(());
    };

    let mut statuses = Vec::new();
    for node in &plan.nodes {
        statuses.push(reconcile_node(clients, node, rendered).await);
    }
    let ready = statuses.iter().filter(|s| s.healthy).count();
    let phase = if ready == statuses.len() {
        "ready"
    } else if ready == 0 {
        "applying"
    } else {
        "degraded"
    };
    let message = statuses
        .iter()
        .filter(|s| !s.healthy)
        .map(|s| format!("{}: {}", s.node_id, s.message))
        .collect::<Vec<_>>()
        .join("; ");
    if phase == "ready" {
        info!(gateway = %name, hash = %rendered.hash, "ingress gateway ready");
    }
    db.upsert_ingress_gateway_status(&IngressGatewayStatusRow {
        name: name.into(),
        observed_generation: generation,
        phase: phase.into(),
        message,
        nodes_json: serde_json::to_string(&statuses).map_err(|e| e.to_string())?,
    })
    .map_err(|e| e.to_string())?;
    let route_phase = if phase == "ready" { "ready" } else { "pending" };
    mark_routes(db, st, name, route_phase, "", &statuses, &plan);
    Ok(())
}

async fn reconcile_node(
    clients: &NodeClients,
    node: &GatewayNode,
    rendered: &Rendered,
) -> NodeStatusJson {
    let mut out = NodeStatusJson {
        node_id: node.id.clone(),
        desired_hash: rendered.hash.clone(),
        ..Default::default()
    };
    if clients.get_admin(&node.address).is_none() {
        if let Err(e) = clients.connect(&node.address).await {
            out.message = format!("cannot reach node: {e}");
            return out;
        }
    }
    let Some(mut admin) = clients.get_admin(&node.address) else {
        out.message = "no connection to node".into();
        return out;
    };
    let status = match admin
        .get_ingress_status(node_proto::GetIngressStatusRequest {})
        .await
    {
        Ok(r) => r.into_inner(),
        Err(e) => {
            out.message = format!("status: {}", e.message());
            return out;
        }
    };
    out.applied_hash = status.applied_hash.clone();
    if !status.running {
        out.message = if status.message.is_empty() {
            "ingress service is not running; waiting for the node to enable it".into()
        } else {
            status.message.clone()
        };
        return out;
    }
    if status.applied_hash != rendered.hash {
        match admin
            .apply_ingress_config(node_proto::ApplyIngressConfigRequest {
                config_json: rendered.json.clone(),
                hash: rendered.hash.clone(),
            })
            .await
        {
            Ok(r) => {
                let r = r.into_inner();
                if !r.success {
                    out.message = format!("apply refused: {}", r.message);
                    return out;
                }
                out.applied_hash = r.applied_hash;
            }
            Err(e) => {
                out.message = format!("apply: {}", e.message());
                return out;
            }
        }
    }
    out.healthy = out.applied_hash == rendered.hash;
    out.vip_holder = status.vip_holder;
    out.certs = status
        .certs
        .into_iter()
        .map(|c| CertJson {
            host: c.host,
            issuer: c.issuer,
            not_after: c.not_after,
        })
        .collect();
    out.upstreams = status
        .upstreams
        .into_iter()
        .map(|u| UpstreamJson {
            address: u.address,
            healthy: u.healthy,
        })
        .collect();
    out
}

#[derive(serde::Serialize)]
struct RouteStatusJson<'a> {
    certs: Vec<&'a CertJson>,
    upstreams: Vec<&'a UpstreamJson>,
}

fn mark_routes(
    db: &Database,
    st: &IngressState,
    gateway: &str,
    phase: &str,
    message: &str,
    statuses: &[NodeStatusJson],
    plan: &Plan,
) {
    for (rname, route) in st.routes.iter().filter(|(_, r)| r.gateway == gateway) {
        let dials: BTreeSet<&String> = plan
            .route_dials
            .get(rname)
            .map(|d| d.iter().collect())
            .unwrap_or_default();
        let hosts: BTreeSet<String> = route.hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
        let mut certs = Vec::new();
        let mut ups = Vec::new();
        for s in statuses {
            certs.extend(
                s.certs
                    .iter()
                    .filter(|c| hosts.contains(&c.host.to_ascii_lowercase())),
            );
            ups.extend(s.upstreams.iter().filter(|u| dials.contains(&u.address)));
        }
        let degraded = ups.iter().any(|u| !u.healthy);
        let phase = if phase == "ready" && degraded {
            "degraded"
        } else {
            phase
        };
        let status_json = serde_json::to_string(&RouteStatusJson {
            certs,
            upstreams: ups,
        })
        .unwrap_or_else(|_| "{}".into());
        if let Err(e) = db.upsert_ingress_route_status(&IngressRouteStatusRow {
            name: rname.clone(),
            phase: phase.into(),
            message: message.into(),
            status_json,
        }) {
            debug!(route = %rname, error = %e, "route status write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, addr: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            hostname: id.into(),
            address: addr.into(),
            cpu_cores: 4,
            memory_bytes: 0,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: String::new(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: String::new(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            cert_expiry_days: 0,
            luks_method: String::new(),
            dc_id: String::new(),
        }
    }

    fn vm(name: &str, node_id: &str, network: &str, ip: &str) -> VmRow {
        VmRow {
            id: format!("id-{name}"),
            name: name.into(),
            cpu: 1,
            memory_bytes: 0,
            image_path: String::new(),
            image_url: String::new(),
            image_sha256: String::new(),
            image_format: String::new(),
            image_size: 0,
            network: network.into(),
            auto_start: true,
            node_id: node_id.into(),
            created_at: String::new(),
            runtime_state: "running".into(),
            cloud_init_user_data: String::new(),
            storage_backend: String::new(),
            storage_size_bytes: 0,
            vm_ip: ip.into(),
            pci_devices: String::new(),
            labels: String::new(),
        }
    }

    fn network(name: &str, node_id: &str, kind: &str) -> NetworkRow {
        NetworkRow {
            name: name.into(),
            external_ip: String::new(),
            gateway_ip: String::new(),
            internal_netmask: String::new(),
            node_id: node_id.into(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: kind.into(),
            enable_outbound_nat: true,
            vni: 0,
            next_ip: 0,
        }
    }

    fn state() -> IngressState {
        let mut st = IngressState {
            nodes: vec![
                node("node-1", "10.0.0.1:9091"),
                node("node-2", "10.0.0.2:9091"),
            ],
            vms: vec![vm("web", "node-1", "lab", "10.240.0.5")],
            networks: vec![network("lab", "node-1", "nat")],
            ..Default::default()
        };
        st.gateways.insert(
            "edge".into(),
            ingress_spec::parse_gateway(
                r#"{"nodes":["node-1"],
                    "listeners":[{"address":"203.0.113.10","ports":[80,443]}],
                    "tls":{"acme":{"email":"ops@example.com"}}}"#,
            )
            .unwrap(),
        );
        st.routes.insert(
            "shop".into(),
            ingress_spec::parse_route(
                r#"{"gateway":"edge","hosts":["shop.example.com"],
                    "paths":[{"prefix":"/","backends":[{"kind":"vm","name":"web","port":8080}]}]}"#,
            )
            .unwrap(),
        );
        st
    }

    #[test]
    fn plans_a_nat_vm_on_the_gateway_node() {
        let plan = plan_gateway(&state(), "edge");
        assert!(plan.errors.is_empty(), "{:?}", plan.errors);
        let r = plan.rendered.unwrap();
        assert!(r.json.contains("10.240.0.5:8080"));
        assert_eq!(
            plan.nodes,
            vec![GatewayNode {
                id: "node-1".into(),
                address: "10.0.0.1:9091".into()
            }]
        );
        assert!(plan.warnings.iter().any(|w| w.contains("no WAF policy")));
        assert_eq!(
            plan.route_dials["shop"],
            vec!["10.240.0.5:8080".to_string()]
        );
    }

    #[test]
    fn nat_vm_on_another_node_is_refused_with_the_fix() {
        let mut st = state();
        st.vms[0].node_id = "node-2".into();
        let plan = plan_gateway(&st, "edge");
        assert!(plan.rendered.is_none());
        assert!(
            plan.errors.iter().any(|e| e.contains("move the VM")),
            "{:?}",
            plan.errors
        );
    }

    #[test]
    fn vxlan_needs_every_gateway_node_as_member() {
        let mut st = state();
        st.networks = vec![network("lab", "node-2", "vxlan")];
        assert!(plan_gateway(&st, "edge")
            .errors
            .iter()
            .any(|e| e.contains("vxlan")));
        st.networks.push(network("lab", "node-1", "vxlan"));
        assert!(plan_gateway(&st, "edge").errors.is_empty());
    }

    #[test]
    fn missing_pieces_are_reported_together() {
        let mut st = state();
        st.vms.clear();
        st.gateways.get_mut("edge").unwrap().default_waf_policy = Some("crs".into());
        st.gateways.get_mut("edge").unwrap().nodes = vec!["node-9".into()];
        let plan = plan_gateway(&st, "edge");
        let all = plan.errors.join("\n");
        assert!(all.contains("node node-9 is not registered"), "{all}");
        assert!(all.contains("WafPolicy crs does not exist"), "{all}");
        assert!(all.contains("VM web does not exist"), "{all}");
    }

    #[test]
    fn container_backend_is_refused_until_nodes_report_addresses() {
        let mut st = state();
        st.routes.get_mut("shop").unwrap().paths[0].backends = vec![Backend::Container {
            name: "php".into(),
            port: 80,
        }];
        let plan = plan_gateway(&st, "edge");
        assert!(plan.errors.iter().any(|e| e.contains("external backend")));
    }

    #[test]
    fn nix_values_follow_node_order_for_vrrp() {
        let mut st = state();
        let gw = st.gateways.get_mut("edge").unwrap();
        gw.nodes = vec!["node-1".into(), "node-2".into()];
        gw.high_availability = Some(ingress_spec::HighAvailability {
            vip_address: "203.0.113.10".into(),
            vip_interface: "eno1".into(),
            virtual_router_id: 61,
        });
        let first = ingress_nix_for_node(&st, &st.nodes[0]).unwrap();
        let second = ingress_nix_for_node(&st, &st.nodes[1]).unwrap();
        let (v1, v2) = (first.vip.unwrap(), second.vip.unwrap());
        assert_eq!((v1.priority, v2.priority), (100, 50));
        assert_eq!(v1.source_address, "10.0.0.1");
        assert_eq!(v1.peers, vec!["10.0.0.2".to_string()]);
        assert_eq!(v2.peers, vec!["10.0.0.1".to_string()]);
        let other = node("node-3", "10.0.0.3:9091");
        assert!(ingress_nix_for_node(&st, &other).is_none());
    }

    #[test]
    fn overlay_documents_replace_stored_ones() {
        let mut st = state();
        apply_document(
            &mut st,
            "IngressRoute",
            "shop",
            r#"{"gateway":"edge","hosts":["new.example.com"],"paths":[{"prefix":"/","backends":[{"kind":"external","address":"192.0.2.9:80"}]}]}"#,
        )
        .unwrap();
        let plan = plan_gateway(&st, "edge");
        assert!(plan.errors.is_empty(), "{:?}", plan.errors);
        assert!(plan.rendered.unwrap().json.contains("new.example.com"));
        assert!(apply_document(&mut st, "Nope", "x", "{}").is_err());
    }
}
