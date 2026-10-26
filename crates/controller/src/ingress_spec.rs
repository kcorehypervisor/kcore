//! Ingress resource specs: `IngressGateway`, `IngressRoute`, `WafPolicy`.
//!
//! These are the canonical camelCase documents from
//! `docs/ingress-waf-load-balancer-plan.md`. They are stored as JSON, parsed
//! strictly (unknown fields are an error, so a typo never silently drops a
//! security setting), and validated here without touching the database or the
//! network. Checks that need cluster state (node addresses, SharedFilesystem
//! existence) belong to the gRPC layer.
//!
//! Phase 1 scope: HTTP routes to `vm`, `container` and `external` backends with
//! ACME HTTP-01/TLS-ALPN-01 certificates. Fields for later phases are absent, so
//! a document that uses them is rejected rather than half-applied.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- gateway

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewaySpec {
    pub nodes: Vec<String>,
    pub listeners: Vec<Listener>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high_availability: Option<HighAvailability>,
    pub tls: GatewayTls,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_waf_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    #[serde(default)]
    pub access_log: bool,
    /// Serve `Ingress` objects of these clusters (`ingressClassName: kcore`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<KubernetesIngress>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KubernetesIngress {
    #[serde(default)]
    pub ingress_controller: bool,
    pub clusters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Listener {
    pub address: String,
    pub ports: Vec<u16>,
    #[serde(default)]
    pub http3: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HighAvailability {
    pub vip_address: String,
    pub vip_interface: String,
    pub virtual_router_id: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayTls {
    pub acme: Acme,
    /// Where certificates are kept: `local` (each node its own) or
    /// `replicated` (two nodes; the controller copies the VIP holder's store to
    /// the standby, and only a node that holds the VIP, or already has every
    /// certificate, runs ACME).
    #[serde(default = "default_storage")]
    pub storage: String,
    #[serde(default = "default_min_tls")]
    pub min_version: String,
}

fn default_storage() -> String {
    "local".into()
}

fn default_min_tls() -> String {
    "1.2".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Acme {
    pub email: String,
    /// `letsencrypt`, `letsencrypt-staging`, `zerossl`, or a directory URL.
    #[serde(default = "default_ca")]
    pub ca: String,
    #[serde(default = "default_challenges")]
    pub challenges: Vec<String>,
    /// DNS-01 provider; required for wildcard hosts and for `dns-01`. The
    /// credentials live in `/etc/kcore/ingress/dns.env` on each gateway node
    /// (written with `kctl ingress secret set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns01: Option<Dns01>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Dns01 {
    /// `cloudflare`, `route53` or `rfc2136`.
    pub provider: String,
}

/// Credentials file name the DNS-01 provider reads its environment from.
pub const DNS_SECRET_NAME: &str = "dns.env";

fn default_ca() -> String {
    "letsencrypt".into()
}

fn default_challenges() -> Vec<String> {
    vec!["http-01".into(), "tls-alpn-01".into()]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateLimit {
    pub requests_per_minute: u32,
    #[serde(default = "default_rate_key")]
    pub key: String,
}

fn default_rate_key() -> String {
    "client_ip".into()
}

// ------------------------------------------------------------------ route

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteSpec {
    pub gateway: String,
    /// `http` (default) terminates TLS and applies the WAF; `tcp` forwards raw
    /// TCP or, with `hosts`, passes TLS through by SNI.
    #[serde(default)]
    pub kind: RouteKind,
    /// HTTP: the hostnames served. TCP: SNI names to match (empty matches any).
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub paths: Vec<PathSpec>,
    /// TCP routes: the listening port. It must be one of a gateway listener's ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// TCP routes: where connections go.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<Backend>,
    /// TCP routes: send the PROXY protocol v2 header to the backend.
    #[serde(default)]
    pub proxy_protocol: bool,
    /// Requests per minute per client for this route; replaces the gateway's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<u32>,
    #[serde(default)]
    pub tls: RouteTls,
    #[serde(default)]
    pub waf: RouteWaf,
    #[serde(default)]
    pub headers: Headers,
    #[serde(default = "default_true")]
    pub redirect_http: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteKind {
    #[default]
    Http,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PathSpec {
    pub prefix: String,
    pub backends: Vec<Backend>,
    #[serde(default)]
    pub load_balancing: LoadBalancing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheck>,
    /// Re-encrypt to the backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_tls: Option<BackendTls>,
    /// Send the PROXY protocol v2 header to the backend.
    #[serde(default)]
    pub proxy_protocol: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendTls {
    /// Name to verify the backend certificate against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// `system` (public CAs) or a PEM bundle. There is no switch to skip
    /// verification: a backend with a private certificate is given its PEM.
    #[serde(default = "default_ca_bundle")]
    pub ca_bundle: String,
}

fn default_ca_bundle() -> String {
    "system".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Backend {
    Vm {
        name: String,
        port: u16,
    },
    Container {
        name: String,
        port: u16,
    },
    External {
        address: String,
    },
    /// A Service of a kcore Kubernetes cluster.
    Kubernetes {
        cluster: String,
        namespace: String,
        service: String,
        /// Service port, by name or number.
        port: PortRef,
        /// How the Service is reached; `loadBalancer` when omitted.
        #[serde(default)]
        endpoints: K8sEndpoints,
    },
}

/// A Kubernetes Service port: a name (`http`) or a number (`8080`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PortRef {
    Number(u16),
    Name(String),
}

impl std::fmt::Display for PortRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortRef::Number(n) => write!(f, "{n}"),
            PortRef::Name(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum K8sEndpoints {
    /// The Service's `status.loadBalancer.ingress` address.
    #[default]
    LoadBalancer,
    /// `nodePort` on every ready worker node.
    NodePort,
    /// Pod addresses from the Service's EndpointSlices; needs routable pod IPs.
    Pods,
}

impl Backend {
    /// Identity used to key resolved addresses; stable across renders.
    pub fn key(&self) -> String {
        match self {
            Backend::Vm { name, port } => format!("vm/{name}:{port}"),
            Backend::Container { name, port } => format!("container/{name}:{port}"),
            Backend::External { address } => format!("external/{address}"),
            Backend::Kubernetes {
                cluster,
                namespace,
                service,
                port,
                endpoints,
            } => format!("k8s/{cluster}/{namespace}/{service}:{port}:{endpoints:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoadBalancing {
    #[serde(default = "default_lb_policy")]
    pub policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub try_duration: Option<String>,
}

impl Default for LoadBalancing {
    fn default() -> Self {
        Self {
            policy: default_lb_policy(),
            try_duration: None,
        }
    }
}

fn default_lb_policy() -> String {
    "round_robin".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HealthCheck {
    pub path: String,
    #[serde(default = "default_interval")]
    pub interval: String,
    #[serde(default = "default_expect_status")]
    pub expect_status: u16,
}

fn default_interval() -> String {
    "10s".into()
}

fn default_expect_status() -> u16 {
    200
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteTls {
    /// Phase 1 supports `acme` only.
    #[serde(default = "default_issuer")]
    pub issuer: String,
}

impl Default for RouteTls {
    fn default() -> Self {
        Self {
            issuer: default_issuer(),
        }
    }
}

fn default_issuer() -> String {
    "acme".into()
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteWaf {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// `detect`, `block` or `off`; overrides the policy's engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusions: Vec<WafExclusion>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WafExclusion {
    pub rule_id: u32,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Headers {
    #[serde(default)]
    pub hsts: bool,
    /// Only `X-Real-IP` needs rendering; Caddy always rewrites the
    /// `X-Forwarded-*` set itself because no proxy is trusted.
    #[serde(default)]
    pub forwarded: Vec<String>,
}

// -------------------------------------------------------------------- WAF

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WafPolicySpec {
    /// `detect` or `block`.
    #[serde(default = "default_engine")]
    pub engine: String,
    #[serde(default)]
    pub crs: Crs,
    #[serde(default = "default_body_limit")]
    pub request_body_limit_bytes: u64,
    #[serde(default)]
    pub response_body_inspection: bool,
    #[serde(default)]
    pub ip_allow_list: Vec<String>,
    #[serde(default)]
    pub ip_deny_list: Vec<String>,
    #[serde(default)]
    pub custom_rules: String,
    #[serde(default)]
    pub audit: Audit,
}

fn default_engine() -> String {
    "detect".into()
}

fn default_body_limit() -> u64 {
    1_048_576
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Crs {
    #[serde(default = "default_paranoia")]
    pub paranoia_level: u8,
    #[serde(default = "default_inbound")]
    pub inbound_anomaly_threshold: u32,
    #[serde(default = "default_outbound")]
    pub outbound_anomaly_threshold: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_content_types: Vec<String>,
}

impl Default for Crs {
    fn default() -> Self {
        Self {
            paranoia_level: default_paranoia(),
            inbound_anomaly_threshold: default_inbound(),
            outbound_anomaly_threshold: default_outbound(),
            allowed_methods: Vec::new(),
            allowed_content_types: Vec::new(),
        }
    }
}

fn default_paranoia() -> u8 {
    1
}

fn default_inbound() -> u32 {
    5
}

fn default_outbound() -> u32 {
    4
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Audit {
    /// `off`, `relevantOnly` or `on`.
    #[serde(default = "default_audit_engine")]
    pub engine: String,
    #[serde(default = "default_audit_parts")]
    pub parts: String,
}

impl Default for Audit {
    fn default() -> Self {
        Self {
            engine: default_audit_engine(),
            parts: default_audit_parts(),
        }
    }
}

fn default_audit_engine() -> String {
    "relevantOnly".into()
}

fn default_audit_parts() -> String {
    "ABCFHZ".into()
}

// ---------------------------------------------------------------- parsing

pub fn parse_gateway(json: &str) -> Result<GatewaySpec, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid IngressGateway spec: {e}"))
}

pub fn parse_route(json: &str) -> Result<RouteSpec, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid IngressRoute spec: {e}"))
}

pub fn parse_waf_policy(json: &str) -> Result<WafPolicySpec, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid WafPolicy spec: {e}"))
}

// -------------------------------------------------------------- validation

/// Maps an ACME `ca` setting to its directory URL.
pub fn acme_directory_url(ca: &str) -> String {
    match ca {
        "letsencrypt" => "https://acme-v02.api.letsencrypt.org/directory".into(),
        "letsencrypt-staging" => "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
        "zerossl" => "https://acme.zerossl.com/v2/DV90".into(),
        other => other.into(),
    }
}

pub fn validate_gateway(spec: &GatewaySpec) -> Result<(), String> {
    if spec.nodes.is_empty() {
        return Err("nodes must name at least one node".into());
    }
    if spec.nodes.len() > 2 {
        return Err("nodes: at most two (active/active is out of scope)".into());
    }
    let unique: BTreeSet<_> = spec.nodes.iter().collect();
    if unique.len() != spec.nodes.len() || spec.nodes.iter().any(|n| n.trim().is_empty()) {
        return Err("nodes must be non-empty and unique".into());
    }
    if spec.listeners.is_empty() {
        return Err("listeners must name at least one address".into());
    }
    let mut bound = BTreeSet::new();
    for l in &spec.listeners {
        if l.address.parse::<IpAddr>().is_err() {
            return Err(format!(
                "listener address {:?} is not an IP address",
                l.address
            ));
        }
        if l.ports.is_empty() {
            return Err(format!("listener {} needs at least one port", l.address));
        }
        for p in &l.ports {
            if *p == 0 {
                return Err("listener port 0 is not valid".into());
            }
            if !bound.insert((l.address.clone(), *p)) {
                return Err(format!("listener {}:{p} is declared twice", l.address));
            }
        }
        if l.http3 && !l.ports.contains(&443) {
            return Err(format!("listener {}: http3 needs port 443", l.address));
        }
    }
    if let Some(ha) = &spec.high_availability {
        if ha.vip_address.parse::<IpAddr>().is_err() {
            return Err("highAvailability.vipAddress is not an IP address".into());
        }
        if ha.vip_interface.trim().is_empty() {
            return Err("highAvailability.vipInterface is required".into());
        }
        if ha.virtual_router_id == 0 {
            return Err("highAvailability.virtualRouterId must be 1-255".into());
        }
        if !spec.listeners.iter().any(|l| l.address == ha.vip_address) {
            return Err("highAvailability.vipAddress must be one of the listener addresses".into());
        }
    } else if spec.nodes.len() == 2 {
        return Err("two nodes need highAvailability (a shared VIP)".into());
    }
    if spec.tls.acme.email.trim().is_empty() || !spec.tls.acme.email.contains('@') {
        return Err("tls.acme.email must be an email address".into());
    }
    if !matches!(spec.tls.min_version.as_str(), "1.2" | "1.3") {
        return Err("tls.minVersion must be \"1.2\" or \"1.3\"".into());
    }
    if spec.tls.acme.challenges.is_empty() {
        return Err("tls.acme.challenges must not be empty".into());
    }
    for c in &spec.tls.acme.challenges {
        match c.as_str() {
            "http-01" | "tls-alpn-01" => {}
            "dns-01" => {
                if spec.tls.acme.dns01.is_none() {
                    return Err("challenge dns-01 needs tls.acme.dns01.provider".into());
                }
            }
            other => return Err(format!("unknown ACME challenge {other:?}")),
        }
    }
    if let Some(d) = &spec.tls.acme.dns01 {
        if !matches!(d.provider.as_str(), "cloudflare" | "route53" | "rfc2136") {
            return Err(format!(
                "tls.acme.dns01.provider {:?} must be cloudflare, route53 or rfc2136",
                d.provider
            ));
        }
    }
    if spec.tls.acme.challenges.iter().any(|c| c == "http-01")
        && !spec.listeners.iter().any(|l| l.ports.contains(&80))
    {
        return Err("http-01 needs a listener on port 80".into());
    }
    if spec.tls.acme.challenges.iter().any(|c| c == "tls-alpn-01")
        && !spec.listeners.iter().any(|l| l.ports.contains(&443))
    {
        return Err("tls-alpn-01 needs a listener on port 443".into());
    }
    match spec.tls.storage.as_str() {
        "local" => {}
        "replicated" => {
            if spec.high_availability.is_none() {
                return Err("tls.storage replicated needs highAvailability (two nodes)".into());
            }
        }
        "shared-filesystem" => {
            return Err(
                "tls.storage shared-filesystem is not available: nodes do not mount a \
                 SharedFilesystem yet; use replicated"
                    .into(),
            )
        }
        other => return Err(format!("tls.storage {other:?} must be local or replicated")),
    }
    if let Some(k) = &spec.kubernetes {
        if k.ingress_controller && k.clusters.is_empty() {
            return Err("kubernetes.ingressController needs at least one cluster".into());
        }
        let mut seen = BTreeSet::new();
        for c in &k.clusters {
            if c.trim().is_empty() || !seen.insert(c) {
                return Err("kubernetes.clusters must be non-empty and unique".into());
            }
        }
    }
    if let Some(rl) = &spec.rate_limit {
        if rl.requests_per_minute == 0 {
            return Err("rateLimit.requestsPerMinute must be positive".into());
        }
        if rl.key != "client_ip" {
            return Err("rateLimit.key: only client_ip is supported".into());
        }
    }
    Ok(())
}

pub fn validate_route(spec: &RouteSpec) -> Result<(), String> {
    if spec.gateway.trim().is_empty() {
        return Err("gateway is required".into());
    }
    let mut seen = BTreeSet::new();
    for h in &spec.hosts {
        validate_hostname(h)?;
        if !seen.insert(h.to_ascii_lowercase()) {
            return Err(format!("host {h} is listed twice"));
        }
    }
    match spec.kind {
        RouteKind::Http => validate_http_route(spec),
        RouteKind::Tcp => validate_tcp_route(spec),
    }
}

fn validate_http_route(spec: &RouteSpec) -> Result<(), String> {
    if spec.hosts.is_empty() {
        return Err("hosts must name at least one hostname".into());
    }
    if spec.port.is_some() || !spec.backends.is_empty() || spec.proxy_protocol {
        return Err("port, backends and proxyProtocol belong to kind: tcp routes".into());
    }
    if spec.rate_limit == Some(0) {
        return Err("rateLimit must be positive".into());
    }
    if spec.paths.is_empty() {
        return Err("paths must contain at least one prefix".into());
    }
    let mut prefixes = BTreeSet::new();
    for p in &spec.paths {
        validate_path(p)?;
        if !prefixes.insert(p.prefix.as_str()) {
            return Err(format!("path prefix {} is listed twice", p.prefix));
        }
    }
    if spec.tls.issuer != "acme" {
        return Err(format!(
            "tls.issuer {:?} is not supported yet (only \"acme\")",
            spec.tls.issuer
        ));
    }
    if let Some(mode) = &spec.waf.mode {
        if !matches!(mode.as_str(), "detect" | "block" | "off") {
            return Err("waf.mode must be detect, block or off".into());
        }
    }
    for e in &spec.waf.exclusions {
        if !e.path.starts_with('/') {
            return Err("waf.exclusions[].path must start with /".into());
        }
        if let Some(arg) = &e.arg {
            if arg.is_empty()
                || !arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
            {
                return Err(format!(
                    "waf.exclusions[].arg {arg:?} is not a plain argument name"
                ));
            }
        }
    }
    for f in &spec.headers.forwarded {
        if !matches!(
            f.as_str(),
            "X-Forwarded-For" | "X-Forwarded-Proto" | "X-Real-IP"
        ) {
            return Err(format!("headers.forwarded: unknown header {f:?}"));
        }
    }
    Ok(())
}

fn validate_tcp_route(spec: &RouteSpec) -> Result<(), String> {
    let port = spec.port.ok_or("tcp routes need a port")?;
    if port == 0 || port == 80 || port == 443 {
        return Err("tcp route port must not be 0, 80 or 443 (those carry HTTP)".into());
    }
    if !spec.paths.is_empty() {
        return Err("tcp routes have no paths".into());
    }
    if spec.waf.policy.is_some() || spec.waf.mode.is_some() || !spec.waf.exclusions.is_empty() {
        return Err("tcp routes carry no WAF".into());
    }
    if spec.headers.hsts || !spec.headers.forwarded.is_empty() {
        return Err("tcp routes have no HTTP headers".into());
    }
    if spec.backends.is_empty() {
        return Err("tcp routes need at least one backend".into());
    }
    for b in &spec.backends {
        validate_backend("tcp route", b)?;
    }
    Ok(())
}

/// True for `*.example.com` style hosts.
pub fn is_wildcard_host(h: &str) -> bool {
    h.starts_with("*.")
}

fn validate_hostname(h: &str) -> Result<(), String> {
    let name = h.strip_prefix("*.").unwrap_or(h);
    if is_wildcard_host(h) && !name.contains('.') {
        return Err(format!(
            "wildcard host {h} needs a registered domain after *."
        ));
    }
    let ok = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if !ok || name.parse::<IpAddr>().is_ok() {
        return Err(format!("{h:?} is not a valid DNS hostname"));
    }
    Ok(())
}

fn validate_backend(what: &str, b: &Backend) -> Result<(), String> {
    match b {
        Backend::Vm { name, port } | Backend::Container { name, port } => {
            if name.trim().is_empty() || *port == 0 {
                return Err(format!("{what}: backend needs a name and a port"));
            }
        }
        Backend::Kubernetes {
            cluster,
            namespace,
            service,
            port,
            ..
        } => {
            let dns_label = |v: &str| {
                !v.is_empty()
                    && v.len() <= 63
                    && v.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            };
            if cluster.trim().is_empty() || !dns_label(namespace) || !dns_label(service) {
                return Err(format!(
                    "{what}: kubernetes backend needs a cluster, a namespace and a service name"
                ));
            }
            match port {
                PortRef::Number(0) => {
                    return Err(format!("{what}: kubernetes backend port must not be 0"))
                }
                PortRef::Name(n) if !dns_label(n) => {
                    return Err(format!(
                        "{what}: kubernetes backend port name {n:?} is not valid"
                    ))
                }
                _ => {}
            }
        }
        Backend::External { address } => {
            let ok = address
                .rsplit_once(':')
                .map(|(host, port)| {
                    !host.is_empty() && port.parse::<u16>().map(|n| n != 0).unwrap_or(false)
                })
                .unwrap_or(false);
            if !ok {
                return Err(format!(
                    "{what}: external address {address:?} must be host:port"
                ));
            }
        }
    }
    Ok(())
}

fn validate_path(p: &PathSpec) -> Result<(), String> {
    if !p.prefix.starts_with('/') || p.prefix.contains(['*', '?', '#', ' ']) {
        return Err(format!(
            "path prefix {:?} must start with / and contain no wildcards",
            p.prefix
        ));
    }
    if p.backends.is_empty() {
        return Err(format!("path {} needs at least one backend", p.prefix));
    }
    for b in &p.backends {
        validate_backend(&format!("path {}", p.prefix), b)?;
    }
    if !matches!(
        p.load_balancing.policy.as_str(),
        "round_robin" | "least_conn" | "ip_hash" | "cookie" | "first"
    ) {
        return Err(format!(
            "path {}: unknown loadBalancing.policy {:?}",
            p.prefix, p.load_balancing.policy
        ));
    }
    for d in p
        .load_balancing
        .try_duration
        .iter()
        .chain(p.health_check.iter().map(|h| &h.interval))
    {
        validate_duration(d)?;
    }
    if let Some(h) = &p.health_check {
        if !h.path.starts_with('/') {
            return Err(format!(
                "path {}: healthCheck.path must start with /",
                p.prefix
            ));
        }
        if !(100..=599).contains(&h.expect_status) {
            return Err(format!(
                "path {}: healthCheck.expectStatus is not an HTTP status",
                p.prefix
            ));
        }
    }
    if let Some(t) = &p.backend_tls {
        validate_backend_tls(&p.prefix, t)?;
    }
    Ok(())
}

fn validate_backend_tls(prefix: &str, t: &BackendTls) -> Result<(), String> {
    if let Some(n) = &t.server_name {
        validate_hostname(n).map_err(|e| format!("path {prefix}: backendTls.serverName: {e}"))?;
    }
    match t.ca_bundle.as_str() {
        "system" => Ok(()),
        "kcore-pki" => Err(format!(
            "path {prefix}: backendTls.caBundle kcore-pki is not available yet; give the PEM"
        )),
        pem_text => {
            let certs = pem_certificates(pem_text);
            if certs.is_empty() {
                Err(format!(
                    "path {prefix}: backendTls.caBundle must be \"system\" or a PEM certificate bundle"
                ))
            } else {
                Ok(())
            }
        }
    }
}

/// DER bytes of every certificate in a PEM bundle.
pub fn pem_certificates(text: &str) -> Vec<Vec<u8>> {
    pem::parse_many(text)
        .map(|blocks| {
            blocks
                .into_iter()
                .filter(|b| b.tag() == "CERTIFICATE")
                .map(|b| b.into_contents())
                .collect()
        })
        .unwrap_or_default()
}

fn validate_duration(d: &str) -> Result<(), String> {
    let digits = d.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &d[digits.len()..];
    if digits.is_empty()
        || !digits.chars().all(|c| c.is_ascii_digit())
        || !matches!(unit, "s" | "m" | "h")
    {
        return Err(format!("duration {d:?} must look like 5s, 2m or 1h"));
    }
    Ok(())
}

pub fn validate_waf_policy(spec: &WafPolicySpec) -> Result<(), String> {
    if !matches!(spec.engine.as_str(), "detect" | "block") {
        return Err("engine must be detect or block".into());
    }
    if !(1..=4).contains(&spec.crs.paranoia_level) {
        return Err("crs.paranoiaLevel must be 1-4".into());
    }
    if spec.crs.inbound_anomaly_threshold == 0 || spec.crs.outbound_anomaly_threshold == 0 {
        return Err("crs anomaly thresholds must be positive".into());
    }
    for m in &spec.crs.allowed_methods {
        if m.is_empty() || !m.chars().all(|c| c.is_ascii_uppercase()) {
            return Err(format!("crs.allowedMethods: {m:?} is not an HTTP method"));
        }
    }
    for ct in &spec.crs.allowed_content_types {
        if ct.contains(['|', ' ', '"', '\n']) || !ct.contains('/') {
            return Err(format!(
                "crs.allowedContentTypes: {ct:?} is not a media type"
            ));
        }
    }
    if spec.request_body_limit_bytes == 0 {
        return Err("requestBodyLimitBytes must be positive".into());
    }
    for cidr in spec.ip_allow_list.iter().chain(&spec.ip_deny_list) {
        validate_cidr(cidr)?;
    }
    if !matches!(spec.audit.engine.as_str(), "off" | "relevantOnly" | "on") {
        return Err("audit.engine must be off, relevantOnly or on".into());
    }
    if spec.audit.parts.is_empty()
        || !spec
            .audit
            .parts
            .chars()
            .all(|c| c.is_ascii_uppercase() && c != 'J')
    {
        return Err("audit.parts must be audit-log part letters such as ABCFHZ".into());
    }
    Ok(())
}

fn validate_cidr(s: &str) -> Result<(), String> {
    let (addr, prefix) = s.split_once('/').unwrap_or((s, ""));
    let ip: IpAddr = addr
        .parse()
        .map_err(|_| format!("{s:?} is not an IP address or CIDR"))?;
    if !prefix.is_empty() {
        let max = if ip.is_ipv4() { 32 } else { 128 };
        match prefix.parse::<u8>() {
            Ok(n) if n <= max => {}
            _ => return Err(format!("{s:?} has an invalid prefix length")),
        }
    }
    Ok(())
}

/// Hostnames must be unique across the HTTP routes of one gateway, and a TCP
/// port is shared only by routes that match different SNI names.
pub fn check_host_conflicts(routes: &BTreeMap<String, RouteSpec>) -> Result<(), String> {
    let mut owner: BTreeMap<String, &str> = BTreeMap::new();
    let mut tcp: BTreeMap<u16, Vec<(&str, &RouteSpec)>> = BTreeMap::new();
    for (name, r) in routes {
        if r.kind == RouteKind::Tcp {
            if let Some(port) = r.port {
                tcp.entry(port).or_default().push((name, r));
            }
            continue;
        }
        for h in &r.hosts {
            if let Some(prev) = owner.insert(h.to_ascii_lowercase(), name) {
                return Err(format!("host {h} is used by routes {prev} and {name}"));
            }
        }
    }
    for (port, users) in &tcp {
        let mut sni: BTreeSet<String> = BTreeSet::new();
        for (name, r) in users {
            if users.len() > 1 && r.hosts.is_empty() {
                return Err(format!(
                    "tcp route {name} matches every connection on port {port}, which another route also uses"
                ));
            }
            for h in &r.hosts {
                if !sni.insert(h.to_ascii_lowercase()) {
                    return Err(format!("tcp port {port}: SNI {h} is used twice"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway() -> GatewaySpec {
        parse_gateway(
            r#"{
              "nodes": ["node-105"],
              "listeners": [{"address": "203.0.113.10", "ports": [80, 443], "http3": true}],
              "tls": {"acme": {"email": "ops@example.com"}}
            }"#,
        )
        .unwrap()
    }

    fn route() -> RouteSpec {
        parse_route(
            r#"{
              "gateway": "edge",
              "hosts": ["shop.example.com"],
              "paths": [{"prefix": "/", "backends": [{"kind": "vm", "name": "web", "port": 8080}]}]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn defaults_apply_and_validate() {
        let g = gateway();
        assert_eq!(g.tls.acme.ca, "letsencrypt");
        assert_eq!(g.tls.min_version, "1.2");
        validate_gateway(&g).unwrap();
        let r = route();
        assert!(r.redirect_http);
        validate_route(&r).unwrap();
        validate_waf_policy(&parse_waf_policy("{}").unwrap()).unwrap();
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = parse_gateway(
            r#"{"nodes":["a"],"listeners":[],"tls":{"acme":{"email":"a@b"}},"wafMode":"block"}"#,
        )
        .unwrap_err();
        assert!(err.contains("wafMode"), "{err}");
        // A later-phase feature is refused, not ignored.
        assert!(parse_route(
            r#"{"gateway":"g","hosts":["a.b"],"paths":[{"prefix":"/","backends":[],"backendTls":{"enabled":true}}]}"#
        )
        .is_err());
    }

    #[test]
    fn gateway_rules() {
        let mut g = gateway();
        g.nodes = vec!["a".into(), "b".into(), "c".into()];
        assert!(validate_gateway(&g).unwrap_err().contains("at most two"));

        let mut g = gateway();
        g.nodes = vec!["a".into(), "b".into()];
        assert!(validate_gateway(&g)
            .unwrap_err()
            .contains("highAvailability"));
        g.high_availability = Some(HighAvailability {
            vip_address: "203.0.113.99".into(),
            vip_interface: "eno1".into(),
            virtual_router_id: 61,
        });
        assert!(validate_gateway(&g)
            .unwrap_err()
            .contains("listener addresses"));
        g.high_availability.as_mut().unwrap().vip_address = "203.0.113.10".into();
        validate_gateway(&g).unwrap();

        let mut g = gateway();
        g.listeners[0].ports = vec![443];
        assert!(validate_gateway(&g).unwrap_err().contains("port 80"));

        let mut g = gateway();
        g.listeners[0].address = "not-an-ip".into();
        assert!(validate_gateway(&g).is_err());

        let mut g = gateway();
        g.tls.acme.challenges = vec!["dns-01".into()];
        assert!(validate_gateway(&g).unwrap_err().contains("dns01.provider"));
        g.tls.acme.dns01 = Some(Dns01 {
            provider: "cloudflare".into(),
        });
        validate_gateway(&g).unwrap();
        g.tls.acme.dns01 = Some(Dns01 {
            provider: "godaddy".into(),
        });
        assert!(validate_gateway(&g).is_err());
    }

    #[test]
    fn route_rules() {
        let mut r = route();
        r.hosts = vec!["*.example.com".into()];
        validate_route(&r).unwrap();
        r.hosts = vec!["*.com".into()];
        assert!(validate_route(&r).is_err());

        let mut r = route();
        r.hosts = vec!["bad host".into()];
        assert!(validate_route(&r).is_err());

        let mut r = route();
        r.paths[0].prefix = "api".into();
        assert!(validate_route(&r).is_err());

        let mut r = route();
        r.paths[0].backends = vec![Backend::External {
            address: "10.0.0.1".into(),
        }];
        assert!(validate_route(&r).unwrap_err().contains("host:port"));

        let mut r = route();
        r.paths[0].load_balancing.policy = "random".into();
        assert!(validate_route(&r).is_err());

        let mut r = route();
        r.waf.mode = Some("blok".into());
        assert!(validate_route(&r).is_err());

        let mut r = route();
        r.tls.issuer = "none".into();
        assert!(validate_route(&r).is_err());
    }

    #[test]
    fn waf_policy_rules() {
        let mut p = parse_waf_policy("{}").unwrap();
        p.crs.paranoia_level = 5;
        assert!(validate_waf_policy(&p).is_err());
        let mut p = parse_waf_policy("{}").unwrap();
        p.ip_deny_list = vec!["10.0.0.0/33".into()];
        assert!(validate_waf_policy(&p).is_err());
        p.ip_deny_list = vec![
            "10.0.0.0/8".into(),
            "2001:db8::/32".into(),
            "192.0.2.1".into(),
        ];
        validate_waf_policy(&p).unwrap();
    }

    #[test]
    fn host_conflicts_are_found() {
        let mut routes = BTreeMap::new();
        routes.insert("a".to_string(), route());
        let mut other = route();
        other.hosts = vec!["SHOP.example.com".into()];
        routes.insert("b".to_string(), other);
        assert!(check_host_conflicts(&routes)
            .unwrap_err()
            .contains("routes a and b"));
    }

    #[test]
    fn spec_round_trips_as_json() {
        let r = route();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(parse_route(&json).unwrap(), r);
    }

    fn example_spec(yaml: &str) -> String {
        let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        serde_json::to_string(&doc["spec"]).unwrap()
    }

    #[test]
    fn shipped_examples_parse_and_validate() {
        const POLICY: &str = include_str!("../../../examples/ingress/waf-policy.yaml");
        const GATEWAY: &str = include_str!("../../../examples/ingress/gateway.yaml");
        const GATEWAY_HA: &str = include_str!("../../../examples/ingress/gateway-ha.yaml");
        const ROUTE: &str = include_str!("../../../examples/ingress/route.yaml");
        const ROUTE_K8S: &str = include_str!("../../../examples/ingress/route-k8s.yaml");
        const ROUTE_TCP: &str = include_str!("../../../examples/ingress/route-tcp.yaml");
        const ROUTE_WILD: &str = include_str!("../../../examples/ingress/route-wildcard.yaml");
        validate_waf_policy(&parse_waf_policy(&example_spec(POLICY)).unwrap()).unwrap();
        for g in [GATEWAY, GATEWAY_HA] {
            validate_gateway(&parse_gateway(&example_spec(g)).unwrap()).unwrap();
        }
        for r in [ROUTE, ROUTE_K8S, ROUTE_TCP, ROUTE_WILD] {
            validate_route(&parse_route(&example_spec(r)).unwrap()).unwrap();
        }
    }

    fn tcp_route() -> RouteSpec {
        parse_route(
            r#"{"gateway":"edge","kind":"tcp","port":5432,
                "backends":[{"kind":"external","address":"10.0.0.9:5432"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn tcp_routes_have_their_own_rules() {
        validate_route(&tcp_route()).unwrap();
        let mut r = tcp_route();
        r.port = None;
        assert!(validate_route(&r).unwrap_err().contains("port"));
        let mut r = tcp_route();
        r.port = Some(443);
        assert!(validate_route(&r).is_err());
        let mut r = tcp_route();
        r.backends.clear();
        assert!(validate_route(&r).is_err());
        let mut r = tcp_route();
        r.waf.mode = Some("block".into());
        assert!(validate_route(&r).unwrap_err().contains("no WAF"));
        let mut r = tcp_route();
        r.hosts = vec!["db.example.com".into()];
        validate_route(&r).unwrap();
        // HTTP routes cannot borrow TCP fields.
        let mut h = route();
        h.port = Some(5432);
        assert!(validate_route(&h).is_err());
    }

    #[test]
    fn tcp_port_sharing_needs_distinct_sni() {
        let mut a = tcp_route();
        a.hosts = vec!["a.example.com".into()];
        let mut b = tcp_route();
        b.hosts = vec!["b.example.com".into()];
        let routes = BTreeMap::from([("a".to_string(), a.clone()), ("b".to_string(), b)]);
        check_host_conflicts(&routes).unwrap();
        let catch_all = BTreeMap::from([("a".to_string(), a), ("c".to_string(), tcp_route())]);
        assert!(check_host_conflicts(&catch_all)
            .unwrap_err()
            .contains("matches every connection"));
    }

    #[test]
    fn backend_tls_requires_a_real_pem_or_system() {
        let mut r = route();
        r.paths[0].backend_tls = Some(BackendTls {
            server_name: Some("payments.internal".into()),
            ca_bundle: "system".into(),
        });
        validate_route(&r).unwrap();
        for bad in ["kcore-pki", "not a pem", ""] {
            r.paths[0].backend_tls = Some(BackendTls {
                server_name: None,
                ca_bundle: bad.into(),
            });
            assert!(validate_route(&r).is_err(), "{bad:?}");
        }
        r.paths[0].backend_tls = Some(BackendTls {
            server_name: None,
            ca_bundle: include_str!("../../../tests/fixtures/ingress/backend-ca.pem").into(),
        });
        validate_route(&r).unwrap();
    }

    #[test]
    fn storage_modes_are_checked() {
        let mut g = gateway();
        g.tls.storage = "replicated".into();
        assert!(validate_gateway(&g)
            .unwrap_err()
            .contains("highAvailability"));
        g.nodes = vec!["a".into(), "b".into()];
        g.high_availability = Some(HighAvailability {
            vip_address: "203.0.113.10".into(),
            vip_interface: "eno1".into(),
            virtual_router_id: 61,
        });
        validate_gateway(&g).unwrap();
        g.tls.storage = "shared-filesystem".into();
        assert!(validate_gateway(&g).unwrap_err().contains("not available"));
        g.tls.storage = "ceph".into();
        assert!(validate_gateway(&g).is_err());
    }

    #[test]
    fn kubernetes_backends_and_controller_settings_validate() {
        let mut r = route();
        r.paths[0].backends = vec![parse_backend(
            r#"{"kind":"kubernetes","cluster":"lab","namespace":"shop","service":"api","port":"http"}"#,
        )];
        validate_route(&r).unwrap();
        r.paths[0].backends = vec![parse_backend(
            r#"{"kind":"kubernetes","cluster":"lab","namespace":"Shop","service":"api","port":8080,"endpoints":"nodePort"}"#,
        )];
        assert!(validate_route(&r).is_err(), "uppercase namespace");
        r.paths[0].backends = vec![parse_backend(
            r#"{"kind":"kubernetes","cluster":"lab","namespace":"shop","service":"api","port":0}"#,
        )];
        assert!(validate_route(&r).is_err(), "port 0");

        let mut g = gateway();
        g.kubernetes = Some(KubernetesIngress {
            ingress_controller: true,
            clusters: vec![],
        });
        assert!(validate_gateway(&g).is_err());
        g.kubernetes = Some(KubernetesIngress {
            ingress_controller: true,
            clusters: vec!["lab".into()],
        });
        validate_gateway(&g).unwrap();
    }

    fn parse_backend(json: &str) -> Backend {
        serde_json::from_str(json).unwrap()
    }
}
