use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub node_id: String,
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default)]
    pub controller_addr: String,
    #[serde(default)]
    pub controllers: Vec<String>,
    #[serde(default = "default_dc_id")]
    pub dc_id: String,
    pub tls: Option<TlsConfig>,
    #[serde(default = "default_vm_socket_dir")]
    pub vm_socket_dir: String,
    #[serde(default = "default_nix_config_path")]
    pub nix_config_path: String,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub cert_rotation: CertRotationConfig,
    #[serde(default)]
    pub revocation: NodeRevocationConfig,
}

/// Node-side certificate rotation. The controller drives rotation too
/// (`NodeAdmin.RotateNodeCert`); this is the node's own safety net for when it
/// cannot be reached. Thresholds mirror the controller defaults so the two
/// agree on when a certificate is due.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CertRotationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How often the node checks its own certificate.
    #[serde(default = "default_node_rotation_check_interval_secs")]
    pub check_interval_secs: u64,
    /// Rotate when fewer than this many days remain.
    #[serde(default = "default_renew_before_days")]
    pub renew_before_days: i64,
    /// Rotate when less than this fraction of the total lifetime remains.
    #[serde(default = "default_renew_at_lifetime_fraction")]
    pub renew_at_lifetime_fraction: f64,
}

/// Revocation checking for peers connecting to this node-agent.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRevocationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How often the CRL is fetched from the controller.
    #[serde(default = "default_crl_fetch_interval_secs")]
    pub fetch_interval_secs: u64,
    /// How old the CRL may get before `fail_mode` applies.
    #[serde(default = "default_max_staleness_secs")]
    pub max_staleness_secs: u64,
    /// `soft-fail` (default) keeps serving on the last known CRL and warns;
    /// `hard-fail` rejects every peer until fresh data arrives.
    #[serde(default = "default_fail_mode")]
    pub fail_mode: String,
    /// Consult the controller's OCSP responder for a live answer before
    /// applying `fail_mode` to a stale CRL.
    #[serde(default = "default_true")]
    pub ocsp_enabled: bool,
    /// Base URL of the controller's PKI HTTP endpoints, e.g.
    /// `http://controller.example:9095`. Empty disables OCSP point queries.
    #[serde(default)]
    pub ocsp_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    pub ca_file: String,
    pub cert_file: String,
    pub key_file: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageConfig {
    #[serde(default)]
    pub backend: StorageBackendKind,
    #[serde(default = "default_image_cache_dir")]
    pub image_cache_dir: String,
    #[serde(default = "default_filesystem_volume_dir")]
    pub filesystem_volume_dir: String,
    #[serde(default)]
    pub lvm: Option<LvmConfig>,
    #[serde(default)]
    pub zfs: Option<ZfsConfig>,
    #[serde(default)]
    pub ceph: Option<CephConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum StorageBackendKind {
    #[default]
    Filesystem,
    Lvm,
    Zfs,
    Ceph,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LvmConfig {
    pub vg_name: String,
    #[serde(default = "default_lvm_lv_prefix")]
    pub lv_prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZfsConfig {
    pub pool_name: String,
    #[serde(default = "default_zfs_dataset_prefix")]
    pub dataset_prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CephConfig {
    #[serde(default = "default_ceph_pool")]
    pub pool: String,
}

fn default_listen_addr() -> String {
    "0.0.0.0:9091".to_string()
}

fn default_vm_socket_dir() -> String {
    "/run/kcore".to_string()
}

fn default_nix_config_path() -> String {
    "/etc/nixos/kcore-vms.nix".to_string()
}

fn default_dc_id() -> String {
    "DC1".to_string()
}

fn default_image_cache_dir() -> String {
    "/var/lib/kcore/images".to_string()
}

fn default_filesystem_volume_dir() -> String {
    "/var/lib/kcore/volumes".to_string()
}

fn default_lvm_lv_prefix() -> String {
    "kcore-".to_string()
}

fn default_zfs_dataset_prefix() -> String {
    "kcore-".to_string()
}

fn default_ceph_pool() -> String {
    "kcore-vms".to_string()
}

fn default_true() -> bool {
    true
}

fn default_node_rotation_check_interval_secs() -> u64 {
    3600
}

fn default_renew_before_days() -> i64 {
    30
}

fn default_renew_at_lifetime_fraction() -> f64 {
    0.25
}

fn default_crl_fetch_interval_secs() -> u64 {
    900
}

fn default_max_staleness_secs() -> u64 {
    // Six hours: long enough that a controller restart or a rolling update is
    // invisible, short enough that a revocation is enforced the same shift.
    21_600
}

fn default_fail_mode() -> String {
    "soft-fail".to_string()
}

impl Default for CertRotationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_secs: default_node_rotation_check_interval_secs(),
            renew_before_days: default_renew_before_days(),
            renew_at_lifetime_fraction: default_renew_at_lifetime_fraction(),
        }
    }
}

impl Default for NodeRevocationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fetch_interval_secs: default_crl_fetch_interval_secs(),
            max_staleness_secs: default_max_staleness_secs(),
            fail_mode: default_fail_mode(),
            ocsp_enabled: true,
            ocsp_url: String::new(),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackendKind::Filesystem,
            image_cache_dir: default_image_cache_dir(),
            filesystem_volume_dir: default_filesystem_volume_dir(),
            lvm: None,
            zfs: None,
            ceph: None,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(Path::new(path))
            .with_context(|| format!("reading config {path}"))?;
        let cfg: Config = serde_yaml::from_str(&contents).context("parsing config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.node_id.trim().is_empty() {
            anyhow::bail!("nodeId is required");
        }
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
                if !std::path::Path::new(path).exists() {
                    anyhow::bail!("{label} '{}' does not exist", path);
                }
            }
        }
        match &self.storage.backend {
            StorageBackendKind::Lvm => {
                if self.storage.lvm.is_none() {
                    anyhow::bail!("storage.lvm config is required when backend is 'lvm'");
                }
            }
            StorageBackendKind::Zfs => {
                if self.storage.zfs.is_none() {
                    anyhow::bail!("storage.zfs config is required when backend is 'zfs'");
                }
            }
            StorageBackendKind::Filesystem => {}
            StorageBackendKind::Ceph => {}
        }
        self.validate_pki()?;
        Ok(())
    }

    fn validate_pki(&self) -> anyhow::Result<()> {
        let rot = &self.cert_rotation;
        if rot.enabled {
            if rot.check_interval_secs == 0 {
                anyhow::bail!("certRotation.checkIntervalSecs must be > 0");
            }
            if rot.renew_before_days < 0 {
                anyhow::bail!("certRotation.renewBeforeDays must be >= 0");
            }
            if !(0.0..=1.0).contains(&rot.renew_at_lifetime_fraction) {
                anyhow::bail!(
                    "certRotation.renewAtLifetimeFraction must be between 0.0 and 1.0, got {}",
                    rot.renew_at_lifetime_fraction
                );
            }
        }

        let rev = &self.revocation;
        if rev.enabled {
            if crate::pki::revocation::FailMode::from_config_str(&rev.fail_mode).is_none() {
                anyhow::bail!(
                    "revocation.failMode '{}' is not 'soft-fail' or 'hard-fail'",
                    rev.fail_mode
                );
            }
            if rev.fetch_interval_secs == 0 {
                anyhow::bail!("revocation.fetchIntervalSecs must be > 0");
            }
            if rev.max_staleness_secs < rev.fetch_interval_secs {
                anyhow::bail!(
                    "revocation.maxStalenessSecs ({}) must be >= revocation.fetchIntervalSecs ({}), \
                     otherwise the CRL is stale the moment it is fetched",
                    rev.max_staleness_secs,
                    rev.fetch_interval_secs
                );
            }
            let scheme_ok =
                rev.ocsp_url.starts_with("http://") || rev.ocsp_url.starts_with("https://");
            if !rev.ocsp_url.is_empty() && !scheme_ok {
                anyhow::bail!(
                    "revocation.ocspUrl '{}' must start with http:// or https://",
                    rev.ocsp_url
                );
            }
        }
        Ok(())
    }

    pub fn controller_endpoints(&self) -> Vec<String> {
        if !self.controllers.is_empty() {
            return self
                .controllers
                .iter()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect();
        }
        let single = self.controller_addr.trim();
        if single.is_empty() {
            Vec::new()
        } else {
            vec![single.to_string()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_defaults_to_filesystem_backend() {
        let parsed: Config = serde_yaml::from_str(
            r#"
nodeId: node-1
"#,
        )
        .expect("parse");
        assert!(matches!(
            parsed.storage.backend,
            StorageBackendKind::Filesystem
        ));
        assert_eq!(parsed.storage.image_cache_dir, "/var/lib/kcore/images");
        assert_eq!(
            parsed.storage.filesystem_volume_dir,
            "/var/lib/kcore/volumes"
        );
    }

    #[test]
    fn parses_lvm_storage_config() {
        let parsed: Config = serde_yaml::from_str(
            r#"
nodeId: node-1
storage:
  backend: lvm
  lvm:
    vgName: vg0
"#,
        )
        .expect("parse lvm");
        assert!(matches!(parsed.storage.backend, StorageBackendKind::Lvm));
        let lvm = parsed.storage.lvm.expect("lvm config");
        assert_eq!(lvm.vg_name, "vg0");
        assert_eq!(lvm.lv_prefix, "kcore-");
    }

    #[test]
    fn parses_ceph_storage_config_with_default_and_custom_pool() {
        let defaulted: Config = serde_yaml::from_str(
            r#"
nodeId: node-1
storage:
  backend: ceph
"#,
        )
        .expect("parse ceph");
        assert!(matches!(
            defaulted.storage.backend,
            StorageBackendKind::Ceph
        ));
        defaulted.validate().expect("ceph needs no extra block");
        assert!(defaulted.storage.ceph.is_none());

        let custom: Config = serde_yaml::from_str(
            r#"
nodeId: node-1
storage:
  backend: ceph
  ceph:
    pool: custom-vms
"#,
        )
        .expect("parse custom pool");
        assert_eq!(custom.storage.ceph.as_ref().unwrap().pool, "custom-vms");
        custom.validate().expect("valid");
    }

    #[test]
    fn validate_rejects_lvm_without_config() {
        let cfg: Config = serde_yaml::from_str(
            r#"
nodeId: node-1
storage:
  backend: lvm
"#,
        )
        .expect("parse");
        let err = cfg
            .validate()
            .expect_err("should reject missing lvm config");
        assert!(err.to_string().contains("lvm"));
    }

    #[test]
    fn pki_sections_default_to_safe_values() {
        let parsed: Config = serde_yaml::from_str(
            r#"
nodeId: node-1
"#,
        )
        .expect("parse");
        assert!(parsed.cert_rotation.enabled);
        assert_eq!(parsed.cert_rotation.renew_before_days, 30);
        assert_eq!(parsed.cert_rotation.renew_at_lifetime_fraction, 0.25);
        assert!(parsed.revocation.enabled);
        assert_eq!(
            parsed.revocation.fail_mode, "soft-fail",
            "the default must not brick a cluster on a fetch failure"
        );
        assert_eq!(parsed.revocation.max_staleness_secs, 21_600);
        parsed.validate_pki().expect("defaults are valid");
    }

    #[test]
    fn parses_explicit_pki_sections() {
        let parsed: Config = serde_yaml::from_str(
            r#"
nodeId: node-1
certRotation:
  enabled: true
  checkIntervalSecs: 600
  renewBeforeDays: 7
  renewAtLifetimeFraction: 0.5
revocation:
  enabled: true
  fetchIntervalSecs: 60
  maxStalenessSecs: 300
  failMode: hard-fail
  ocspEnabled: true
  ocspUrl: http://controller.example:9095
"#,
        )
        .expect("parse");
        assert_eq!(parsed.cert_rotation.check_interval_secs, 600);
        assert_eq!(parsed.cert_rotation.renew_before_days, 7);
        assert_eq!(parsed.revocation.fail_mode, "hard-fail");
        assert_eq!(parsed.revocation.ocsp_url, "http://controller.example:9095");
        parsed.validate_pki().expect("valid");
    }

    #[test]
    fn validate_rejects_an_unknown_fail_mode() {
        let mut cfg: Config = serde_yaml::from_str("nodeId: node-1\n").expect("parse");
        cfg.revocation.fail_mode = "explode".to_string();
        let err = cfg.validate_pki().expect_err("bad fail mode");
        assert!(err.to_string().contains("failMode"), "{err}");
    }

    #[test]
    fn validate_rejects_staleness_shorter_than_the_fetch_interval() {
        let mut cfg: Config = serde_yaml::from_str("nodeId: node-1\n").expect("parse");
        cfg.revocation.fetch_interval_secs = 900;
        cfg.revocation.max_staleness_secs = 60;
        let err = cfg.validate_pki().expect_err("impossible freshness target");
        assert!(err.to_string().contains("maxStalenessSecs"), "{err}");
    }

    #[test]
    fn validate_rejects_an_out_of_range_lifetime_fraction() {
        let mut cfg: Config = serde_yaml::from_str("nodeId: node-1\n").expect("parse");
        cfg.cert_rotation.renew_at_lifetime_fraction = 1.5;
        let err = cfg.validate_pki().expect_err("fraction out of range");
        assert!(err.to_string().contains("renewAtLifetimeFraction"), "{err}");
    }

    #[test]
    fn validate_rejects_a_non_http_ocsp_url() {
        let mut cfg: Config = serde_yaml::from_str("nodeId: node-1\n").expect("parse");
        cfg.revocation.ocsp_url = "controller.example:9095".to_string();
        let err = cfg.validate_pki().expect_err("scheme is required");
        assert!(err.to_string().contains("ocspUrl"), "{err}");
    }

    #[test]
    fn disabled_pki_sections_skip_validation() {
        let mut cfg: Config = serde_yaml::from_str("nodeId: node-1\n").expect("parse");
        cfg.revocation.enabled = false;
        cfg.revocation.fail_mode = "explode".to_string();
        cfg.cert_rotation.enabled = false;
        cfg.cert_rotation.renew_at_lifetime_fraction = 9.0;
        cfg.validate_pki()
            .expect("disabled sections are not checked");
    }

    #[test]
    fn defaults_dc_id_to_dc1() {
        let parsed: Config = serde_yaml::from_str(
            r#"
nodeId: node-1
"#,
        )
        .expect("parse");
        assert_eq!(parsed.dc_id, "DC1");
    }
}
