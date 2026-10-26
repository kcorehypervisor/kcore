//! `kctl` commands for the ingress gateway: `IngressGateway`, `IngressRoute`
//! and `WafPolicy` resources, `kctl ingress plan`, WAF events and tuning.
//!
//! The manifests are the camelCase documents from
//! `docs/ingress-waf-load-balancer-plan.md`. The spec is forwarded to the
//! controller as JSON; the controller owns parsing and validation.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::apply_summary::render_apply_summary;
use crate::client::{self, controller_proto as pb};
use crate::config::ConnectionInfo;

pub const KIND_GATEWAY: &str = "IngressGateway";
pub const KIND_ROUTE: &str = "IngressRoute";
pub const KIND_POLICY: &str = "WafPolicy";

/// One parsed manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub kind: &'static str,
    pub name: String,
    pub spec_json: String,
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    kind: String,
    metadata: RawMetadata,
    #[serde(default)]
    spec: serde_yaml::Value,
}

#[derive(Debug, Deserialize)]
struct RawMetadata {
    name: String,
}

/// Canonical kind for the spellings `kctl apply` accepts.
pub fn canonical_kind(kind: &str) -> Option<&'static str> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "ingressgateway" | "ingress-gateway" | "ingress_gateway" => Some(KIND_GATEWAY),
        "ingressroute" | "ingress-route" | "ingress_route" => Some(KIND_ROUTE),
        "wafpolicy" | "waf-policy" | "waf_policy" => Some(KIND_POLICY),
        _ => None,
    }
}

pub fn parse_document(text: &str) -> Result<Document> {
    let raw: RawManifest = serde_yaml::from_str(text).context("parsing manifest YAML")?;
    let kind =
        canonical_kind(&raw.kind).with_context(|| format!("not an ingress kind: {}", raw.kind))?;
    let name = raw.metadata.name.trim().to_string();
    if name.is_empty() {
        bail!("metadata.name is required");
    }
    // An empty spec is a valid WafPolicy (all defaults), so null becomes {}.
    let spec = if raw.spec.is_null() {
        serde_json::json!({})
    } else {
        serde_json::to_value(&raw.spec).context("converting spec to JSON")?
    };
    Ok(Document {
        kind,
        name,
        spec_json: spec.to_string(),
    })
}

pub fn read_document(file: &str) -> Result<Document> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    parse_document(&text).with_context(|| format!("in {file}"))
}

pub async fn apply_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let doc = read_document(file)?;
    let mut client = client::controller_client(info).await?;
    let label = format!("{} '{}'", doc.kind, doc.name);
    let (action, changed) = match doc.kind {
        KIND_GATEWAY => {
            let r = client
                .create_ingress_gateway(pb::CreateIngressGatewayRequest {
                    name: doc.name.clone(),
                    spec_json: doc.spec_json,
                })
                .await
                .context("create_ingress_gateway rpc")?
                .into_inner();
            (r.action, r.changed_fields)
        }
        KIND_ROUTE => {
            let r = client
                .create_ingress_route(pb::CreateIngressRouteRequest {
                    name: doc.name.clone(),
                    spec_json: doc.spec_json,
                })
                .await
                .context("create_ingress_route rpc")?
                .into_inner();
            (r.action, r.changed_fields)
        }
        _ => {
            let r = client
                .create_waf_policy(pb::CreateWafPolicyRequest {
                    name: doc.name.clone(),
                    spec_json: doc.spec_json,
                })
                .await
                .context("create_waf_policy rpc")?
                .into_inner();
            (r.action, r.changed_fields)
        }
    };
    println!("{}", render_apply_summary(action, &changed, &label));
    Ok(())
}

// --------------------------------------------------------------------- plan

/// Which gateway a set of documents is about: the gateway document itself, or
/// the gateway a route names. `--gateway` wins when given.
pub fn plan_target(docs: &[Document], explicit: Option<&str>) -> Result<String> {
    if let Some(g) = explicit.map(str::trim).filter(|g| !g.is_empty()) {
        return Ok(g.to_string());
    }
    if let Some(g) = docs.iter().find(|d| d.kind == KIND_GATEWAY) {
        return Ok(g.name.clone());
    }
    for d in docs.iter().filter(|d| d.kind == KIND_ROUTE) {
        let v: serde_json::Value = serde_json::from_str(&d.spec_json).unwrap_or_default();
        if let Some(g) = v["gateway"].as_str().filter(|g| !g.is_empty()) {
            return Ok(g.to_string());
        }
    }
    bail!("cannot tell which gateway to plan; pass --gateway")
}

pub async fn plan(
    info: &ConnectionInfo,
    files: &[String],
    gateway: Option<&str>,
    show_config: bool,
) -> Result<()> {
    if files.is_empty() {
        bail!("pass at least one -f FILE");
    }
    let docs: Vec<Document> = files
        .iter()
        .map(|f| read_document(f))
        .collect::<Result<_>>()?;
    let target = plan_target(&docs, gateway)?;
    let mut client = client::controller_client(info).await?;
    let resp = client
        .plan_ingress(pb::PlanIngressRequest {
            gateway: target.clone(),
            documents: docs
                .into_iter()
                .map(|d| pb::IngressDocument {
                    kind: d.kind.to_string(),
                    name: d.name,
                    spec_json: d.spec_json,
                })
                .collect(),
        })
        .await
        .context("plan_ingress rpc")?
        .into_inner();
    print!("{}", render_plan(&target, &resp, show_config));
    if !resp.errors.is_empty() {
        bail!("plan has {} error(s)", resp.errors.len());
    }
    Ok(())
}

pub fn render_plan(gateway: &str, resp: &pb::PlanIngressResponse, show_config: bool) -> String {
    let mut out = format!("Plan for gateway '{gateway}'\n");
    for e in &resp.errors {
        out.push_str(&format!("  error:   {e}\n"));
    }
    for w in &resp.warnings {
        out.push_str(&format!("  warning: {w}\n"));
    }
    for n in &resp.nodes {
        let state = if n.changed {
            "will change"
        } else {
            "unchanged"
        };
        let short = |h: &str| h.chars().take(12).collect::<String>();
        out.push_str(&format!(
            "  node {}: {state} (applied {} -> {})\n",
            n.node_id,
            if n.applied_hash.is_empty() {
                "none".into()
            } else {
                short(&n.applied_hash)
            },
            short(&n.hash)
        ));
    }
    if show_config {
        if let Some(n) = resp.nodes.first() {
            out.push_str("\nRendered Caddy configuration:\n");
            out.push_str(&n.caddy_json);
            out.push('\n');
        }
    }
    if resp.errors.is_empty() && resp.nodes.is_empty() {
        out.push_str("  nothing to render\n");
    }
    out
}

// ------------------------------------------------------------- get/describe

fn phase_of_gateway(g: &pb::IngressGateway) -> (String, String) {
    g.status
        .as_ref()
        .map(|s| (s.phase.clone(), s.message.clone()))
        .unwrap_or_else(|| ("unknown".into(), String::new()))
}

pub async fn list_gateways(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_ingress_gateways(pb::ListIngressGatewaysRequest {})
        .await
        .context("list_ingress_gateways rpc")?
        .into_inner();
    if resp.gateways.is_empty() {
        println!("No IngressGateway resources.");
        return Ok(());
    }
    for g in resp.gateways {
        let (phase, message) = phase_of_gateway(&g);
        println!("{}  gen={}  phase={phase}  {message}", g.name, g.generation);
    }
    Ok(())
}

pub async fn describe_gateway(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let g = client
        .get_ingress_gateway(pb::GetIngressGatewayRequest { name: name.into() })
        .await
        .context("get_ingress_gateway rpc")?
        .into_inner()
        .gateway
        .context("empty response")?;
    let (phase, message) = phase_of_gateway(&g);
    println!("Name:        {}", g.name);
    println!("Generation:  {}", g.generation);
    println!("Phase:       {phase}");
    if !message.is_empty() {
        println!("Message:     {message}");
    }
    let status = g.status.unwrap_or_default();
    for n in &status.nodes {
        println!(
            "Node {}: {} applied={} desired={}",
            n.node_id,
            if n.healthy { "healthy" } else { "not ready" },
            short_hash(&n.applied_hash),
            short_hash(&n.desired_hash)
        );
        if !n.vip_holder.is_empty() {
            println!("  holds {}", n.vip_holder);
        }
        if !n.healthy && !n.message.is_empty() {
            println!("  {}", n.message);
        }
        for c in &n.certs {
            println!("  cert {} ({}) until {}", c.host, c.issuer, c.not_after);
        }
    }
    println!("Spec:\n{}", pretty_json(&g.spec_json));
    Ok(())
}

pub async fn list_routes(info: &ConnectionInfo, gateway: Option<&str>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_ingress_routes(pb::ListIngressRoutesRequest {
            gateway: gateway.unwrap_or_default().into(),
        })
        .await
        .context("list_ingress_routes rpc")?
        .into_inner();
    if resp.routes.is_empty() {
        println!("No IngressRoute resources.");
        return Ok(());
    }
    for r in resp.routes {
        let phase = r
            .status
            .as_ref()
            .map(|s| s.phase.as_str())
            .unwrap_or("unknown");
        let owner = if r.owner.is_empty() {
            String::new()
        } else {
            format!("  owner={}", r.owner)
        };
        println!(
            "{}  gateway={}  gen={}  phase={phase}{owner}",
            r.name, r.gateway, r.generation
        );
    }
    Ok(())
}

pub async fn describe_route(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let r = client
        .get_ingress_route(pb::GetIngressRouteRequest { name: name.into() })
        .await
        .context("get_ingress_route rpc")?
        .into_inner()
        .route
        .context("empty response")?;
    let status = r.status.clone().unwrap_or_default();
    println!("Name:        {}", r.name);
    println!("Gateway:     {}", r.gateway);
    println!("Generation:  {}", r.generation);
    println!(
        "Phase:       {}",
        if status.phase.is_empty() {
            "unknown"
        } else {
            &status.phase
        }
    );
    if !status.message.is_empty() {
        println!("Message:     {}", status.message);
    }
    for c in &status.certs {
        println!(
            "Cert:        {} ({}) until {}",
            c.host, c.issuer, c.not_after
        );
    }
    for u in &status.upstreams {
        println!(
            "Upstream:    {} {}",
            u.address,
            if u.healthy { "healthy" } else { "unhealthy" }
        );
    }
    let spec: serde_json::Value = serde_json::from_str(&r.spec_json).unwrap_or_default();
    println!(
        "WAF mode:    {}",
        spec["waf"]["mode"].as_str().unwrap_or("(policy default)")
    );
    println!("Spec:\n{}", pretty_json(&r.spec_json));
    Ok(())
}

pub async fn list_policies(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_waf_policies(pb::ListWafPoliciesRequest {})
        .await
        .context("list_waf_policies rpc")?
        .into_inner();
    if resp.policies.is_empty() {
        println!("No WafPolicy resources.");
        return Ok(());
    }
    for p in resp.policies {
        let v: serde_json::Value = serde_json::from_str(&p.spec_json).unwrap_or_default();
        println!(
            "{}  gen={}  engine={}  paranoia={}",
            p.name,
            p.generation,
            v["engine"].as_str().unwrap_or("detect"),
            v["crs"]["paranoiaLevel"].as_i64().unwrap_or(1)
        );
    }
    Ok(())
}

pub async fn describe_policy(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let p = client
        .get_waf_policy(pb::GetWafPolicyRequest { name: name.into() })
        .await
        .context("get_waf_policy rpc")?
        .into_inner()
        .policy
        .context("empty response")?;
    println!("Name:        {}", p.name);
    println!("Generation:  {}", p.generation);
    println!("Spec:\n{}", pretty_json(&p.spec_json));
    Ok(())
}

fn short_hash(h: &str) -> String {
    if h.is_empty() {
        "none".into()
    } else {
        h.chars().take(12).collect()
    }
}

fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| s.to_string())
}

// ------------------------------------------------------------------- delete

pub async fn delete(info: &ConnectionInfo, kind: &str, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let ok = match kind {
        KIND_GATEWAY => {
            client
                .delete_ingress_gateway(pb::DeleteIngressGatewayRequest { name: name.into() })
                .await
                .context("delete_ingress_gateway rpc")?
                .into_inner()
                .success
        }
        KIND_ROUTE => {
            client
                .delete_ingress_route(pb::DeleteIngressRouteRequest { name: name.into() })
                .await
                .context("delete_ingress_route rpc")?
                .into_inner()
                .success
        }
        _ => {
            client
                .delete_waf_policy(pb::DeleteWafPolicyRequest { name: name.into() })
                .await
                .context("delete_waf_policy rpc")?
                .into_inner()
                .success
        }
    };
    if ok {
        println!("Deleted {kind} '{name}'.");
        Ok(())
    } else {
        bail!("{kind} '{name}' was not found")
    }
}

// --------------------------------------------------------------- WAF events

/// Parses `30s`, `15m`, `1h`, `24h`, `7d` into seconds.
pub fn parse_since(s: &str) -> Result<i64> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = digits
        .parse()
        .with_context(|| format!("invalid duration {s:?}"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("invalid duration {s:?}; use e.g. 30m, 1h, 7d"),
    };
    Ok(n * mult)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn fetch_events(
    info: &ConnectionInfo,
    route: Option<&str>,
    since: Option<&str>,
    rule: Option<i32>,
    limit: i32,
) -> Result<Vec<pb::WafEvent>> {
    let since_unix = match since {
        Some(s) => now_unix() - parse_since(s)?,
        None => 0,
    };
    let mut client = client::controller_client(info).await?;
    Ok(client
        .list_waf_events(pb::ListWafEventsRequest {
            gateway: String::new(),
            route: route.unwrap_or_default().into(),
            rule_id: rule.unwrap_or(0),
            client_ip: String::new(),
            since_unix,
            limit,
        })
        .await
        .context("list_waf_events rpc")?
        .into_inner()
        .events)
}

pub async fn waf_events(
    info: &ConnectionInfo,
    route: Option<&str>,
    since: Option<&str>,
    rule: Option<i32>,
    limit: i32,
) -> Result<()> {
    let events = fetch_events(info, route, since, rule, limit).await?;
    if events.is_empty() {
        println!("No WAF events.");
        return Ok(());
    }
    for e in events {
        let at = e.created_at.map(|t| t.seconds).unwrap_or(0);
        println!(
            "{at}  {:<8}  route={}  rule={}  {}  {}{}",
            e.action, e.route, e.rule_id, e.client_ip, e.host, e.uri
        );
    }
    Ok(())
}

pub async fn set_mode(info: &ConnectionInfo, route: &str, mode: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    client
        .set_ingress_route_waf_mode(pb::SetIngressRouteWafModeRequest {
            name: route.into(),
            mode: mode.into(),
        })
        .await
        .context("set_ingress_route_waf_mode rpc")?;
    println!("Route '{route}' WAF mode is now {mode}.");
    Ok(())
}

/// One suggested exclusion and how many events it would have cleared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Suggestion {
    pub rule_id: i32,
    pub path: String,
    pub arg: Option<String>,
    pub events: usize,
}

/// Path of a request URI without the query string.
fn uri_path(uri: &str) -> String {
    let p = uri.split(['?', '#']).next().unwrap_or("/");
    if p.is_empty() {
        "/".into()
    } else {
        p.to_string()
    }
}

/// The argument name a rule matched, from Coraza's message text
/// (`... found within ARGS:q: <script>`).
fn matched_arg(raw_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw_json).ok()?;
    let text = v["data"]["data"]
        .as_str()
        .or_else(|| v["message"].as_str())?;
    let rest = text.split("ARGS:").nth(1)?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Exclusions that would have cleared the given events. Anomaly-score
/// summary rules (949110, 959100, 980130) say nothing about a cause and are
/// skipped. A suggestion needs `min_events` hits so one-off probes are not
/// turned into permanent holes.
pub fn suggest_exclusions(events: &[pb::WafEvent], min_events: usize) -> Vec<Suggestion> {
    const SUMMARY_RULES: [i32; 3] = [949110, 959100, 980130];
    let mut counts: BTreeMap<(i32, String, Option<String>), usize> = BTreeMap::new();
    for e in events {
        if SUMMARY_RULES.contains(&e.rule_id) || e.rule_id <= 0 {
            continue;
        }
        *counts
            .entry((e.rule_id, uri_path(&e.uri), matched_arg(&e.raw_json)))
            .or_default() += 1;
    }
    let mut out: Vec<Suggestion> = counts
        .into_iter()
        .filter(|(_, n)| *n >= min_events)
        .map(|((rule_id, path, arg), events)| Suggestion {
            rule_id,
            path,
            arg,
            events,
        })
        .collect();
    out.sort_by(|a, b| b.events.cmp(&a.events).then(a.cmp(b)));
    out
}

pub fn render_suggestions(route: &str, suggestions: &[Suggestion]) -> String {
    if suggestions.is_empty() {
        return format!("No exclusions suggested for route '{route}'.\n");
    }
    let mut out = format!(
        "# Paste under spec.waf of route '{route}'. Review each one: an exclusion\n# disables that rule for that path.\nexclusions:\n"
    );
    for s in suggestions {
        out.push_str(&format!(
            "  # {} event(s)\n  - ruleId: {}\n    path: {}\n",
            s.events, s.rule_id, s.path
        ));
        if let Some(arg) = &s.arg {
            out.push_str(&format!("    arg: {arg}\n"));
        }
    }
    out
}

pub async fn waf_suggest(
    info: &ConnectionInfo,
    route: &str,
    since: &str,
    min_events: usize,
) -> Result<()> {
    let events = fetch_events(info, Some(route), Some(since), None, 5000).await?;
    print!(
        "{}",
        render_suggestions(route, &suggest_exclusions(&events, min_events))
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifests_and_kind_aliases() {
        let d = parse_document(
            "kind: ingress-route\nmetadata:\n  name: shop\nspec:\n  gateway: edge\n  hosts: [a.example.com]\n",
        )
        .unwrap();
        assert_eq!(d.kind, KIND_ROUTE);
        assert_eq!(d.name, "shop");
        let v: serde_json::Value = serde_json::from_str(&d.spec_json).unwrap();
        assert_eq!(v["hosts"][0], "a.example.com");
        // A policy with no spec is all defaults, not an error.
        let p = parse_document("kind: WafPolicy\nmetadata:\n  name: crs\n").unwrap();
        assert_eq!(p.spec_json, "{}");
        assert!(parse_document("kind: VM\nmetadata:\n  name: x\n").is_err());
        assert!(parse_document("kind: WafPolicy\nmetadata:\n  name: ' '\n").is_err());
    }

    #[test]
    fn plan_target_prefers_flag_then_gateway_then_route() {
        let gw = Document {
            kind: KIND_GATEWAY,
            name: "edge".into(),
            spec_json: "{}".into(),
        };
        let route = Document {
            kind: KIND_ROUTE,
            name: "shop".into(),
            spec_json: r#"{"gateway":"other"}"#.into(),
        };
        assert_eq!(
            plan_target(&[route.clone(), gw.clone()], None).unwrap(),
            "edge"
        );
        assert_eq!(
            plan_target(std::slice::from_ref(&route), None).unwrap(),
            "other"
        );
        assert_eq!(plan_target(&[route, gw], Some("x")).unwrap(), "x");
        assert!(plan_target(&[], None).is_err());
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_since("30s").unwrap(), 30);
        assert_eq!(parse_since("15m").unwrap(), 900);
        assert_eq!(parse_since("24h").unwrap(), 86_400);
        assert_eq!(parse_since("7d").unwrap(), 604_800);
        for bad in ["", "h", "10", "5w", "-1h"] {
            assert!(parse_since(bad).is_err(), "{bad}");
        }
    }

    fn event(rule: i32, uri: &str, arg: &str) -> pb::WafEvent {
        pb::WafEvent {
            rule_id: rule,
            uri: uri.into(),
            raw_json: format!(
                r#"{{"data":{{"data":"Matched Data: x found within ARGS:{arg}: x"}}}}"#
            ),
            ..Default::default()
        }
    }

    #[test]
    fn suggestions_group_by_rule_path_and_arg() {
        let mut events = vec![
            event(942100, "/api/search?q=1", "q"),
            event(942100, "/api/search?q=2", "q"),
            event(942100, "/api/search?q=3", "q"),
            event(941100, "/once", "x"),
        ];
        events.extend((0..5).map(|_| event(949110, "/api/search", "q")));
        let s = suggest_exclusions(&events, 2);
        assert_eq!(
            s,
            vec![Suggestion {
                rule_id: 942100,
                path: "/api/search".into(),
                arg: Some("q".into()),
                events: 3
            }]
        );
        let yaml = render_suggestions("shop", &s);
        assert!(yaml.contains("- ruleId: 942100"));
        assert!(yaml.contains("arg: q"));
        assert!(render_suggestions("shop", &[]).contains("No exclusions"));
    }

    #[test]
    fn plan_output_shows_errors_warnings_and_nodes() {
        let resp = pb::PlanIngressResponse {
            errors: vec!["route shop: bad".into()],
            warnings: vec!["route shop has no WAF policy".into()],
            nodes: vec![pb::PlanIngressNode {
                node_id: "node-1".into(),
                caddy_json: "{}".into(),
                hash: "abcdef0123456789".into(),
                applied_hash: String::new(),
                changed: true,
            }],
        };
        let out = render_plan("edge", &resp, true);
        assert!(out.contains("error:   route shop: bad"));
        assert!(out.contains("warning: route shop has no WAF policy"));
        assert!(out.contains("node node-1: will change (applied none -> abcdef012345)"));
        assert!(out.contains("Rendered Caddy configuration"));
        assert!(!render_plan("edge", &resp, false).contains("Rendered Caddy"));
    }
}
