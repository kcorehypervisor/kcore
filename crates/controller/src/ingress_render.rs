//! Pure renderers for the ingress gateway.
//!
//! [`render_caddy`] turns a gateway, its routes, the WAF policies they name and
//! the resolved backend addresses into the Caddy JSON the node agent loads
//! through the admin socket. [`render_waf_directives`] turns a `WafPolicy` into
//! Coraza directives. Neither touches the database or the network, so both are
//! covered by golden files under `tests/fixtures/ingress/`.

use std::collections::{BTreeMap, BTreeSet};

use aws_lc_rs::digest;
use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::ingress_spec::{
    acme_directory_url, is_wildcard_host, pem_certificates, Backend, BackendTls, GatewaySpec,
    PathSpec, RouteKind, RouteSpec, WafExclusion, WafPolicySpec,
};

/// Where the node-local pieces live; supplied by the node's Nix module values.
#[derive(Debug, Clone)]
pub struct NodePaths {
    pub admin_socket: String,
    pub cert_store: String,
    pub waf_audit_log: String,
    pub metrics_addr: String,
}

impl Default for NodePaths {
    fn default() -> Self {
        Self {
            admin_socket: "/run/kcore-ingress/admin.sock".into(),
            cert_store: "/var/lib/kcore/ingress/certmagic".into(),
            waf_audit_log: "/var/log/kcore/ingress/waf.jsonl".into(),
            metrics_addr: "127.0.0.1:2020".into(),
        }
    }
}

pub struct RenderInput<'a> {
    pub gateway: &'a GatewaySpec,
    /// Routes of this gateway by name.
    pub routes: &'a BTreeMap<String, RouteSpec>,
    pub waf_policies: &'a BTreeMap<String, WafPolicySpec>,
    /// Dial addresses (`ip:port`) per [`Backend::key`]. A backend with no
    /// entry, or an empty list, is a render error rather than a silent drop.
    pub backends: &'a BTreeMap<String, Vec<String>>,
    pub paths: &'a NodePaths,
    /// Whether this node may obtain and renew certificates. A standby that has
    /// not yet received the holder's certificates must not try: its ACME
    /// challenges would land on the other node and fail.
    pub issue: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub json: String,
    /// Lowercase hex SHA-256 of `json`; compared with the node's reported hash.
    pub hash: String,
}

pub fn render_caddy(input: &RenderInput<'_>) -> Result<Rendered, String> {
    let gw = input.gateway;
    // Ports owned by TCP routes belong to the layer-4 app, not the HTTP server.
    let tcp_ports: BTreeSet<u16> = input
        .routes
        .values()
        .filter(|r| r.kind == RouteKind::Tcp)
        .filter_map(|r| r.port)
        .collect();
    let mut listen = Vec::new();
    for l in &gw.listeners {
        for port in l.ports.iter().filter(|p| !tcp_ports.contains(p)) {
            listen.push(Value::String(format!(
                "{}:{port}",
                host_port_addr(&l.address)
            )));
        }
    }
    let http3 = gw.listeners.iter().any(|l| l.http3);

    let mut routes = Vec::new();
    let mut tcp_routes: Vec<(&String, &RouteSpec)> = Vec::new();
    let mut subjects = Vec::new();
    let mut any_redirect = false;
    for (name, route) in input.routes {
        if route.kind == RouteKind::Tcp {
            tcp_routes.push((name, route));
            continue;
        }
        any_redirect |= route.redirect_http;
        let mut hosts: Vec<String> = route.hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
        hosts.sort();
        subjects.extend(hosts.iter().cloned());

        // Longest prefix first: Caddy takes the first matching route.
        let mut paths: Vec<&PathSpec> = route.paths.iter().collect();
        paths.sort_by(|a, b| {
            b.prefix
                .len()
                .cmp(&a.prefix.len())
                .then(a.prefix.cmp(&b.prefix))
        });
        for path in paths {
            routes.push(render_route(input, name, route, &hosts, path)?);
        }
    }
    subjects.sort();

    let mut server = Map::new();
    server.insert("listen".into(), Value::Array(listen));
    server.insert("routes".into(), Value::Array(routes));
    server.insert("metrics".into(), json!({}));
    if http3 {
        server.insert("protocols".into(), json!(["h1", "h2", "h3"]));
    }
    if !input.issue {
        // Without this Caddy would manage certificates for the route hosts anyway.
        server.insert("automatic_https".into(), json!({"disable": true}));
    } else if !any_redirect {
        server.insert("automatic_https".into(), json!({"disable_redirects": true}));
    }
    if gw.access_log {
        server.insert("logs".into(), json!({}));
    }

    let mut apps = Map::new();
    apps.insert(
        "http".into(),
        json!({"servers": {
            "kcore": Value::Object(server),
            "metrics": {
                "listen": [input.paths.metrics_addr],
                "routes": [{"handle": [{"handler": "metrics"}]}],
                "automatic_https": {"disable": true}
            }
        }}),
    );
    if !tcp_routes.is_empty() {
        apps.insert("layer4".into(), render_layer4(input, &tcp_routes)?);
    }
    if !subjects.is_empty() && input.issue {
        apps.insert("tls".into(), render_tls(gw, &subjects));
    }

    let mut root = Map::new();
    root.insert(
        "admin".into(),
        json!({"listen": format!("unix/{}|0600", input.paths.admin_socket)}),
    );
    root.insert(
        "storage".into(),
        json!({"module": "file_system", "root": input.paths.cert_store}),
    );
    if gw.access_log {
        root.insert(
            "logging".into(),
            json!({"logs": {"default": {"encoder": {"format": "json"}}}}),
        );
    }
    root.insert("apps".into(), Value::Object(apps));

    let json = serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
    let hash = hex(digest::digest(&digest::SHA256, json.as_bytes()).as_ref());
    Ok(Rendered { json, hash })
}

/// One layer-4 server per TCP port. Routes with SNI names come before the
/// catch-all so a specific name is never shadowed.
fn render_layer4(
    input: &RenderInput<'_>,
    tcp_routes: &[(&String, &RouteSpec)],
) -> Result<Value, String> {
    let mut by_port: BTreeMap<u16, Vec<(&String, &RouteSpec)>> = BTreeMap::new();
    for (name, r) in tcp_routes {
        let port = r
            .port
            .ok_or_else(|| format!("tcp route {name} has no port"))?;
        by_port.entry(port).or_default().push((name, r));
    }
    let mut servers = Map::new();
    for (port, mut routes) in by_port {
        let listen: Vec<String> = input
            .gateway
            .listeners
            .iter()
            .filter(|l| l.ports.contains(&port))
            .map(|l| format!("{}:{port}", host_port_addr(&l.address)))
            .collect();
        if listen.is_empty() {
            return Err(format!("no gateway listener carries tcp port {port}"));
        }
        routes.sort_by_key(|(name, r)| (r.hosts.is_empty(), (*name).clone()));
        let mut rendered = Vec::new();
        for (name, r) in routes {
            let mut dials = Vec::new();
            for b in &r.backends {
                dials.extend(resolve(input, name, "tcp", b)?);
            }
            dials.sort();
            dials.dedup();
            let mut proxy = Map::new();
            proxy.insert("handler".into(), json!("proxy"));
            proxy.insert(
                "upstreams".into(),
                Value::Array(dials.iter().map(|d| json!({"dial": [d]})).collect()),
            );
            if r.proxy_protocol {
                proxy.insert("proxy_protocol".into(), json!("v2"));
            }
            let mut route = Map::new();
            if !r.hosts.is_empty() {
                let mut sni: Vec<String> = r.hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
                sni.sort();
                route.insert("match".into(), json!([{"tls": {"sni": sni}}]));
            }
            route.insert("handle".into(), json!([Value::Object(proxy)]));
            rendered.push(Value::Object(route));
        }
        servers.insert(
            format!("tcp-{port}"),
            json!({"listen": listen, "routes": rendered}),
        );
    }
    Ok(json!({"servers": Value::Object(servers)}))
}

fn dns_provider(provider: &str) -> Value {
    match provider {
        "cloudflare" => json!({"name": "cloudflare", "api_token": "{env.CLOUDFLARE_API_TOKEN}"}),
        "rfc2136" => json!({
            "name": "rfc2136",
            "server": "{env.RFC2136_SERVER}",
            "key_name": "{env.RFC2136_KEY_NAME}",
            "key_alg": "{env.RFC2136_KEY_ALG}",
            "key": "{env.RFC2136_KEY}"
        }),
        // route53 reads the standard AWS_* variables from the credentials file.
        other => json!({"name": other}),
    }
}

/// ACME automation policies. Wildcard names need DNS-01. Other names use the
/// HTTP challenges when the gateway enables them, and DNS-01 when `dns-01` is
/// the only challenge listed.
fn render_tls(gw: &GatewaySpec, subjects: &[String]) -> Value {
    let acme = &gw.tls.acme;
    let enabled = |c: &str| acme.challenges.iter().any(|x| x == c);
    let http_enabled = enabled("http-01") || enabled("tls-alpn-01");
    let (wild, plain): (Vec<&String>, Vec<&String>) =
        subjects.iter().partition(|s| is_wildcard_host(s));
    let (std_subjects, dns_subjects): (Vec<&String>, Vec<&String>) = if http_enabled {
        (plain, wild)
    } else {
        (Vec::new(), subjects.iter().collect())
    };

    let issuer = |challenges: Map<String, Value>| {
        let mut issuer = Map::new();
        issuer.insert("module".into(), json!("acme"));
        issuer.insert("email".into(), json!(acme.email));
        issuer.insert("ca".into(), json!(acme_directory_url(&acme.ca)));
        if !challenges.is_empty() {
            issuer.insert("challenges".into(), Value::Object(challenges));
        }
        Value::Object(issuer)
    };

    let mut policies = Vec::new();
    if !std_subjects.is_empty() {
        let mut challenges = Map::new();
        if !enabled("http-01") {
            challenges.insert("http".into(), json!({"disabled": true}));
        }
        if !enabled("tls-alpn-01") {
            challenges.insert("tls-alpn".into(), json!({"disabled": true}));
        }
        policies.push(json!({
            "subjects": std_subjects,
            "issuers": [issuer(challenges)]
        }));
    }
    if !dns_subjects.is_empty() {
        let provider = acme
            .dns01
            .as_ref()
            .map(|d| dns_provider(&d.provider))
            .unwrap_or(Value::Null);
        let mut challenges = Map::new();
        challenges.insert("dns".into(), json!({"provider": provider}));
        challenges.insert("http".into(), json!({"disabled": true}));
        challenges.insert("tls-alpn".into(), json!({"disabled": true}));
        policies.push(json!({
            "subjects": dns_subjects,
            "issuers": [issuer(challenges)]
        }));
    }
    json!({"automation": {"policies": policies}})
}

fn render_route(
    input: &RenderInput<'_>,
    name: &str,
    route: &RouteSpec,
    hosts: &[String],
    path: &PathSpec,
) -> Result<Value, String> {
    let paths = if path.prefix == "/" {
        vec!["/*".to_string()]
    } else {
        let p = path.prefix.trim_end_matches('/').to_string();
        vec![p.clone(), format!("{p}/*")]
    };
    let mut handle: Vec<Value> = Vec::new();

    // Headers first so they apply to blocked responses as well.
    let mut headers = Map::new();
    if route.headers.hsts {
        headers.insert(
            "response".into(),
            json!({"set": {"Strict-Transport-Security": ["max-age=31536000"]}}),
        );
    }
    if route.headers.forwarded.iter().any(|h| h == "X-Real-IP") {
        headers.insert(
            "request".into(),
            json!({"set": {"X-Real-IP": ["{http.request.remote.host}"]}}),
        );
    }
    if !headers.is_empty() {
        let mut h = Map::new();
        h.insert("handler".into(), json!("headers"));
        h.extend(headers);
        handle.push(Value::Object(h));
    }

    if let Some(waf) = render_route_waf(input, name, route)? {
        handle.push(waf);
    }

    let per_minute = route.rate_limit.or(input
        .gateway
        .rate_limit
        .as_ref()
        .map(|r| r.requests_per_minute));
    if let Some(max_events) = per_minute {
        handle.push(json!({
            "handler": "rate_limit",
            "rate_limits": {"client": {
                "key": "{http.request.remote.host}",
                "window": "1m",
                "max_events": max_events
            }}
        }));
    }

    handle.push(render_proxy(input, name, path)?);

    Ok(json!({
        "match": [{"host": hosts, "path": paths}],
        "handle": handle,
        "terminal": true
    }))
}

fn render_route_waf(
    input: &RenderInput<'_>,
    route_name: &str,
    route: &RouteSpec,
) -> Result<Option<Value>, String> {
    let policy_name = route
        .waf
        .policy
        .as_ref()
        .or(input.gateway.default_waf_policy.as_ref());
    let mode = route.waf.mode.as_deref();
    if mode == Some("off") {
        return Ok(None);
    }
    let Some(policy_name) = policy_name else {
        return Ok(None);
    };
    let policy = input
        .waf_policies
        .get(policy_name)
        .ok_or_else(|| format!("route {route_name}: WafPolicy {policy_name} does not exist"))?;
    let directives = render_waf_directives(
        policy,
        mode,
        &route.waf.exclusions,
        &input.paths.waf_audit_log,
    );
    Ok(Some(json!({
        "handler": "waf",
        "load_owasp_crs": true,
        "directives": directives
    })))
}

fn render_proxy(input: &RenderInput<'_>, route: &str, path: &PathSpec) -> Result<Value, String> {
    let mut dials = Vec::new();
    for b in &path.backends {
        let addrs = resolve(input, route, &path.prefix, b)?;
        dials.extend(addrs);
    }
    dials.sort();
    dials.dedup();

    let mut lb = Map::new();
    lb.insert(
        "selection_policy".into(),
        json!({"policy": path.load_balancing.policy}),
    );
    if let Some(t) = &path.load_balancing.try_duration {
        lb.insert("try_duration".into(), json!(t));
    }

    let mut proxy = Map::new();
    proxy.insert("handler".into(), json!("reverse_proxy"));
    proxy.insert(
        "upstreams".into(),
        Value::Array(dials.iter().map(|d| json!({"dial": d})).collect()),
    );
    proxy.insert("load_balancing".into(), Value::Object(lb));
    if path.backend_tls.is_some() || path.proxy_protocol {
        let mut transport = Map::new();
        transport.insert("protocol".into(), json!("http"));
        if let Some(t) = &path.backend_tls {
            transport.insert("tls".into(), render_backend_tls(t));
        }
        if path.proxy_protocol {
            transport.insert("proxy_protocol".into(), json!("v2"));
        }
        proxy.insert("transport".into(), Value::Object(transport));
    }
    if let Some(h) = &path.health_check {
        proxy.insert(
            "health_checks".into(),
            json!({"active": {
                "uri": h.path,
                "interval": h.interval,
                "expect_status": h.expect_status
            }}),
        );
    }
    Ok(Value::Object(proxy))
}

/// Upstream TLS settings. Verification is always on; a private CA is given as
/// an inline pool of DER certificates.
fn render_backend_tls(t: &BackendTls) -> Value {
    let mut tls = Map::new();
    if let Some(name) = &t.server_name {
        tls.insert("server_name".into(), json!(name));
    }
    if t.ca_bundle != "system" {
        let certs: Vec<String> = pem_certificates(&t.ca_bundle)
            .iter()
            .map(|der| base64::engine::general_purpose::STANDARD.encode(der))
            .collect();
        tls.insert(
            "ca".into(),
            json!({"provider": "inline", "trusted_ca_certs": certs}),
        );
    }
    Value::Object(tls)
}

fn resolve(
    input: &RenderInput<'_>,
    route: &str,
    prefix: &str,
    b: &Backend,
) -> Result<Vec<String>, String> {
    if let Backend::External { address } = b {
        return Ok(vec![address.clone()]);
    }
    match input.backends.get(&b.key()) {
        Some(addrs) if !addrs.is_empty() => Ok(addrs.clone()),
        _ => Err(format!(
            "route {route} path {prefix}: backend {} has no resolved address",
            b.key()
        )),
    }
}

/// `1.2.3.4` stays as is; an IPv6 literal gets brackets.
fn host_port_addr(addr: &str) -> String {
    if addr.contains(':') {
        format!("[{addr}]")
    } else {
        addr.to_string()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------- WAF

/// Renders a policy to Coraza directives. `mode` is the route override
/// (`detect`, `block`; `off` is handled by the caller).
pub fn render_waf_directives(
    policy: &WafPolicySpec,
    mode: Option<&str>,
    exclusions: &[WafExclusion],
    audit_log: &str,
) -> String {
    let engine = match mode.unwrap_or(policy.engine.as_str()) {
        "block" => "On",
        _ => "DetectionOnly",
    };
    let crs = &policy.crs;
    let mut out = String::new();
    out.push_str("Include @coraza.conf-recommended\n");
    out.push_str("Include @crs-setup.conf.example\n");
    out.push_str(&format!(
        "SecAction \"id:900000,phase:1,pass,nolog,setvar:tx.blocking_paranoia_level={}\"\n",
        crs.paranoia_level
    ));
    out.push_str(&format!(
        "SecAction \"id:900110,phase:1,pass,nolog,setvar:tx.inbound_anomaly_score_threshold={},setvar:tx.outbound_anomaly_score_threshold={}\"\n",
        crs.inbound_anomaly_threshold, crs.outbound_anomaly_threshold
    ));
    if !crs.allowed_methods.is_empty() {
        out.push_str(&format!(
            "SecAction \"id:900200,phase:1,pass,nolog,setvar:'tx.allowed_methods={}'\"\n",
            crs.allowed_methods.join(" ")
        ));
    }
    if !crs.allowed_content_types.is_empty() {
        out.push_str(&format!(
            "SecAction \"id:900220,phase:1,pass,nolog,setvar:'tx.allowed_request_content_type={}'\"\n",
            crs.allowed_content_types
                .iter()
                .map(|c| format!("|{c}|"))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    // IP lists run before the CRS so a denied client is cut off early.
    for (i, cidr) in policy.ip_allow_list.iter().enumerate() {
        out.push_str(&format!(
            "SecRule REMOTE_ADDR \"@ipMatch {cidr}\" \"id:{},phase:1,allow,nolog\"\n",
            100_000 + i
        ));
    }
    for (i, cidr) in policy.ip_deny_list.iter().enumerate() {
        out.push_str(&format!(
            "SecRule REMOTE_ADDR \"@ipMatch {cidr}\" \"id:{},phase:1,deny,status:403,log,msg:'ip denied'\"\n",
            110_000 + i
        ));
    }
    out.push_str("Include @owasp_crs/*.conf\n");
    out.push_str(&format!(
        "SecRequestBodyLimit {}\n",
        policy.request_body_limit_bytes
    ));
    out.push_str(&format!(
        "SecResponseBodyAccess {}\n",
        if policy.response_body_inspection {
            "On"
        } else {
            "Off"
        }
    ));
    out.push_str(&format!("SecRuleEngine {engine}\n"));
    match policy.audit.engine.as_str() {
        "off" => out.push_str("SecAuditEngine Off\n"),
        other => {
            let e = if other == "on" { "On" } else { "RelevantOnly" };
            out.push_str(&format!("SecAuditEngine {e}\n"));
            out.push_str("SecAuditLogFormat JSON\n");
            out.push_str(&format!("SecAuditLogParts {}\n", policy.audit.parts));
            out.push_str(&format!("SecAuditLog {audit_log}\n"));
        }
    }
    // Exclusions must follow the CRS includes: they edit rules already loaded.
    for e in exclusions {
        let target = match &e.arg {
            Some(arg) => format!("ARGS:{arg}"),
            None => "ARGS".to_string(),
        };
        out.push_str(&format!(
            "SecRule REQUEST_URI \"@beginsWith {}\" \"id:{},phase:1,pass,nolog,ctl:ruleRemoveTargetById={};{target}\"\n",
            e.path,
            200_000 + e.rule_id,
            e.rule_id
        ));
    }
    if !policy.custom_rules.trim().is_empty() {
        out.push_str(policy.custom_rules.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress_spec::{parse_gateway, parse_route, parse_waf_policy};

    const GATEWAY: &str = include_str!("../../../tests/fixtures/ingress/gateway.json");
    const ROUTE: &str = include_str!("../../../tests/fixtures/ingress/route-shop.json");
    const POLICY: &str = include_str!("../../../tests/fixtures/ingress/waf-crs-default.json");
    const GOLDEN_CADDY: &str =
        include_str!("../../../tests/fixtures/ingress/caddy-shop.golden.json");
    const GOLDEN_WAF: &str = include_str!("../../../tests/fixtures/ingress/waf-block.golden.conf");

    fn backends() -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([
            (
                "vm/shop-web-1:8080".into(),
                vec!["10.240.10.102:8080".into()],
            ),
            (
                "vm/shop-web-2:8080".into(),
                vec!["10.240.10.103:8080".into()],
            ),
            (
                "container/legacy-php:80".into(),
                vec!["10.240.10.50:80".into()],
            ),
        ])
    }

    fn render(mode: Option<&str>) -> Rendered {
        let gateway = parse_gateway(GATEWAY).unwrap();
        let mut route = parse_route(ROUTE).unwrap();
        route.waf.mode = mode.map(str::to_string);
        let routes = BTreeMap::from([("shop".to_string(), route)]);
        let policies =
            BTreeMap::from([("crs-default".to_string(), parse_waf_policy(POLICY).unwrap())]);
        let paths = NodePaths::default();
        render_caddy(&RenderInput {
            gateway: &gateway,
            routes: &routes,
            waf_policies: &policies,
            backends: &backends(),
            paths: &paths,
            issue: true,
        })
        .unwrap()
    }

    #[test]
    fn caddy_matches_golden_file() {
        let got = render(Some("block"));
        if std::env::var_os("KCORE_UPDATE_GOLDEN").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fixtures/ingress/caddy-shop.golden.json"
                ),
                &got.json,
            )
            .unwrap();
        }
        assert_eq!(got.json.trim_end(), GOLDEN_CADDY.trim_end());
    }

    #[test]
    fn waf_directives_match_golden_file() {
        let policy = parse_waf_policy(POLICY).unwrap();
        let route = parse_route(ROUTE).unwrap();
        let got = render_waf_directives(
            &policy,
            Some("block"),
            &route.waf.exclusions,
            "/var/log/kcore/ingress/waf.jsonl",
        );
        if std::env::var_os("KCORE_UPDATE_GOLDEN").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fixtures/ingress/waf-block.golden.conf"
                ),
                &got,
            )
            .unwrap();
        }
        assert_eq!(got, GOLDEN_WAF);
    }

    #[test]
    fn render_is_deterministic_and_hash_tracks_content() {
        let a = render(Some("block"));
        let b = render(Some("block"));
        assert_eq!(a, b);
        assert_eq!(a.hash.len(), 64);
        assert_ne!(a.hash, render(Some("detect")).hash);
    }

    #[test]
    fn mode_override_controls_the_engine() {
        let policy = parse_waf_policy(POLICY).unwrap();
        let detect = render_waf_directives(&policy, None, &[], "/log");
        assert!(detect.contains("SecRuleEngine DetectionOnly"));
        let block = render_waf_directives(&policy, Some("block"), &[], "/log");
        assert!(block.contains("SecRuleEngine On\n"));
        // A route can relax a blocking policy back to detection.
        let mut p = policy;
        p.engine = "block".into();
        let relaxed = render_waf_directives(&p, Some("detect"), &[], "/log");
        assert!(relaxed.contains("SecRuleEngine DetectionOnly"));
    }

    #[test]
    fn exclusions_follow_the_crs_include() {
        let policy = parse_waf_policy(POLICY).unwrap();
        let route = parse_route(ROUTE).unwrap();
        let out = render_waf_directives(&policy, None, &route.waf.exclusions, "/log");
        let crs = out.find("Include @owasp_crs/*.conf").unwrap();
        let excl = out.find("ruleRemoveTargetById=942100;ARGS:q").unwrap();
        assert!(crs < excl);
    }

    #[test]
    fn off_mode_drops_the_waf_handler() {
        let got = render(Some("off"));
        assert!(!got.json.contains("\"handler\": \"waf\""));
    }

    #[test]
    fn longest_prefix_is_matched_first() {
        let v: Value = serde_json::from_str(&render(None).json).unwrap();
        let routes = v["apps"]["http"]["servers"]["kcore"]["routes"]
            .as_array()
            .unwrap();
        let first: Vec<&str> = routes
            .iter()
            .map(|r| r["match"][0]["path"][0].as_str().unwrap())
            .collect();
        assert_eq!(first, ["/legacy", "/*"]);
    }

    #[test]
    fn unresolved_backend_is_an_error() {
        let gateway = parse_gateway(GATEWAY).unwrap();
        let routes = BTreeMap::from([("shop".to_string(), parse_route(ROUTE).unwrap())]);
        let policies =
            BTreeMap::from([("crs-default".to_string(), parse_waf_policy(POLICY).unwrap())]);
        let paths = NodePaths::default();
        let err = render_caddy(&RenderInput {
            gateway: &gateway,
            routes: &routes,
            waf_policies: &policies,
            backends: &BTreeMap::new(),
            paths: &paths,
            issue: true,
        })
        .unwrap_err();
        assert!(err.contains("no resolved address"), "{err}");
    }

    #[test]
    fn missing_policy_is_an_error() {
        let gateway = parse_gateway(GATEWAY).unwrap();
        let routes = BTreeMap::from([("shop".to_string(), parse_route(ROUTE).unwrap())]);
        let paths = NodePaths::default();
        let err = render_caddy(&RenderInput {
            gateway: &gateway,
            routes: &routes,
            waf_policies: &BTreeMap::new(),
            backends: &backends(),
            paths: &paths,
            issue: true,
        })
        .unwrap_err();
        assert!(
            err.contains("WafPolicy crs-default does not exist"),
            "{err}"
        );
    }

    #[test]
    fn ipv6_listener_is_bracketed() {
        assert_eq!(host_port_addr("2001:db8::1"), "[2001:db8::1]");
        assert_eq!(host_port_addr("203.0.113.10"), "203.0.113.10");
    }

    const GATEWAY_MULTI: &str = include_str!("../../../tests/fixtures/ingress/gateway-multi.json");
    const ROUTE_PAY: &str = include_str!("../../../tests/fixtures/ingress/route-pay.json");
    const ROUTE_DB: &str = include_str!("../../../tests/fixtures/ingress/route-db.json");
    const ROUTE_SNI: &str = include_str!("../../../tests/fixtures/ingress/route-sni.json");
    const GOLDEN_MULTI: &str =
        include_str!("../../../tests/fixtures/ingress/caddy-multi.golden.json");

    fn render_multi() -> Rendered {
        let gateway = parse_gateway(GATEWAY_MULTI).unwrap();
        let routes = BTreeMap::from([
            ("pay".to_string(), parse_route(ROUTE_PAY).unwrap()),
            ("db".to_string(), parse_route(ROUTE_DB).unwrap()),
            ("sni".to_string(), parse_route(ROUTE_SNI).unwrap()),
        ]);
        let backends = BTreeMap::new();
        let policies = BTreeMap::new();
        let paths = NodePaths::default();
        render_caddy(&RenderInput {
            gateway: &gateway,
            routes: &routes,
            waf_policies: &policies,
            backends: &backends,
            paths: &paths,
            issue: true,
        })
        .unwrap()
    }

    #[test]
    fn multi_matches_golden_file() {
        let got = render_multi();
        if std::env::var_os("KCORE_UPDATE_GOLDEN").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fixtures/ingress/caddy-multi.golden.json"
                ),
                &got.json,
            )
            .unwrap();
        }
        assert_eq!(got.json.trim_end(), GOLDEN_MULTI.trim_end());
    }

    #[test]
    fn tcp_ports_leave_the_http_server() {
        let v: Value = serde_json::from_str(&render_multi().json).unwrap();
        let http: Vec<&str> = v["apps"]["http"]["servers"]["kcore"]["listen"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap())
            .collect();
        assert_eq!(http, ["203.0.113.10:80", "203.0.113.10:443"]);
        let l4 = &v["apps"]["layer4"]["servers"];
        assert_eq!(l4["tcp-5432"]["listen"][0], "203.0.113.10:5432");
        let handler = &l4["tcp-5432"]["routes"][0]["handle"][0];
        assert_eq!(handler["handler"], "proxy");
        assert_eq!(handler["proxy_protocol"], "v2");
        assert_eq!(handler["upstreams"][0]["dial"][0], "10.0.0.8:5432");
        // SNI passthrough matches on the ClientHello and has no catch-all.
        assert_eq!(
            l4["tcp-8443"]["routes"][0]["match"][0]["tls"]["sni"][0],
            "pg.example.com"
        );
    }

    #[test]
    fn wildcard_uses_dns01_and_plain_hosts_use_http() {
        let v: Value = serde_json::from_str(&render_multi().json).unwrap();
        let policies = v["apps"]["tls"]["automation"]["policies"]
            .as_array()
            .unwrap();
        assert_eq!(policies.len(), 2);
        let std = &policies[0];
        assert_eq!(std["subjects"], json!(["pay.example.com"]));
        assert!(std["issuers"][0]["challenges"]["dns"].is_null());
        let dns = &policies[1];
        assert_eq!(dns["subjects"], json!(["*.apps.example.com"]));
        assert_eq!(
            dns["issuers"][0]["challenges"]["dns"]["provider"]["name"],
            "cloudflare"
        );
        assert_eq!(dns["issuers"][0]["challenges"]["http"]["disabled"], true);
        // Passthrough hosts get no certificate: the backend terminates TLS.
        assert!(!serde_json::to_string(&v["apps"]["tls"])
            .unwrap()
            .contains("pg.example.com"));
    }

    #[test]
    fn backend_tls_and_proxy_protocol_reach_the_transport() {
        let v: Value = serde_json::from_str(&render_multi().json).unwrap();
        let proxy = &v["apps"]["http"]["servers"]["kcore"]["routes"][0]["handle"][0];
        let transport = if proxy["handler"] == "reverse_proxy" {
            &proxy["transport"]
        } else {
            &v["apps"]["http"]["servers"]["kcore"]["routes"][0]["handle"][1]["transport"]
        };
        assert_eq!(transport["protocol"], "http");
        assert_eq!(transport["proxy_protocol"], "v2");
        assert_eq!(transport["tls"]["server_name"], "payments.internal");
        assert_eq!(transport["tls"]["ca"]["provider"], "inline");
        assert!(transport["tls"]["ca"]["trusted_ca_certs"][0]
            .as_str()
            .unwrap()
            .starts_with("MII"));
    }

    #[test]
    fn dns_only_gateway_puts_every_host_on_dns01() {
        let mut gateway = parse_gateway(GATEWAY_MULTI).unwrap();
        gateway.tls.acme.challenges = vec!["dns-01".into()];
        let routes = BTreeMap::from([("pay".to_string(), parse_route(ROUTE_PAY).unwrap())]);
        let paths = NodePaths::default();
        let out = render_caddy(&RenderInput {
            gateway: &gateway,
            routes: &routes,
            waf_policies: &BTreeMap::new(),
            backends: &BTreeMap::new(),
            paths: &paths,
            issue: true,
        })
        .unwrap();
        let v: Value = serde_json::from_str(&out.json).unwrap();
        let policies = v["apps"]["tls"]["automation"]["policies"]
            .as_array()
            .unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0]["subjects"].as_array().unwrap().len(), 2);
    }
}
