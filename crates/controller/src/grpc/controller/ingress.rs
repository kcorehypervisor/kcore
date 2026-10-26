//! Handlers for the ingress gateway, route, WAF policy and WAF event RPCs.
//!
//! A child of `controller.rs` so it can reach `ControllerService`'s private
//! state; the trait impl there only delegates here.

use tonic::{Request, Response, Status};
use tracing::warn;

use super::ControllerService;
use crate::auth::{self, OperatorRole, CN_NODE_PREFIX};
use crate::controller_proto as pb;
use crate::db::{
    IngressGatewayRow, IngressGatewayStatusRow, IngressRouteRow, IngressRouteStatusRow,
    WafEventFilter, WafEventRow, WafPolicyRow,
};
use crate::grpc::helpers::parse_datetime_to_timestamp;
use crate::grpc::validation::validate_network_name;
use crate::ingress_reconciler::{self, NodeStatusJson};
use crate::ingress_render::render_waf_directives;
use crate::ingress_spec::{self, GatewaySpec, RouteSpec, WafPolicySpec};

/// Max events accepted from a node in one report.
const MAX_EVENTS_PER_REPORT: usize = 1000;

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

fn apply_action(existing: Option<&str>, new_json: &str) -> i32 {
    match existing {
        Some(old) if old == new_json => pb::ApplyAction::Unchanged as i32,
        Some(_) => pb::ApplyAction::Updated as i32,
        None => pb::ApplyAction::Created as i32,
    }
}

/// Top-level spec keys whose value differs between two stored documents.
fn changed_fields(old: Option<&str>, new: &str) -> Vec<String> {
    let new: serde_json::Value = serde_json::from_str(new).unwrap_or_default();
    let Some(old) = old.and_then(|o| serde_json::from_str::<serde_json::Value>(o).ok()) else {
        return Vec::new();
    };
    let (Some(o), Some(n)) = (old.as_object(), new.as_object()) else {
        return Vec::new();
    };
    let mut keys: std::collections::BTreeSet<&String> = o.keys().collect();
    keys.extend(n.keys());
    keys.into_iter()
        .filter(|k| o.get(*k) != n.get(*k))
        .cloned()
        .collect()
}

fn gateway_to_proto(
    row: &IngressGatewayRow,
    status: Option<IngressGatewayStatusRow>,
) -> pb::IngressGateway {
    let status = status.map(|s| {
        let nodes: Vec<NodeStatusJson> = serde_json::from_str(&s.nodes_json).unwrap_or_default();
        pb::IngressGatewayStatus {
            observed_generation: s.observed_generation,
            phase: s.phase,
            message: s.message,
            nodes: nodes
                .into_iter()
                .map(|n| pb::IngressNodeStatus {
                    node_id: n.node_id,
                    desired_hash: n.desired_hash,
                    applied_hash: n.applied_hash,
                    healthy: n.healthy,
                    message: n.message,
                    vip_holder: n.vip_holder,
                    certs: n
                        .certs
                        .into_iter()
                        .map(|c| pb::IngressCertStatus {
                            host: c.host,
                            issuer: c.issuer,
                            not_after: c.not_after,
                        })
                        .collect(),
                    upstreams: n
                        .upstreams
                        .into_iter()
                        .map(|u| pb::IngressUpstreamHealth {
                            address: u.address,
                            healthy: u.healthy,
                        })
                        .collect(),
                })
                .collect(),
        }
    });
    pb::IngressGateway {
        name: row.name.clone(),
        generation: row.generation,
        spec_json: row.spec_json.clone(),
        status,
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
    }
}

fn route_to_proto(
    row: &IngressRouteRow,
    status: Option<IngressRouteStatusRow>,
) -> pb::IngressRoute {
    let status = status.map(|s| {
        let v: serde_json::Value = serde_json::from_str(&s.status_json).unwrap_or_default();
        let certs = v["certs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| pb::IngressCertStatus {
                        host: c["host"].as_str().unwrap_or_default().into(),
                        issuer: c["issuer"].as_str().unwrap_or_default().into(),
                        not_after: c["not_after"].as_str().unwrap_or_default().into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let upstreams = v["upstreams"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|u| pb::IngressUpstreamHealth {
                        address: u["address"].as_str().unwrap_or_default().into(),
                        healthy: u["healthy"].as_bool().unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        pb::IngressRouteStatus {
            phase: s.phase,
            message: s.message,
            certs,
            upstreams,
            last_waf_block_at: String::new(),
        }
    });
    pb::IngressRoute {
        name: row.name.clone(),
        generation: row.generation,
        gateway: row.gateway.clone(),
        spec_json: row.spec_json.clone(),
        owner: row.owner.clone(),
        status,
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
    }
}

fn policy_to_proto(row: &WafPolicyRow) -> pb::WafPolicy {
    pb::WafPolicy {
        name: row.name.clone(),
        generation: row.generation,
        spec_json: row.spec_json.clone(),
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
    }
}

fn event_to_proto(e: WafEventRow) -> pb::WafEvent {
    pb::WafEvent {
        id: e.id,
        gateway: e.gateway,
        route: e.route,
        node_id: e.node_id,
        rule_id: e.rule_id,
        severity: e.severity,
        client_ip: e.client_ip,
        host: e.host,
        uri: e.uri,
        action: e.action,
        tx_id: e.tx_id,
        raw_json: e.raw_json,
        created_at: Some(prost_types::Timestamp {
            seconds: e.created_at,
            nanos: 0,
        }),
    }
}

fn canonical<T: serde::Serialize>(spec: &T) -> Result<String, Status> {
    serde_json::to_string(spec).map_err(|e| Status::internal(format!("encode spec: {e}")))
}

fn required_name(name: &str) -> Result<String, Status> {
    validate_network_name(name)
}

impl ControllerService {
    /// Pushes a node's Nix configuration so its `kcore.ingress` block follows
    /// the gateways that name it. A failure is logged: the reconciler keeps
    /// retrying the routing table, and the next push repeats this one.
    async fn push_ingress_nodes(&self, node_names: &[String]) {
        let Ok(nodes) = self.db.list_nodes() else {
            return;
        };
        for name in node_names {
            let Some(node) = nodes.iter().find(|n| &n.id == name || &n.hostname == name) else {
                continue;
            };
            if let Err(e) = self.push_config_to_node(node).await {
                warn!(node = %node.id, error = %e, "ingress: pushing node configuration failed");
            }
        }
    }

    fn ingress_state(&self) -> Result<ingress_reconciler::IngressState, Status> {
        ingress_reconciler::load_state(&self.db).map_err(Status::internal)
    }

    pub(super) async fn ingress_plan(
        &self,
        request: Request<pb::PlanIngressRequest>,
    ) -> Result<Response<pb::PlanIngressResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let mut st = self.ingress_state()?;
        let mut errors = Vec::new();
        for d in &req.documents {
            if let Err(e) =
                ingress_reconciler::apply_document(&mut st, &d.kind, &d.name, &d.spec_json)
            {
                errors.push(format!("{} {}: {e}", d.kind, d.name));
            }
        }
        let gateway = req.gateway.trim().to_string();
        if gateway.is_empty() {
            errors.push("gateway is required".into());
        }
        if !errors.is_empty() {
            return Ok(Response::new(pb::PlanIngressResponse {
                errors,
                ..Default::default()
            }));
        }
        let plan = ingress_reconciler::plan_gateway(&st, &gateway);
        let applied: Vec<NodeStatusJson> = self
            .db
            .get_ingress_gateway_status(&gateway)
            .map_err(internal)?
            .and_then(|s| serde_json::from_str(&s.nodes_json).ok())
            .unwrap_or_default();
        let nodes = plan
            .rendered
            .as_ref()
            .map(|r| {
                plan.nodes
                    .iter()
                    .map(|n| {
                        let applied_hash = applied
                            .iter()
                            .find(|a| a.node_id == n.id)
                            .map(|a| a.applied_hash.clone())
                            .unwrap_or_default();
                        pb::PlanIngressNode {
                            node_id: n.id.clone(),
                            caddy_json: r.json.clone(),
                            hash: r.hash.clone(),
                            changed: applied_hash != r.hash,
                            applied_hash,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Response::new(pb::PlanIngressResponse {
            errors: plan.errors,
            warnings: plan.warnings,
            nodes,
        }))
    }

    // ------------------------------------------------------------- gateways

    pub(super) async fn ingress_create_gateway(
        &self,
        request: Request<pb::CreateIngressGatewayRequest>,
    ) -> Result<Response<pb::CreateIngressGatewayResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = required_name(&req.name)?;
        let spec: GatewaySpec =
            ingress_spec::parse_gateway(&req.spec_json).map_err(Status::invalid_argument)?;
        ingress_spec::validate_gateway(&spec).map_err(Status::invalid_argument)?;
        let spec_json = canonical(&spec)?;

        let st = self.ingress_state()?;
        let nodes = self.db.list_nodes().map_err(internal)?;
        for n in &spec.nodes {
            if !nodes.iter().any(|row| &row.id == n || &row.hostname == n) {
                return Err(Status::failed_precondition(format!(
                    "node {n} is not registered"
                )));
            }
        }
        // One listener address and port is bound by one gateway.
        for (other_name, other) in &st.gateways {
            if other_name == &name {
                continue;
            }
            let shares_node = other.nodes.iter().any(|n| spec.nodes.contains(n));
            if !shares_node {
                continue;
            }
            for l in &spec.listeners {
                for ol in &other.listeners {
                    if l.address == ol.address && l.ports.iter().any(|p| ol.ports.contains(p)) {
                        return Err(Status::failed_precondition(format!(
                            "listener {} is already bound by gateway {other_name} on a shared node",
                            l.address
                        )));
                    }
                }
            }
        }

        let existing = self.db.get_ingress_gateway(&name).map_err(internal)?;
        let action = apply_action(existing.as_ref().map(|e| e.spec_json.as_str()), &spec_json);
        let changed = changed_fields(existing.as_ref().map(|e| e.spec_json.as_str()), &spec_json);
        let previous_nodes: Vec<String> = existing
            .as_ref()
            .and_then(|e| ingress_spec::parse_gateway(&e.spec_json).ok())
            .map(|g| g.nodes)
            .unwrap_or_default();
        let row = if action == pb::ApplyAction::Unchanged as i32 {
            existing.expect("unchanged implies existing")
        } else {
            let generation = existing.as_ref().map(|e| e.generation + 1).unwrap_or(1);
            let row = IngressGatewayRow {
                name: name.clone(),
                generation,
                spec_json,
                created_at: existing.map(|e| e.created_at).unwrap_or_default(),
                updated_at: String::new(),
            };
            self.db.upsert_ingress_gateway(&row).map_err(internal)?;
            self.db
                .upsert_ingress_gateway_status(&IngressGatewayStatusRow {
                    name: name.clone(),
                    observed_generation: 0,
                    phase: "pending".into(),
                    message: String::new(),
                    nodes_json: "[]".into(),
                })
                .map_err(internal)?;
            let mut to_push = spec.nodes.clone();
            to_push.extend(previous_nodes);
            to_push.sort();
            to_push.dedup();
            self.push_ingress_nodes(&to_push).await;
            self.db
                .get_ingress_gateway(&name)
                .map_err(internal)?
                .unwrap_or(row)
        };
        self.record_audit(
            &actor,
            "CreateIngressGateway",
            &name,
            format!("action={action}"),
        );
        let status = self
            .db
            .get_ingress_gateway_status(&name)
            .map_err(internal)?;
        Ok(Response::new(pb::CreateIngressGatewayResponse {
            success: true,
            gateway: Some(gateway_to_proto(&row, status)),
            action,
            changed_fields: changed,
        }))
    }

    pub(super) async fn ingress_get_gateway(
        &self,
        request: Request<pb::GetIngressGatewayRequest>,
    ) -> Result<Response<pb::GetIngressGatewayResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name.trim().to_string();
        let row = self
            .db
            .get_ingress_gateway(&name)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("IngressGateway '{name}' not found")))?;
        let status = self
            .db
            .get_ingress_gateway_status(&name)
            .map_err(internal)?;
        Ok(Response::new(pb::GetIngressGatewayResponse {
            gateway: Some(gateway_to_proto(&row, status)),
        }))
    }

    pub(super) async fn ingress_list_gateways(
        &self,
        request: Request<pb::ListIngressGatewaysRequest>,
    ) -> Result<Response<pb::ListIngressGatewaysResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let mut out = Vec::new();
        for row in self.db.list_ingress_gateways().map_err(internal)? {
            let status = self
                .db
                .get_ingress_gateway_status(&row.name)
                .map_err(internal)?;
            out.push(gateway_to_proto(&row, status));
        }
        Ok(Response::new(pb::ListIngressGatewaysResponse {
            gateways: out,
        }))
    }

    pub(super) async fn ingress_delete_gateway(
        &self,
        request: Request<pb::DeleteIngressGatewayRequest>,
    ) -> Result<Response<pb::DeleteIngressGatewayResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = required_name(&request.into_inner().name)?;
        let routes = self.db.list_ingress_routes(&name).map_err(internal)?;
        if !routes.is_empty() {
            return Err(Status::failed_precondition(format!(
                "gateway {name} still has routes ({}); delete them first",
                routes
                    .iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let nodes = self
            .db
            .get_ingress_gateway(&name)
            .map_err(internal)?
            .and_then(|g| ingress_spec::parse_gateway(&g.spec_json).ok())
            .map(|g| g.nodes)
            .unwrap_or_default();
        let success = self.db.delete_ingress_gateway(&name).map_err(internal)?;
        if success {
            self.record_audit(&actor, "DeleteIngressGateway", &name, "");
            self.push_ingress_nodes(&nodes).await;
        }
        Ok(Response::new(pb::DeleteIngressGatewayResponse { success }))
    }

    // --------------------------------------------------------------- routes

    pub(super) async fn ingress_create_route(
        &self,
        request: Request<pb::CreateIngressRouteRequest>,
    ) -> Result<Response<pb::CreateIngressRouteResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = required_name(&req.name)?;
        let spec: RouteSpec =
            ingress_spec::parse_route(&req.spec_json).map_err(Status::invalid_argument)?;
        ingress_spec::validate_route(&spec).map_err(Status::invalid_argument)?;
        let spec_json = canonical(&spec)?;
        if self
            .db
            .get_ingress_gateway(&spec.gateway)
            .map_err(internal)?
            .is_none()
        {
            return Err(Status::failed_precondition(format!(
                "IngressGateway '{}' does not exist",
                spec.gateway
            )));
        }
        let existing = self.db.get_ingress_route(&name).map_err(internal)?;
        if existing.as_ref().is_some_and(|e| !e.owner.is_empty()) {
            return Err(Status::failed_precondition(format!(
                "route {name} is managed by {}; change the source object instead",
                existing.map(|e| e.owner).unwrap_or_default()
            )));
        }

        // Surface this route's own problems now rather than at the next tick.
        let mut st = self.ingress_state()?;
        st.routes.insert(name.clone(), spec.clone());
        let plan = ingress_reconciler::plan_gateway(&st, &spec.gateway);
        let own: Vec<&String> = plan
            .errors
            .iter()
            .filter(|e| e.starts_with(&format!("route {name}")) || e.contains("is used by routes"))
            .collect();
        if !own.is_empty() {
            return Err(Status::failed_precondition(
                own.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }

        let action = apply_action(existing.as_ref().map(|e| e.spec_json.as_str()), &spec_json);
        let changed = changed_fields(existing.as_ref().map(|e| e.spec_json.as_str()), &spec_json);
        let row = if action == pb::ApplyAction::Unchanged as i32 {
            existing.expect("unchanged implies existing")
        } else {
            let row = IngressRouteRow {
                name: name.clone(),
                gateway: spec.gateway.clone(),
                generation: existing.as_ref().map(|e| e.generation + 1).unwrap_or(1),
                spec_json,
                owner: String::new(),
                created_at: existing.map(|e| e.created_at).unwrap_or_default(),
                updated_at: String::new(),
            };
            self.db.upsert_ingress_route(&row).map_err(internal)?;
            self.db
                .upsert_ingress_route_status(&IngressRouteStatusRow {
                    name: name.clone(),
                    phase: "pending".into(),
                    ..Default::default()
                })
                .map_err(internal)?;
            self.db
                .get_ingress_route(&name)
                .map_err(internal)?
                .unwrap_or(row)
        };
        self.record_audit(
            &actor,
            "CreateIngressRoute",
            &name,
            format!("action={action}"),
        );
        let status = self.db.get_ingress_route_status(&name).map_err(internal)?;
        Ok(Response::new(pb::CreateIngressRouteResponse {
            success: true,
            route: Some(route_to_proto(&row, status)),
            action,
            changed_fields: changed,
        }))
    }

    pub(super) async fn ingress_get_route(
        &self,
        request: Request<pb::GetIngressRouteRequest>,
    ) -> Result<Response<pb::GetIngressRouteResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name.trim().to_string();
        let row = self
            .db
            .get_ingress_route(&name)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("IngressRoute '{name}' not found")))?;
        let status = self.db.get_ingress_route_status(&name).map_err(internal)?;
        Ok(Response::new(pb::GetIngressRouteResponse {
            route: Some(route_to_proto(&row, status)),
        }))
    }

    pub(super) async fn ingress_list_routes(
        &self,
        request: Request<pb::ListIngressRoutesRequest>,
    ) -> Result<Response<pb::ListIngressRoutesResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let gateway = request.into_inner().gateway.trim().to_string();
        let mut out = Vec::new();
        for row in self.db.list_ingress_routes(&gateway).map_err(internal)? {
            let status = self
                .db
                .get_ingress_route_status(&row.name)
                .map_err(internal)?;
            out.push(route_to_proto(&row, status));
        }
        Ok(Response::new(pb::ListIngressRoutesResponse { routes: out }))
    }

    pub(super) async fn ingress_delete_route(
        &self,
        request: Request<pb::DeleteIngressRouteRequest>,
    ) -> Result<Response<pb::DeleteIngressRouteResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = required_name(&request.into_inner().name)?;
        if let Some(row) = self.db.get_ingress_route(&name).map_err(internal)? {
            if !row.owner.is_empty() {
                return Err(Status::failed_precondition(format!(
                    "route {name} is managed by {}; delete the source object instead",
                    row.owner
                )));
            }
        }
        let success = self.db.delete_ingress_route(&name).map_err(internal)?;
        if success {
            self.record_audit(&actor, "DeleteIngressRoute", &name, "");
        }
        Ok(Response::new(pb::DeleteIngressRouteResponse { success }))
    }

    pub(super) async fn ingress_set_route_waf_mode(
        &self,
        request: Request<pb::SetIngressRouteWafModeRequest>,
    ) -> Result<Response<pb::SetIngressRouteWafModeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = required_name(&req.name)?;
        let mode = req.mode.trim().to_string();
        if !matches!(mode.as_str(), "detect" | "block" | "off") {
            return Err(Status::invalid_argument(
                "mode must be detect, block or off",
            ));
        }
        let row = self
            .db
            .get_ingress_route(&name)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("IngressRoute '{name}' not found")))?;
        let mut spec = ingress_spec::parse_route(&row.spec_json).map_err(Status::internal)?;
        spec.waf.mode = Some(mode.clone());
        let updated = IngressRouteRow {
            generation: row.generation + 1,
            spec_json: canonical(&spec)?,
            ..row
        };
        self.db.upsert_ingress_route(&updated).map_err(internal)?;
        self.record_audit(
            &actor,
            "SetIngressRouteWafMode",
            &name,
            format!("mode={mode}"),
        );
        let status = self.db.get_ingress_route_status(&name).map_err(internal)?;
        Ok(Response::new(pb::SetIngressRouteWafModeResponse {
            success: true,
            route: Some(route_to_proto(&updated, status)),
        }))
    }

    // ---------------------------------------------------------- WAF policies

    pub(super) async fn ingress_create_waf_policy(
        &self,
        request: Request<pb::CreateWafPolicyRequest>,
    ) -> Result<Response<pb::CreateWafPolicyResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = required_name(&req.name)?;
        let spec: WafPolicySpec =
            ingress_spec::parse_waf_policy(&req.spec_json).map_err(Status::invalid_argument)?;
        ingress_spec::validate_waf_policy(&spec).map_err(Status::invalid_argument)?;
        // Rendering is pure; do it so a policy that cannot render never lands.
        let _ = render_waf_directives(&spec, None, &[], "/dev/null");
        let spec_json = canonical(&spec)?;
        let existing = self.db.get_waf_policy(&name).map_err(internal)?;
        let action = apply_action(existing.as_ref().map(|e| e.spec_json.as_str()), &spec_json);
        let changed = changed_fields(existing.as_ref().map(|e| e.spec_json.as_str()), &spec_json);
        let row = if action == pb::ApplyAction::Unchanged as i32 {
            existing.expect("unchanged implies existing")
        } else {
            let row = WafPolicyRow {
                name: name.clone(),
                generation: existing.as_ref().map(|e| e.generation + 1).unwrap_or(1),
                spec_json,
                created_at: existing.map(|e| e.created_at).unwrap_or_default(),
                updated_at: String::new(),
            };
            self.db.upsert_waf_policy(&row).map_err(internal)?;
            self.db
                .get_waf_policy(&name)
                .map_err(internal)?
                .unwrap_or(row)
        };
        self.record_audit(&actor, "CreateWafPolicy", &name, format!("action={action}"));
        Ok(Response::new(pb::CreateWafPolicyResponse {
            success: true,
            policy: Some(policy_to_proto(&row)),
            action,
            changed_fields: changed,
        }))
    }

    pub(super) async fn ingress_get_waf_policy(
        &self,
        request: Request<pb::GetWafPolicyRequest>,
    ) -> Result<Response<pb::GetWafPolicyResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name.trim().to_string();
        let row = self
            .db
            .get_waf_policy(&name)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("WafPolicy '{name}' not found")))?;
        Ok(Response::new(pb::GetWafPolicyResponse {
            policy: Some(policy_to_proto(&row)),
        }))
    }

    pub(super) async fn ingress_list_waf_policies(
        &self,
        request: Request<pb::ListWafPoliciesRequest>,
    ) -> Result<Response<pb::ListWafPoliciesResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let policies = self
            .db
            .list_waf_policies()
            .map_err(internal)?
            .iter()
            .map(policy_to_proto)
            .collect();
        Ok(Response::new(pb::ListWafPoliciesResponse { policies }))
    }

    pub(super) async fn ingress_delete_waf_policy(
        &self,
        request: Request<pb::DeleteWafPolicyRequest>,
    ) -> Result<Response<pb::DeleteWafPolicyResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = required_name(&request.into_inner().name)?;
        let st = self.ingress_state()?;
        let users: Vec<String> = st
            .gateways
            .iter()
            .filter(|(_, g)| g.default_waf_policy.as_deref() == Some(name.as_str()))
            .map(|(n, _)| format!("gateway {n}"))
            .chain(
                st.routes
                    .iter()
                    .filter(|(_, r)| r.waf.policy.as_deref() == Some(name.as_str()))
                    .map(|(n, _)| format!("route {n}")),
            )
            .collect();
        if !users.is_empty() {
            return Err(Status::failed_precondition(format!(
                "WafPolicy {name} is used by {}",
                users.join(", ")
            )));
        }
        let success = self.db.delete_waf_policy(&name).map_err(internal)?;
        if success {
            self.record_audit(&actor, "DeleteWafPolicy", &name, "");
        }
        Ok(Response::new(pb::DeleteWafPolicyResponse { success }))
    }

    // ------------------------------------------------------------ WAF events

    pub(super) async fn ingress_list_waf_events(
        &self,
        request: Request<pb::ListWafEventsRequest>,
    ) -> Result<Response<pb::ListWafEventsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let r = request.into_inner();
        let events = self
            .db
            .list_waf_events(&WafEventFilter {
                gateway: r.gateway.trim().into(),
                route: r.route.trim().into(),
                rule_id: r.rule_id,
                client_ip: r.client_ip.trim().into(),
                since_unix: r.since_unix,
                limit: i64::from(r.limit),
            })
            .map_err(internal)?
            .into_iter()
            .map(event_to_proto)
            .collect();
        Ok(Response::new(pb::ListWafEventsResponse { events }))
    }

    pub(super) async fn ingress_report_waf_events(
        &self,
        request: Request<pb::ReportWafEventsRequest>,
    ) -> Result<Response<pb::ReportWafEventsResponse>, Status> {
        auth::require_peer(&request, &[CN_NODE_PREFIX])?;
        let req = request.into_inner();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // The agent only sees the Host header; the controller knows which route
        // owns it (and so which gateway), and which gateway the node belongs to.
        let st = self.ingress_state()?;
        let by_host: std::collections::HashMap<String, (String, String)> = st
            .routes
            .iter()
            .flat_map(|(name, r)| {
                r.hosts
                    .iter()
                    .map(move |h| (h.to_ascii_lowercase(), (name.clone(), r.gateway.clone())))
            })
            .collect();
        let node_gateway = st
            .gateways
            .iter()
            .find(|(_, g)| g.nodes.contains(&req.node_id))
            .map(|(n, _)| n.clone())
            .unwrap_or_default();
        let rows: Vec<WafEventRow> = req
            .events
            .into_iter()
            .take(MAX_EVENTS_PER_REPORT)
            .map(|e| {
                let (route, gateway) = by_host
                    .get(&e.host.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_else(|| {
                        let gw = if req.gateway.is_empty() {
                            node_gateway.clone()
                        } else {
                            req.gateway.clone()
                        };
                        (e.route.clone(), gw)
                    });
                (route, gateway, e)
            })
            .map(|(route, gateway, e)| WafEventRow {
                id: 0,
                gateway,
                route,
                node_id: req.node_id.clone(),
                rule_id: e.rule_id,
                severity: e.severity,
                client_ip: e.client_ip,
                host: e.host,
                uri: e.uri.chars().take(2048).collect(),
                action: if e.action == "blocked" {
                    "blocked".into()
                } else {
                    "detected".into()
                },
                tx_id: e.tx_id,
                raw_json: e.raw_json.chars().take(16_384).collect(),
                created_at: e
                    .created_at
                    .map(|t| t.seconds)
                    .filter(|s| *s > 0)
                    .unwrap_or(now),
            })
            .collect();
        let accepted = self.db.insert_waf_events(&rows).map_err(internal)?;
        Ok(Response::new(pb::ReportWafEventsResponse {
            accepted: accepted as i32,
        }))
    }

    pub(super) async fn ingress_set_secret(
        &self,
        request: Request<pb::SetIngressSecretRequest>,
    ) -> Result<Response<pb::SetIngressSecretResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = req.name.trim().to_string();
        // Only the DNS-01 credentials file exists today; refusing other names
        // keeps this from becoming an arbitrary root file writer.
        if name != ingress_spec::DNS_SECRET_NAME {
            return Err(Status::invalid_argument(format!(
                "secret name must be {}",
                ingress_spec::DNS_SECRET_NAME
            )));
        }
        if req.content.is_empty() || req.content.len() > 16 * 1024 {
            return Err(Status::invalid_argument("secret must be 1 byte to 16 KiB"));
        }
        let gateway = req.gateway.trim().to_string();
        let row = self
            .db
            .get_ingress_gateway(&gateway)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("IngressGateway '{gateway}' not found")))?;
        let spec = ingress_spec::parse_gateway(&row.spec_json).map_err(Status::internal)?;
        let nodes = self.db.list_nodes().map_err(internal)?;
        let mut written = Vec::new();
        for n in &spec.nodes {
            let node = nodes
                .iter()
                .find(|row| &row.id == n || &row.hostname == n)
                .ok_or_else(|| {
                    Status::failed_precondition(format!("node {n} is not registered"))
                })?;
            let mut admin = self.ensure_admin_client_for_node(node).await?;
            admin
                .write_ingress_secret(crate::node_proto::WriteIngressSecretRequest {
                    name: name.clone(),
                    content: req.content.clone(),
                })
                .await
                .map_err(|e| {
                    Status::unavailable(format!(
                        "writing the secret on {}: {}",
                        node.id,
                        e.message()
                    ))
                })?;
            written.push(node.id.clone());
        }
        // The content is never logged or audited, only the fact and the targets.
        self.record_audit(
            &actor,
            "SetIngressSecret",
            &gateway,
            format!("name={name} nodes={}", written.join(",")),
        );
        Ok(Response::new(pb::SetIngressSecretResponse {
            success: true,
            nodes: written,
        }))
    }
}
