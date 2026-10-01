//! Push delivery of cluster events to operator-configured HTTP endpoints.
//!
//! The controller observes three transitions and, when `webhooks` is configured,
//! POSTs a JSON body to each matching endpoint:
//!
//! - `node.heartbeat.missed` — a ready node passed the heartbeat deadline
//! - `cert.expiry.warning` — a certificate is inside `certRotation.warnBeforeDays`
//! - `vm.state.changed` — a VM's stored runtime state actually changed
//!
//! Delivery is asynchronous. Emitters use `try_send` so a slow receiver cannot
//! stall heartbeats or VM sync. Failures are logged and retried; they never
//! fail the RPC that observed the event.
//!
//! When `secret` is set, the raw body is signed with HMAC-SHA256 and sent as
//! `X-Kcore-Signature: sha256=<hex>`. Receivers must verify that header against
//! the bytes they read, not a re-encoded JSON value. Redirects are not followed.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use aws_lc_rs::hmac;
use rustls::pki_types::{CertificateDer, ServerName};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use url::Url;

use crate::config::{WebhookEndpoint, WebhooksConfig};

pub const EVENT_NODE_HEARTBEAT_MISSED: &str = "node.heartbeat.missed";
pub const EVENT_CERT_EXPIRY_WARNING: &str = "cert.expiry.warning";
pub const EVENT_VM_STATE_CHANGED: &str = "vm.state.changed";

pub const KNOWN_EVENTS: &[&str] = &[
    EVENT_NODE_HEARTBEAT_MISSED,
    EVENT_CERT_EXPIRY_WARNING,
    EVENT_VM_STATE_CHANGED,
];

const QUEUE_CAPACITY: usize = 256;

pub fn is_known_event(name: &str) -> bool {
    KNOWN_EVENTS.contains(&name)
}

/// Identity stamped on every event so a receiver shared by several controllers
/// can tell the copies apart. Prefers `replication.controllerId`.
#[derive(Clone, Debug)]
pub struct EventSource {
    pub controller_id: String,
}

impl EventSource {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let from_replication = cfg
            .replication
            .as_ref()
            .map(|rep| rep.controller_id.trim().to_string())
            .filter(|id| !id.is_empty());
        Self {
            controller_id: from_replication.unwrap_or_else(|| cfg.listen_addr.clone()),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub occurred_at: String,
    pub controller_id: String,
    pub resource: String,
    pub data: Value,
}

/// Remembers certificate warnings so an hourly reconcile does not POST the
/// same expiry on every tick. A slot that leaves the warning window is
/// forgotten, so the next entry emits again (for example after a rotation
/// that later drifts back into the window).
#[derive(Debug, Default)]
pub struct WarningDedup {
    emitted: HashMap<String, String>,
    live: HashSet<String>,
}

impl WarningDedup {
    /// Returns true when `fingerprint` has not already been emitted for `slot`
    /// during the current stay in the warning window.
    pub fn claim(&mut self, slot: &str, fingerprint: &str) -> bool {
        self.live.insert(slot.to_string());
        match self.emitted.get(slot) {
            Some(prev) if prev == fingerprint => false,
            _ => {
                self.emitted
                    .insert(slot.to_string(), fingerprint.to_string());
                true
            }
        }
    }

    /// Drop warnings that were not claimed on this pass.
    pub fn finish_pass(&mut self) {
        self.emitted.retain(|slot, _| self.live.contains(slot));
        self.live.clear();
    }
}

#[derive(Clone)]
pub struct Dispatcher {
    tx: Option<mpsc::Sender<Event>>,
    source: EventSource,
}

impl Dispatcher {
    pub fn noop() -> Self {
        Self {
            tx: None,
            source: EventSource {
                controller_id: String::new(),
            },
        }
    }

    /// Starts the delivery worker when at least one endpoint is active.
    pub fn spawn(cfg: &WebhooksConfig, source: EventSource) -> Self {
        if !cfg.enabled {
            if !cfg.endpoints.is_empty() {
                warn!(
                    endpoints = cfg.endpoints.len(),
                    "webhooks.enabled is false; events will not be delivered"
                );
            }
            return Self { tx: None, source };
        }
        if cfg.endpoints.is_empty() {
            return Self { tx: None, source };
        }
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let cfg = cfg.clone();
        info!(
            endpoints = cfg.endpoints.len(),
            timeout_secs = cfg.timeout_secs,
            max_retries = cfg.max_retries,
            "webhook delivery enabled"
        );
        tokio::spawn(async move {
            run_worker(cfg, rx).await;
        });
        Self {
            tx: Some(tx),
            source,
        }
    }

    pub fn emit(&self, event_type: &str, resource: impl Into<String>, data: Value) {
        let Some(tx) = &self.tx else {
            return;
        };
        let event = Event {
            id: uuid::Uuid::new_v4().to_string(),
            event_type: event_type.to_string(),
            occurred_at: now_rfc3339(),
            controller_id: self.source.controller_id.clone(),
            resource: resource.into(),
            data,
        };
        match tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(event)) => {
                warn!(
                    event_type = %event.event_type,
                    resource = %event.resource,
                    "webhook queue is full; dropping event"
                );
            }
            Err(mpsc::error::TrySendError::Closed(event)) => {
                warn!(
                    event_type = %event.event_type,
                    resource = %event.resource,
                    "webhook worker has stopped; dropping event"
                );
            }
        }
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// `sha256=<lowercase hex>` HMAC-SHA256 of `body` under `secret`.
pub fn sign_body(secret: &str, body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, body);
    format!("sha256={}", hex_encode(tag.as_ref()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWebhookUrl {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub host_header: String,
    pub path_and_query: String,
    tls_name: TlsName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TlsName {
    Domain(String),
    Ip(std::net::IpAddr),
}

pub fn parse_webhook_url(raw: &str) -> Result<ParsedWebhookUrl, String> {
    let url = Url::parse(raw).map_err(|err| err.to_string())?;
    let https = match url.scheme() {
        "https" => true,
        "http" => false,
        other => {
            return Err(format!(
                "scheme '{other}' is not allowed (use http or https)"
            ))
        }
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err("userinfo is not allowed in a webhook URL".to_string());
    }
    let host = url
        .host()
        .ok_or_else(|| "webhook URL is missing a host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "webhook URL is missing a port".to_string())?;
    let mut path_and_query = url.path().to_string();
    if path_and_query.is_empty() {
        path_and_query.push('/');
    }
    if let Some(query) = url.query() {
        path_and_query.push('?');
        path_and_query.push_str(query);
    }
    if path_and_query
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == b' ')
    {
        return Err("webhook URL path contains whitespace or a line break".to_string());
    }
    let (host_str, display_host) = match &host {
        url::Host::Domain(domain) => ((*domain).to_string(), (*domain).to_string()),
        url::Host::Ipv4(ip) => {
            let text = ip.to_string();
            (text.clone(), text)
        }
        url::Host::Ipv6(ip) => (ip.to_string(), format!("[{ip}]")),
    };
    let default_port = if https { 443 } else { 80 };
    let host_header = if port == default_port {
        display_host
    } else {
        format!("{display_host}:{port}")
    };
    let tls_name = match host {
        url::Host::Domain(domain) => TlsName::Domain(domain.to_string()),
        url::Host::Ipv4(ip) => TlsName::Ip(std::net::IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => TlsName::Ip(std::net::IpAddr::V6(ip)),
    };
    Ok(ParsedWebhookUrl {
        https,
        host: host_str,
        port,
        host_header,
        path_and_query,
        tls_name,
    })
}

fn endpoint_wants(endpoint: &WebhookEndpoint, event_type: &str) -> bool {
    endpoint.events.is_empty() || endpoint.events.iter().any(|name| name == event_type)
}

/// Retry network failures and statuses that mean "try again". A 4xx other
/// than 408/429 will not succeed on a retry of the same body.
pub fn should_retry(status: Option<u16>) -> bool {
    match status {
        None => true,
        Some(408 | 429) => true,
        Some(status) if (500..600).contains(&status) => true,
        Some(_) => false,
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(200u64.saturating_mul(1u64 << attempt.min(5)))
}

async fn run_worker(cfg: WebhooksConfig, mut rx: mpsc::Receiver<Event>) {
    let timeout = Duration::from_secs(cfg.timeout_secs);
    while let Some(event) = rx.recv().await {
        let body = match serde_json::to_vec(&event) {
            Ok(body) => body,
            Err(err) => {
                warn!(error = %err, "failed to encode webhook event");
                continue;
            }
        };
        let mut tasks = tokio::task::JoinSet::new();
        for endpoint in cfg
            .endpoints
            .iter()
            .filter(|endpoint| endpoint_wants(endpoint, &event.event_type))
        {
            let endpoint = endpoint.clone();
            let body = body.clone();
            let event_type = event.event_type.clone();
            let resource = event.resource.clone();
            let event_id = event.id.clone();
            let retries = cfg.max_retries;
            tasks.spawn(async move {
                let mut headers = vec![
                    ("X-Kcore-Event".to_string(), event_type.clone()),
                    ("X-Kcore-Delivery".to_string(), event_id),
                ];
                if !endpoint.secret.is_empty() {
                    headers.push((
                        "X-Kcore-Signature".to_string(),
                        sign_body(&endpoint.secret, &body),
                    ));
                }
                let name = endpoint.name.clone();
                match deliver(&endpoint, &body, &headers, timeout, retries).await {
                    Ok(()) => debug!(
                        endpoint = %name,
                        event_type = %event_type,
                        resource = %resource,
                        "webhook delivered"
                    ),
                    Err(err) => warn!(
                        endpoint = %name,
                        event_type = %event_type,
                        resource = %resource,
                        error = %err,
                        "webhook delivery failed"
                    ),
                }
            });
        }
        while tasks.join_next().await.is_some() {}
    }
}

async fn deliver(
    endpoint: &WebhookEndpoint,
    body: &[u8],
    headers: &[(String, String)],
    timeout: Duration,
    max_retries: u32,
) -> Result<(), String> {
    let attempts = max_retries.saturating_add(1);
    let mut last_error = String::new();
    for attempt in 0..attempts {
        let result = tokio::time::timeout(
            timeout,
            post_once(&endpoint.url, &endpoint.ca_file, body, headers),
        )
        .await;
        match result {
            Ok(Ok(status)) if (200..300).contains(&status) => return Ok(()),
            Ok(Ok(status)) if !should_retry(Some(status)) => {
                return Err(format!("HTTP {status}"));
            }
            Ok(Ok(status)) => last_error = format!("HTTP {status}"),
            Ok(Err(err)) => last_error = err,
            Err(_) => last_error = format!("timed out after {}s", timeout.as_secs()),
        }
        if attempt + 1 == attempts {
            break;
        }
        tokio::time::sleep(backoff(attempt)).await;
    }
    Err(last_error)
}

async fn post_once(
    raw_url: &str,
    ca_file: &str,
    body: &[u8],
    headers: &[(String, String)],
) -> Result<u16, String> {
    let target = parse_webhook_url(raw_url)?;
    let request = build_request(&target, body, headers)?;
    if target.https {
        return post_https(&target, ca_file, &request).await;
    }
    let address = socket_addr(&target);
    let mut stream = TcpStream::connect(&address)
        .await
        .map_err(|err| format!("connect {address}: {err}"))?;
    let _ = stream.set_nodelay(true);
    write_and_status(&mut stream, &request).await
}

fn socket_addr(target: &ParsedWebhookUrl) -> String {
    if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    }
}

fn build_request(
    target: &ParsedWebhookUrl,
    body: &[u8],
    headers: &[(String, String)],
) -> Result<Vec<u8>, String> {
    for (name, value) in headers {
        if name
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n' || byte == b':')
            || value.bytes().any(|byte| byte == b'\r' || byte == b'\n')
        {
            return Err("refusing to send a header containing CR, LF, or ':'".to_string());
        }
    }
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: kcore-controller\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n",
        path = target.path_and_query,
        host = target.host_header,
        len = body.len(),
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    Ok(bytes)
}

async fn write_and_status<S>(stream: &mut S, request: &[u8]) -> Result<u16, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(request)
        .await
        .map_err(|err| format!("write: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| format!("flush: {err}"))?;
    read_http_status(stream).await
}

async fn read_http_status<S>(stream: &mut S) -> Result<u16, String>
where
    S: AsyncRead + Unpin,
{
    let mut buf = [0u8; 1024];
    let mut filled = 0usize;
    loop {
        if filled == buf.len() {
            return Err("HTTP status line exceeded 1024 bytes".to_string());
        }
        let n = stream
            .read(&mut buf[filled..])
            .await
            .map_err(|err| format!("read: {err}"))?;
        if n == 0 {
            return Err("connection closed before an HTTP status".to_string());
        }
        filled += n;
        if let Some(pos) = buf[..filled].windows(2).position(|w| w == b"\r\n") {
            return parse_status(&buf[..pos]);
        }
    }
}

fn parse_status(line: &[u8]) -> Result<u16, String> {
    let text =
        std::str::from_utf8(line).map_err(|_| "HTTP status line is not UTF-8".to_string())?;
    let mut parts = text.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(format!("not an HTTP response: {text}"));
    }
    let code = parts.next().unwrap_or("");
    code.parse::<u16>()
        .map_err(|_| format!("bad HTTP status code in '{text}'"))
}

fn ensure_crypto_provider() {
    // `main` installs the FIPS suite list first. A second install fails, which
    // is what we want in production. Tests that open TLS need a provider.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn tls_client_config(ca_file: &str) -> Result<rustls::ClientConfig, String> {
    ensure_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if roots.is_empty() {
        return Err("built-in HTTPS trust anchors failed to load".to_string());
    }
    if !ca_file.is_empty() {
        let pem_bytes =
            std::fs::read(ca_file).map_err(|err| format!("reading {ca_file}: {err}"))?;
        let parsed =
            pem::parse_many(pem_bytes).map_err(|err| format!("parsing {ca_file}: {err}"))?;
        let mut added_extra = 0usize;
        for item in parsed {
            if item.tag() != "CERTIFICATE" {
                continue;
            }
            roots
                .add(CertificateDer::from(item.contents().to_vec()))
                .map_err(|err| format!("adding CA from {ca_file}: {err}"))?;
            added_extra += 1;
        }
        if added_extra == 0 {
            return Err(format!("{ca_file} contains no CERTIFICATE blocks"));
        }
    }
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn server_name(name: &TlsName) -> Result<ServerName<'static>, String> {
    match name {
        TlsName::Domain(domain) => {
            ServerName::try_from(domain.clone()).map_err(|_| format!("invalid DNS name '{domain}'"))
        }
        TlsName::Ip(ip) => Ok(ServerName::IpAddress(rustls::pki_types::IpAddr::from(*ip))),
    }
}

async fn post_https(
    target: &ParsedWebhookUrl,
    ca_file: &str,
    request: &[u8],
) -> Result<u16, String> {
    let address = socket_addr(target);
    let stream = TcpStream::connect(&address)
        .await
        .map_err(|err| format!("connect {address}: {err}"))?;
    let _ = stream.set_nodelay(true);
    let config = Arc::new(tls_client_config(ca_file)?);
    let connector = tokio_rustls::TlsConnector::from(config);
    let name = server_name(&target.tls_name)?;
    let mut stream = connector
        .connect(name, stream)
        .await
        .map_err(|err| format!("tls handshake with {address}: {err}"))?;
    write_and_status(&mut stream, request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn signature_matches_the_hmac_sha256_test_vector() {
        // RFC 4231 test case 2 is longer; this is the widely published vector
        // HMAC-SHA256(key="key", data="The quick brown fox jumps over the lazy dog").
        let body = b"The quick brown fox jumps over the lazy dog";
        assert_eq!(
            sign_body("key", body),
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
        assert_ne!(sign_body("other", body), sign_body("key", body));
    }

    #[test]
    fn url_parser_accepts_http_and_rejects_userinfo_and_other_schemes() {
        let http = parse_webhook_url("http://alerts.internal:8080/kcore?x=1").expect("http");
        assert!(!http.https);
        assert_eq!(http.host, "alerts.internal");
        assert_eq!(http.port, 8080);
        assert_eq!(http.host_header, "alerts.internal:8080");
        assert_eq!(http.path_and_query, "/kcore?x=1");

        let https = parse_webhook_url("https://hooks.example/v1").expect("https");
        assert!(https.https);
        assert_eq!(https.port, 443);
        assert_eq!(https.host_header, "hooks.example");
        assert_eq!(https.path_and_query, "/v1");

        let v6 = parse_webhook_url("http://[::1]:9/h").expect("ipv6");
        assert_eq!(v6.host_header, "[::1]:9");

        assert!(parse_webhook_url("ftp://example/hook").is_err());
        assert!(parse_webhook_url("https://user:pass@example/hook").is_err());
        assert!(parse_webhook_url("not a url").is_err());
    }

    #[test]
    fn retry_policy_stops_on_client_errors() {
        assert!(should_retry(None));
        assert!(should_retry(Some(500)));
        assert!(should_retry(Some(429)));
        assert!(should_retry(Some(408)));
        assert!(!should_retry(Some(200)));
        assert!(!should_retry(Some(400)));
        assert!(!should_retry(Some(404)));
    }

    #[test]
    fn warning_dedup_emits_once_until_the_slot_leaves_the_window() {
        let mut dedup = WarningDedup::default();
        assert!(dedup.claim("cert/aa", "2027-01-01"));
        assert!(!dedup.claim("cert/aa", "2027-01-01"));
        dedup.finish_pass();
        assert!(!dedup.claim("cert/aa", "2027-01-01"));
        dedup.finish_pass();
        // Not claimed this pass: the certificate left the window.
        dedup.finish_pass();
        assert!(dedup.claim("cert/aa", "2027-01-01"));
        assert!(dedup.claim("cert/aa", "2028-01-01"));
    }

    #[test]
    fn endpoint_filter_treats_an_empty_list_as_every_event() {
        let mut endpoint = WebhookEndpoint {
            name: "pager".into(),
            url: "http://127.0.0.1/hook".into(),
            secret: String::new(),
            events: Vec::new(),
            ca_file: String::new(),
        };
        assert!(endpoint_wants(&endpoint, EVENT_VM_STATE_CHANGED));
        endpoint.events = vec![EVENT_NODE_HEARTBEAT_MISSED.to_string()];
        assert!(endpoint_wants(&endpoint, EVENT_NODE_HEARTBEAT_MISSED));
        assert!(!endpoint_wants(&endpoint, EVENT_VM_STATE_CHANGED));
    }

    #[test]
    fn endpoint_debug_redacts_the_secret() {
        let endpoint = WebhookEndpoint {
            name: "pager".into(),
            url: "http://127.0.0.1/hook".into(),
            secret: "super-secret".into(),
            events: Vec::new(),
            ca_file: String::new(),
        };
        let rendered = format!("{endpoint:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("super-secret"));
    }

    #[tokio::test]
    async fn http_delivery_posts_the_body_and_the_signature() {
        let (url, mut hits) = scripted_server(vec![200]).await;
        let endpoint = WebhookEndpoint {
            name: "lab".into(),
            url,
            secret: "key".into(),
            events: Vec::new(),
            ca_file: String::new(),
        };
        let body = b"{\"type\":\"vm.state.changed\"}";
        let headers = vec![(
            "X-Kcore-Signature".to_string(),
            sign_body(&endpoint.secret, body),
        )];
        deliver(&endpoint, body, &headers, Duration::from_secs(2), 0)
            .await
            .expect("deliver");
        let got = hits.recv().await.expect("request");
        let text = String::from_utf8(got).expect("utf8");
        assert!(text.starts_with("POST /hook?x=1 HTTP/1.1\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.contains(&format!(
            "X-Kcore-Signature: {}\r\n",
            sign_body("key", body)
        )));
        assert!(text.ends_with("{\"type\":\"vm.state.changed\"}"));
    }

    #[tokio::test]
    async fn http_delivery_retries_a_500_and_stops_on_a_400() {
        let (url, mut hits) = scripted_server(vec![500, 200]).await;
        let endpoint = WebhookEndpoint {
            name: "lab".into(),
            url: url.clone(),
            secret: String::new(),
            events: Vec::new(),
            ca_file: String::new(),
        };
        deliver(&endpoint, b"{}", &[], Duration::from_secs(2), 2)
            .await
            .expect("retry then success");
        assert!(!hits.recv().await.expect("first").is_empty());
        assert!(hits.recv().await.is_some());

        let (url, mut hits) = scripted_server(vec![400]).await;
        let endpoint = WebhookEndpoint {
            name: "lab".into(),
            url,
            secret: String::new(),
            events: Vec::new(),
            ca_file: String::new(),
        };
        let err = deliver(&endpoint, b"{}", &[], Duration::from_secs(2), 3)
            .await
            .expect_err("400 is terminal");
        assert!(err.contains("400"), "{err}");
        assert!(hits.recv().await.is_some());
        // A second accept would mean we retried. The server task ends after
        // one response, so a retry would surface as a connect error instead
        // of HTTP 400. The error above is the status, so there was one try.
    }

    #[tokio::test]
    async fn noop_dispatcher_drops_events() {
        let dispatcher = Dispatcher::noop();
        dispatcher.emit(EVENT_VM_STATE_CHANGED, "vm/web", serde_json::json!({}));
    }

    #[test]
    fn https_trust_store_includes_public_roots() {
        tls_client_config("").expect("client config");
    }

    /// Accepts `statuses.len()` connections and records each raw request.
    async fn scripted_server(statuses: Vec<u16>) -> (String, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = mpsc::channel(statuses.len().max(1));
        tokio::spawn(async move {
            for status in statuses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let body = read_http_request(&mut sock).await;
                let _ = tx.send(body).await;
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}/hook?x=1"), rx)
    }

    async fn read_http_request(sock: &mut TcpStream) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&buf[..header_end]).unwrap_or("");
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                if buf.len() >= header_end + 4 + length {
                    break;
                }
            }
            if buf.len() > 64 * 1024 {
                break;
            }
        }
        buf
    }
}
