use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_db_path")]
    pub db_path: String,
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    pub default_network: NetworkConfig,
    /// When set, mutating RPCs append JSON envelopes to `replication_outbox` for future peer sync.
    #[serde(default)]
    pub replication: Option<ReplicationConfig>,
    /// When true, nodes must be manually approved via `kctl node approve`.
    /// Default false: nodes with valid mTLS certificates are auto-approved on registration.
    #[serde(default)]
    pub require_manual_approval: bool,
    /// Proactive node certificate rotation (see `docs/mtls-bootstrap-and-auth.md` §4).
    #[serde(default)]
    pub cert_rotation: CertRotationConfig,
    /// Peer certificate revocation checking.
    #[serde(default)]
    pub revocation: RevocationConfig,
    /// CRL/OCSP distribution endpoints.
    #[serde(default)]
    pub pki: PkiConfig,
    /// Per-identity token bucket on the controller gRPC services.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    /// Optional installed release SBOMs served by `ExportSbom`.
    #[serde(default)]
    pub sbom: SbomConfig,
    /// Outbound webhooks for cluster events. No endpoints means nothing is sent.
    #[serde(default)]
    pub webhooks: WebhooksConfig,
    /// Placement ceiling and the ratios applied to live node load.
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    /// Move Ceph VMs off a node that has missed its heartbeat deadline.
    #[serde(default)]
    pub failover: FailoverConfig,
}

/// Automatic placement of shared-disk VMs when a node becomes unreachable.
///
/// Local-disk VMs stay on the failed node. Set `enabled` to false to leave
/// every VM where it is until an operator drains the node.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailoverConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Ratios applied to physical CPU and memory when deciding whether a node
/// can take another VM. `1.0` schedules against physical capacity. Heartbeat
/// load is what counts as used, so a higher ratio admits VMs onto a quiet
/// node whose live load still has room.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerConfig {
    #[serde(default = "default_overcommit")]
    pub cpu_overcommit: f64,
    #[serde(default = "default_overcommit")]
    pub memory_overcommit: f64,
}

fn default_overcommit() -> f64 {
    1.0
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            cpu_overcommit: 1.0,
            memory_overcommit: 1.0,
        }
    }
}

/// Controller-driven certificate rotation.
///
/// A certificate is due for renewal when **either** it has fewer than
/// `renew_before_days` left **or** less than `renew_at_lifetime_fraction` of
/// its total lifetime remains. The fraction rule is what makes short-lived
/// certificates work: a 7-day certificate would otherwise be "always due"
/// under a 30-day window.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CertRotationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_rotation_check_interval_secs")]
    pub check_interval_secs: u64,
    #[serde(default = "default_renew_before_days")]
    pub renew_before_days: i64,
    #[serde(default = "default_renew_at_lifetime_fraction")]
    pub renew_at_lifetime_fraction: f64,
    #[serde(default = "default_warn_before_days")]
    pub warn_before_days: i64,
    /// Lifetime of certificates the controller signs.
    #[serde(default = "default_cert_validity_days")]
    pub cert_validity_days: i64,
}

impl Default for CertRotationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_secs: default_rotation_check_interval_secs(),
            renew_before_days: default_renew_before_days(),
            renew_at_lifetime_fraction: default_renew_at_lifetime_fraction(),
            warn_before_days: default_warn_before_days(),
            cert_validity_days: default_cert_validity_days(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevocationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `soft-fail` (default) keeps serving when revocation data is stale;
    /// `hard-fail` rejects every peer until it is fresh again.
    #[serde(default = "default_fail_mode")]
    pub fail_mode: String,
    #[serde(default = "default_max_staleness_secs")]
    pub max_staleness_secs: u64,
    #[serde(default = "default_revocation_refresh_secs")]
    pub refresh_interval_secs: u64,
}

impl Default for RevocationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fail_mode: default_fail_mode(),
            max_staleness_secs: default_max_staleness_secs(),
            refresh_interval_secs: default_revocation_refresh_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkiConfig {
    /// When true, serve `/pki/crl.{der,pem}` and `/pki/ocsp` over plain HTTP.
    #[serde(default = "default_true")]
    pub http_enabled: bool,
    #[serde(default = "default_pki_listen_addr")]
    pub http_listen_addr: String,
    /// Base URL advertised to operators and nodes, e.g.
    /// `http://192.168.40.105:9092`. Defaults to the listen address.
    #[serde(default)]
    pub public_base_url: String,
    #[serde(default = "default_crl_validity_hours")]
    pub crl_validity_hours: i64,
    /// Regenerate the CRL once `nextUpdate` is this close.
    #[serde(default = "default_crl_refresh_before_hours")]
    pub crl_refresh_before_hours: i64,
    #[serde(default = "default_ocsp_validity_hours")]
    pub ocsp_validity_hours: i64,
}

impl Default for PkiConfig {
    fn default() -> Self {
        Self {
            http_enabled: true,
            http_listen_addr: default_pki_listen_addr(),
            public_base_url: String::new(),
            crl_validity_hours: default_crl_validity_hours(),
            crl_refresh_before_hours: default_crl_refresh_before_hours(),
            ocsp_validity_hours: default_ocsp_validity_hours(),
        }
    }
}

impl PkiConfig {
    /// URL clients should use for `/pki/...`, without a trailing slash.
    pub fn base_url(&self) -> String {
        if !self.public_base_url.trim().is_empty() {
            return self.public_base_url.trim_end_matches('/').to_string();
        }
        if !self.http_enabled {
            return String::new();
        }
        // A wildcard bind is not a usable URL; leave it to the operator.
        let addr = self.http_listen_addr.trim();
        if addr.starts_with("0.0.0.0:") || addr.starts_with("[::]:") {
            return String::new();
        }
        format!("http://{addr}")
    }
}

fn default_true() -> bool {
    true
}

fn default_rotation_check_interval_secs() -> u64 {
    3600
}

fn default_renew_before_days() -> i64 {
    30
}

fn default_renew_at_lifetime_fraction() -> f64 {
    0.25
}

fn default_warn_before_days() -> i64 {
    45
}

fn default_cert_validity_days() -> i64 {
    365
}

fn default_fail_mode() -> String {
    "soft-fail".to_string()
}

fn default_max_staleness_secs() -> u64 {
    3600
}

fn default_revocation_refresh_secs() -> u64 {
    60
}

fn default_pki_listen_addr() -> String {
    "0.0.0.0:9092".to_string()
}

fn default_crl_validity_hours() -> i64 {
    24
}

fn default_crl_refresh_before_hours() -> i64 {
    6
}

fn default_ocsp_validity_hours() -> i64 {
    1
}

fn default_rate_limit_rps() -> u32 {
    100
}

fn default_rate_limit_burst() -> u32 {
    200
}

/// Inbound gRPC rate limit. Defaults are high enough for heartbeats and
/// reconciliation, and low enough that one peer cannot pin a core.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_rate_limit_rps")]
    pub requests_per_second: u32,
    #[serde(default = "default_rate_limit_burst")]
    pub burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            requests_per_second: default_rate_limit_rps(),
            burst: default_rate_limit_burst(),
        }
    }
}

fn default_webhook_timeout_secs() -> u64 {
    5
}

fn default_webhook_max_retries() -> u32 {
    3
}

/// Outbound event delivery. `enabled: false` is the kill switch; an empty
/// endpoint list delivers nothing even when delivery is enabled.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhooksConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-attempt deadline, including DNS, connect, TLS, and the response status.
    #[serde(default = "default_webhook_timeout_secs")]
    pub timeout_secs: u64,
    /// Extra attempts after the first. 5xx, 408, 429, and network errors are retried.
    #[serde(default = "default_webhook_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub endpoints: Vec<WebhookEndpoint>,
}

impl Default for WebhooksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: default_webhook_timeout_secs(),
            max_retries: default_webhook_max_retries(),
            endpoints: Vec::new(),
        }
    }
}

/// One HTTPS (or HTTP) receiver. `events` empty subscribes to every known event.
/// `secret` is never logged; it is the HMAC-SHA256 key for `X-Kcore-Signature`.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookEndpoint {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub events: Vec<String>,
    /// PEM bundle merged into the HTTPS trust store for this endpoint.
    /// Public webpki roots are always included. Empty uses public roots only.
    #[serde(default)]
    pub ca_file: String,
}

impl std::fmt::Debug for WebhookEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookEndpoint")
            .field("name", &self.name)
            .field("url", &self.url)
            .field(
                "secret",
                &if self.secret.is_empty() {
                    "none"
                } else {
                    "redacted"
                },
            )
            .field("events", &self.events)
            .field("ca_file", &self.ca_file)
            .finish()
    }
}

/// Paths of release SBOM documents installed next to the controller.
/// Empty `crates_file` serves the Cargo.lock graph embedded at build time.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SbomConfig {
    #[serde(default)]
    pub crates_file: String,
    #[serde(default)]
    pub iso_closure_file: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationConfig {
    #[serde(default)]
    pub controller_id: String,
    #[serde(default = "default_dc_id")]
    pub dc_id: String,
    #[serde(default)]
    pub peers: Vec<String>,
}

fn default_dc_id() -> String {
    "DC1".to_string()
}

/// Optional auth overrides (RBAC / bootstrap).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    /// When true, legacy `CN=kctl` keeps cluster-admin even after operators exist.
    #[serde(default)]
    pub bootstrap_kctl: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    pub ca_file: String,
    pub cert_file: String,
    pub key_file: String,
    #[serde(default)]
    pub sub_ca_cert_file: Option<String>,
    #[serde(default)]
    pub sub_ca_key_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfig {
    pub gateway_interface: String,
    pub external_ip: String,
    pub gateway_ip: String,
    #[serde(default = "default_netmask")]
    pub internal_netmask: String,
}

fn default_listen_addr() -> String {
    "0.0.0.0:9090".to_string()
}

fn default_db_path() -> String {
    "/var/lib/kcore/controller.db".to_string()
}

fn default_netmask() -> String {
    "255.255.255.0".to_string()
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        crate::path_safety::assert_safe_path(path, "config file path")?;
        let contents = std::fs::read_to_string(Path::new(path))
            .with_context(|| format!("reading config {path}"))?;
        let cfg: Config = serde_yaml::from_str(&contents).context("parsing config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        crate::path_safety::assert_safe_path(&self.db_path, "dbPath")?;
        if self.listen_addr.parse::<std::net::SocketAddr>().is_err() {
            anyhow::bail!(
                "listen_addr '{}' is not a valid socket address",
                self.listen_addr
            );
        }
        if let Some(tls) = &self.tls {
            for (label, path) in [
                ("tls.ca_file", &tls.ca_file),
                ("tls.cert_file", &tls.cert_file),
                ("tls.key_file", &tls.key_file),
            ] {
                crate::path_safety::assert_safe_path(path, label)?;
                if !std::path::Path::new(path).exists() {
                    anyhow::bail!("{label} '{}' does not exist", path);
                }
            }
            if let Some(p) = &tls.sub_ca_cert_file {
                crate::path_safety::assert_safe_path(p, "tls.sub_ca_cert_file")?;
            }
            if let Some(p) = &tls.sub_ca_key_file {
                crate::path_safety::assert_safe_path(p, "tls.sub_ca_key_file")?;
            }
        }
        if self.default_network.gateway_interface.trim().is_empty() {
            anyhow::bail!("defaultNetwork.gatewayInterface is required");
        }
        if self.default_network.external_ip.trim().is_empty() {
            anyhow::bail!("defaultNetwork.externalIp is required");
        }
        if self.default_network.gateway_ip.trim().is_empty() {
            anyhow::bail!("defaultNetwork.gatewayIp is required");
        }
        if let Some(replication) = &self.replication {
            if replication.dc_id.trim().is_empty() {
                anyhow::bail!("replication.dcId must not be empty");
            }
            if replication.controller_id.trim().is_empty() {
                anyhow::bail!(
                    "replication.controllerId is required when replication section is present"
                );
            }
            if replication.peers.iter().any(|p| p.trim().is_empty()) {
                anyhow::bail!("replication.peers must not contain empty endpoints");
            }
        }
        self.validate_pki()?;
        Ok(())
    }

    fn validate_pki(&self) -> Result<()> {
        let rot = &self.cert_rotation;
        if rot.check_interval_secs == 0 {
            anyhow::bail!("certRotation.checkIntervalSecs must be greater than 0");
        }
        if rot.cert_validity_days <= 0 {
            anyhow::bail!("certRotation.certValidityDays must be greater than 0");
        }
        if rot.renew_before_days < 0 {
            anyhow::bail!("certRotation.renewBeforeDays must not be negative");
        }
        if !(0.0..1.0).contains(&rot.renew_at_lifetime_fraction) {
            anyhow::bail!(
                "certRotation.renewAtLifetimeFraction must be in [0.0, 1.0) (got {})",
                rot.renew_at_lifetime_fraction
            );
        }
        if rot.warn_before_days < rot.renew_before_days {
            anyhow::bail!(
                "certRotation.warnBeforeDays ({}) must be >= renewBeforeDays ({}) so operators are warned before renewal is attempted",
                rot.warn_before_days,
                rot.renew_before_days
            );
        }

        if crate::pki::revocation::FailMode::from_config_str(&self.revocation.fail_mode).is_none() {
            anyhow::bail!(
                "revocation.failMode '{}' is not recognised (expected 'soft-fail' or 'hard-fail')",
                self.revocation.fail_mode
            );
        }
        if self.revocation.refresh_interval_secs == 0 {
            anyhow::bail!("revocation.refreshIntervalSecs must be greater than 0");
        }

        if self.pki.http_enabled
            && self
                .pki
                .http_listen_addr
                .parse::<std::net::SocketAddr>()
                .is_err()
        {
            anyhow::bail!(
                "pki.httpListenAddr '{}' is not a valid socket address",
                self.pki.http_listen_addr
            );
        }
        if self.pki.crl_validity_hours <= 0 {
            anyhow::bail!("pki.crlValidityHours must be greater than 0");
        }
        if self.pki.ocsp_validity_hours <= 0 {
            anyhow::bail!("pki.ocspValidityHours must be greater than 0");
        }
        if self.pki.crl_refresh_before_hours < 0 {
            anyhow::bail!("pki.crlRefreshBeforeHours must not be negative");
        }
        self.validate_rate_limit()?;
        self.validate_sbom_paths()?;
        self.validate_webhooks()?;
        self.validate_scheduler()?;
        Ok(())
    }

    fn validate_scheduler(&self) -> Result<()> {
        for (name, ratio) in [
            ("cpuOvercommit", self.scheduler.cpu_overcommit),
            ("memoryOvercommit", self.scheduler.memory_overcommit),
        ] {
            if !ratio.is_finite() || !(1.0..=16.0).contains(&ratio) {
                anyhow::bail!("scheduler.{name} must be between 1.0 and 16.0 (got {ratio})");
            }
        }
        Ok(())
    }

    fn validate_webhooks(&self) -> Result<()> {
        let hooks = &self.webhooks;
        if hooks.timeout_secs == 0 || hooks.timeout_secs > 30 {
            anyhow::bail!(
                "webhooks.timeoutSecs must be from 1 to 30 (got {})",
                hooks.timeout_secs
            );
        }
        if hooks.max_retries > 10 {
            anyhow::bail!(
                "webhooks.maxRetries must be from 0 to 10 (got {})",
                hooks.max_retries
            );
        }
        let mut names = std::collections::HashSet::new();
        for ep in &hooks.endpoints {
            let name = ep.name.trim();
            if name.is_empty()
                || name.len() > 64
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                anyhow::bail!(
                    "webhooks endpoint name '{name}' must be 1-64 ASCII letters, digits, '_' or '-'"
                );
            }
            if !names.insert(name.to_string()) {
                anyhow::bail!("webhooks endpoint name '{name}' is duplicated");
            }
            crate::webhooks::parse_webhook_url(&ep.url).map_err(|err| {
                anyhow::anyhow!("webhooks endpoint '{name}' has an invalid url: {err}")
            })?;
            for event in &ep.events {
                if !crate::webhooks::is_known_event(event) {
                    anyhow::bail!(
                        "webhooks endpoint '{name}' subscribes to unknown event '{event}' (known: {})",
                        crate::webhooks::KNOWN_EVENTS.join(", ")
                    );
                }
            }
            if !ep.ca_file.trim().is_empty() {
                crate::path_safety::assert_safe_path(&ep.ca_file, "webhooks.caFile")?;
                if !std::path::Path::new(&ep.ca_file).is_file() {
                    anyhow::bail!(
                        "webhooks endpoint '{name}' caFile '{}' does not exist",
                        ep.ca_file
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_rate_limit(&self) -> Result<()> {
        if !self.rate_limit.enabled {
            return Ok(());
        }
        if self.rate_limit.requests_per_second == 0 {
            anyhow::bail!("rateLimit.requestsPerSecond must be greater than 0");
        }
        if self.rate_limit.burst == 0 {
            anyhow::bail!("rateLimit.burst must be greater than 0");
        }
        Ok(())
    }

    fn validate_sbom_paths(&self) -> Result<()> {
        for (label, path) in [
            ("sbom.cratesFile", self.sbom.crates_file.trim()),
            ("sbom.isoClosureFile", self.sbom.iso_closure_file.trim()),
        ] {
            if path.is_empty() {
                continue;
            }
            crate::path_safety::assert_safe_path(path, label)?;
            if !std::path::Path::new(path).is_file() {
                anyhow::bail!("{label} '{path}' does not exist");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_path(name: &str) -> std::path::PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("kcore-controller-{name}-{ts}.yaml"))
    }

    #[test]
    fn load_applies_defaults_for_optional_fields() {
        let path = temp_config_path("defaults");
        std::fs::write(
            &path,
            r#"
defaultNetwork:
  gatewayInterface: eno1
  externalIp: 203.0.113.10
  gatewayIp: 10.0.0.1
"#,
        )
        .expect("write config");

        let cfg = Config::load(path.to_str().expect("path str")).expect("load config");
        assert_eq!(cfg.listen_addr, "0.0.0.0:9090");
        assert_eq!(cfg.db_path, "/var/lib/kcore/controller.db");
        assert_eq!(cfg.default_network.internal_netmask, "255.255.255.0");
        assert!(!cfg.require_manual_approval);
        assert!(cfg.replication.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_parses_replication_section() {
        let path = temp_config_path("repl");
        std::fs::write(
            &path,
            r#"
defaultNetwork:
  gatewayInterface: eno1
  externalIp: 203.0.113.10
  gatewayIp: 10.0.0.1
replication:
  controllerId: ctrl-a
  dcId: DC2
  peers:
    - 10.0.0.11:9090
"#,
        )
        .expect("write config");

        let cfg = Config::load(path.to_str().expect("path str")).expect("load config");
        let rep = cfg.replication.expect("replication");
        assert_eq!(rep.controller_id, "ctrl-a");
        assert_eq!(rep.dc_id, "DC2");
        assert_eq!(rep.peers, vec!["10.0.0.11:9090"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_rejects_replication_peers_without_controller_id() {
        let path = temp_config_path("repl-invalid");
        std::fs::write(
            &path,
            r#"
defaultNetwork:
  gatewayInterface: eno1
  externalIp: 203.0.113.10
  gatewayIp: 10.0.0.1
replication:
  dcId: DC1
  peers:
    - 10.0.0.11:9090
"#,
        )
        .expect("write config");
        let err = Config::load(path.to_str().expect("path str")).expect_err("must fail");
        assert!(err
            .to_string()
            .contains("replication.controllerId is required"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_rejects_empty_peers_replication_without_controller_id() {
        let path = temp_config_path("repl-empty-peers");
        std::fs::write(
            &path,
            r#"
defaultNetwork:
  gatewayInterface: eno1
  externalIp: 203.0.113.10
  gatewayIp: 10.0.0.1
replication:
  dcId: DC1
  peers: []
"#,
        )
        .expect("write config");
        let err = Config::load(path.to_str().expect("path str")).expect_err("must fail");
        assert!(err
            .to_string()
            .contains("replication.controllerId is required"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_returns_error_for_invalid_yaml() {
        let path = temp_config_path("invalid");
        std::fs::write(&path, "defaultNetwork: [").expect("write invalid config");
        let err = Config::load(path.to_str().expect("path str")).expect_err("invalid yaml");
        assert!(err.to_string().contains("parsing config"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_rejects_parent_dir_in_config_file_argument() {
        let err = Config::load("../nonexistent-kcore-config.yaml").expect_err("traversal");
        let s = format!("{err:#}");
        assert!(
            s.contains("config file path") && s.contains(".."),
            "unexpected error: {s}"
        );
    }

    const MINIMAL_NETWORK: &str = r#"
defaultNetwork:
  gatewayInterface: eno1
  externalIp: 203.0.113.10
  gatewayIp: 10.0.0.1
"#;

    fn load_with(name: &str, extra: &str) -> Result<Config> {
        let path = temp_config_path(name);
        std::fs::write(&path, format!("{MINIMAL_NETWORK}{extra}")).expect("write config");
        let result = Config::load(path.to_str().expect("path str"));
        let _ = std::fs::remove_file(path);
        result
    }

    #[test]
    fn pki_sections_default_to_safe_values() {
        let cfg = load_with("pki-defaults", "").expect("load");
        assert!(cfg.cert_rotation.enabled);
        assert_eq!(cfg.cert_rotation.check_interval_secs, 3600);
        assert_eq!(cfg.cert_rotation.renew_before_days, 30);
        assert_eq!(cfg.cert_rotation.warn_before_days, 45);
        assert_eq!(cfg.cert_rotation.cert_validity_days, 365);
        assert!((cfg.cert_rotation.renew_at_lifetime_fraction - 0.25).abs() < f64::EPSILON);
        assert!(cfg.revocation.enabled);
        // The default must not brick a cluster on a transient fetch failure.
        assert_eq!(cfg.revocation.fail_mode, "soft-fail");
        assert_eq!(cfg.revocation.max_staleness_secs, 3600);
        assert!(cfg.pki.http_enabled);
        assert_eq!(cfg.pki.http_listen_addr, "0.0.0.0:9092");
        assert_eq!(cfg.pki.crl_validity_hours, 24);
        assert_eq!(cfg.pki.ocsp_validity_hours, 1);
        assert!(cfg.rate_limit.enabled);
        assert_eq!(cfg.rate_limit.requests_per_second, 100);
        assert_eq!(cfg.rate_limit.burst, 200);
        assert!(cfg.sbom.crates_file.is_empty());
    }

    #[test]
    fn pki_sections_parse_operator_overrides() {
        let cfg = load_with(
            "pki-override",
            r#"
certRotation:
  enabled: false
  checkIntervalSecs: 60
  renewBeforeDays: 2
  renewAtLifetimeFraction: 0.5
  warnBeforeDays: 5
  certValidityDays: 7
revocation:
  failMode: hard-fail
  maxStalenessSecs: 120
  refreshIntervalSecs: 5
pki:
  httpListenAddr: 127.0.0.1:19092
  publicBaseUrl: https://pki.example.test/
  crlValidityHours: 2
  crlRefreshBeforeHours: 1
  ocspValidityHours: 3
"#,
        )
        .expect("load");
        assert!(!cfg.cert_rotation.enabled);
        assert_eq!(cfg.cert_rotation.renew_before_days, 2);
        assert_eq!(cfg.cert_rotation.cert_validity_days, 7);
        assert_eq!(cfg.revocation.fail_mode, "hard-fail");
        assert_eq!(cfg.pki.crl_validity_hours, 2);
        assert_eq!(cfg.pki.base_url(), "https://pki.example.test");
    }

    #[test]
    fn pki_base_url_falls_back_to_the_listen_address() {
        let cfg =
            load_with("pki-base-url", "pki:\n  httpListenAddr: 10.0.0.5:9092\n").expect("load");
        assert_eq!(cfg.pki.base_url(), "http://10.0.0.5:9092");

        // A wildcard bind is not a routable URL, so nothing is advertised.
        let wildcard = load_with("pki-wildcard", "").expect("load");
        assert_eq!(wildcard.pki.base_url(), "");

        let disabled = load_with("pki-disabled", "pki:\n  httpEnabled: false\n").expect("load");
        assert_eq!(disabled.pki.base_url(), "");
    }

    #[test]
    fn load_rejects_unknown_revocation_fail_mode() {
        let err =
            load_with("bad-fail-mode", "revocation:\n  failMode: maybe\n").expect_err("must fail");
        assert!(err.to_string().contains("revocation.failMode"), "{err}");
    }

    #[test]
    fn load_rejects_warn_window_narrower_than_renew_window() {
        let err = load_with(
            "bad-warn",
            "certRotation:\n  renewBeforeDays: 30\n  warnBeforeDays: 10\n",
        )
        .expect_err("must fail");
        assert!(err.to_string().contains("warnBeforeDays"), "{err}");
    }

    #[test]
    fn load_rejects_out_of_range_lifetime_fraction() {
        for value in ["1.0", "1.5", "-0.1"] {
            let err = load_with(
                "bad-fraction",
                &format!("certRotation:\n  renewAtLifetimeFraction: {value}\n"),
            )
            .expect_err("must fail");
            assert!(
                err.to_string().contains("renewAtLifetimeFraction"),
                "value {value}: {err}"
            );
        }
    }

    #[test]
    fn load_rejects_invalid_pki_listen_address() {
        let err = load_with("bad-pki-addr", "pki:\n  httpListenAddr: not-an-address\n")
            .expect_err("must fail");
        assert!(err.to_string().contains("pki.httpListenAddr"), "{err}");
    }

    #[test]
    fn load_rejects_zero_intervals_and_validities() {
        for (name, snippet, needle) in [
            (
                "zero-check",
                "certRotation:\n  checkIntervalSecs: 0\n",
                "checkIntervalSecs",
            ),
            (
                "zero-validity",
                "certRotation:\n  certValidityDays: 0\n",
                "certValidityDays",
            ),
            (
                "zero-refresh",
                "revocation:\n  refreshIntervalSecs: 0\n",
                "refreshIntervalSecs",
            ),
            (
                "zero-crl",
                "pki:\n  crlValidityHours: 0\n",
                "crlValidityHours",
            ),
            (
                "zero-ocsp",
                "pki:\n  ocspValidityHours: 0\n",
                "ocspValidityHours",
            ),
        ] {
            let err = load_with(name, snippet).expect_err("must fail");
            assert!(err.to_string().contains(needle), "{name}: {err}");
        }
    }

    #[test]
    fn load_rejects_parent_dir_in_db_path_field() {
        let path = temp_config_path("bad-db");
        std::fs::write(
            &path,
            r#"
dbPath: ../../../tmp/evil.db
defaultNetwork:
  gatewayInterface: eno1
  externalIp: 203.0.113.10
  gatewayIp: 10.0.0.1
"#,
        )
        .expect("write config");
        let err = Config::load(path.to_str().expect("path str")).expect_err("bad db path");
        let s = format!("{err:#}");
        assert!(
            s.contains("dbPath") && s.contains(".."),
            "unexpected error: {s}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn webhooks_default_to_enabled_with_no_endpoints() {
        let cfg = load_with("webhooks-default", "").expect("load");
        assert!(cfg.webhooks.enabled);
        assert!(cfg.webhooks.endpoints.is_empty());
        assert_eq!(cfg.webhooks.timeout_secs, 5);
        assert_eq!(cfg.webhooks.max_retries, 3);
    }

    #[test]
    fn webhooks_section_parses_endpoints_and_rejects_bad_values() {
        let cfg = load_with(
            "webhooks-ok",
            r#"
webhooks:
  timeoutSecs: 2
  maxRetries: 1
  endpoints:
    - name: pager
      url: https://hooks.example/kcore
      secret: top-secret
      events:
        - node.heartbeat.missed
        - vm.state.changed
"#,
        )
        .expect("load");
        assert_eq!(cfg.webhooks.endpoints.len(), 1);
        assert_eq!(cfg.webhooks.endpoints[0].name, "pager");
        assert_eq!(cfg.webhooks.endpoints[0].events.len(), 2);
        let rendered = format!("{:?}", cfg.webhooks.endpoints[0]);
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(!rendered.contains("top-secret"), "{rendered}");

        for (name, snippet, needle) in [
            (
                "webhooks-scheme",
                r#"
webhooks:
  endpoints:
    - name: pager
      url: ftp://example/hook
"#,
                "scheme",
            ),
            (
                "webhooks-event",
                r#"
webhooks:
  endpoints:
    - name: pager
      url: http://127.0.0.1/hook
      events:
        - vm.deleted
"#,
                "unknown event",
            ),
            (
                "webhooks-dup",
                r#"
webhooks:
  endpoints:
    - name: pager
      url: http://127.0.0.1/hook
    - name: pager
      url: http://127.0.0.1/other
"#,
                "duplicated",
            ),
            (
                "webhooks-timeout",
                r#"
webhooks:
  timeoutSecs: 0
"#,
                "timeoutSecs",
            ),
        ] {
            let err = load_with(name, snippet).expect_err("must fail");
            assert!(err.to_string().contains(needle), "{name}: {err}");
        }
    }
}
