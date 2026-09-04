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
}
