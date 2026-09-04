//! CSR-based rotation of the node's own TLS identity.
//!
//! The ordering is what makes this non-disruptive:
//!
//! 1. generate a fresh keypair **in memory** and a PKCS#10 CSR for the exact
//!    identity the current certificate carries;
//! 2. ask the controller to sign it (`Controller.SignNodeCsr`) over the
//!    existing mTLS channel — the old certificate is still valid, so this call
//!    authenticates normally;
//! 3. validate the signed chain against the keypair we generated, the expected
//!    CN and the clock, all before touching the filesystem;
//! 4. install cert and key with write-temp + fsync + rename, keeping the
//!    previous bytes in memory for rollback;
//! 5. re-read what we just wrote, and roll back if it does not parse;
//! 6. only then ask the listener to reload.
//!
//! Any failure before step 4 leaves the filesystem untouched; a failure in
//! step 5 restores the previous files. Either way the node keeps serving on
//! its existing certificate and the controller retries on the next tick.

use std::path::{Path, PathBuf};

use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, PublicKeyData, SanType};
use time::{Duration, OffsetDateTime};
use tracing::{info, warn};

use super::reload::ReloadHandle;
use super::{facts_from_pem, CertFacts};
use crate::config::{CertRotationConfig, Config};
use crate::controller_proto;

/// What a rotation attempt did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RotationOutcome {
    pub rotated: bool,
    /// The certificate was outside the renewal window and `force` was not set.
    pub skipped: bool,
    /// Serial of the newly installed certificate; empty when nothing changed.
    pub serial_hex: String,
    pub days_until_expiry: i32,
    pub message: String,
}

/// Is this certificate inside its renewal window?
///
/// Same two-part rule as the controller's reconciler
/// (`cert_rotation_reconciler::is_due_for_renewal`), so a controller-driven
/// rotation and a node's own timer agree on when a certificate is due: either
/// fewer than `renew_before_days` remain, or less than `lifetime_fraction` of
/// the total lifetime remains.
pub fn is_due_for_renewal(
    not_before: OffsetDateTime,
    not_after: OffsetDateTime,
    now: OffsetDateTime,
    renew_before_days: i64,
    lifetime_fraction: f64,
) -> bool {
    if now >= not_after {
        return true;
    }
    let remaining = not_after - now;
    if remaining <= Duration::days(renew_before_days) {
        return true;
    }
    let lifetime = not_after - not_before;
    if lifetime <= Duration::ZERO {
        return true;
    }
    remaining.as_seconds_f64() / lifetime.as_seconds_f64() < lifetime_fraction
}

/// Generate a keypair and a CSR requesting `cn` with `sans`.
///
/// The CSR is self-signed by the new key, which is the proof of possession the
/// controller relies on. The requested CN and SANs only have to *match* what
/// the controller will author anyway; it overrides them regardless.
pub fn build_csr(cn: &str, sans: &[String]) -> Result<(String, KeyPair), String> {
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, cn);
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let mut subject_alt_names = Vec::with_capacity(sans.len());
    for san in sans {
        subject_alt_names.push(match san.parse::<std::net::IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(
                san.clone()
                    .try_into()
                    .map_err(|e| format!("invalid DNS SAN '{san}': {e}"))?,
            ),
        });
    }
    params.subject_alt_names = subject_alt_names;

    let key = KeyPair::generate().map_err(|e| format!("generating keypair: {e}"))?;
    let csr = params
        .serialize_request(&key)
        .map_err(|e| format!("serializing CSR: {e}"))?;
    let csr_pem = csr.pem().map_err(|e| format!("encoding CSR PEM: {e}"))?;
    Ok((csr_pem, key))
}

/// Refuse to install a chain that is not usable. Runs before anything is
/// written, so a controller that returns junk cannot break a working node.
pub fn validate_signed_chain(
    chain_pem: &str,
    key: &KeyPair,
    expected_cn: &str,
    now: OffsetDateTime,
) -> Result<CertFacts, String> {
    let facts = facts_from_pem(chain_pem)?;
    if facts.subject_cn != expected_cn {
        return Err(format!(
            "signed certificate CN '{}' does not match expected '{expected_cn}'",
            facts.subject_cn
        ));
    }
    if facts.not_after <= now {
        return Err(format!(
            "signed certificate already expired at {}",
            facts.not_after
        ));
    }
    if facts.spki_der != key.subject_public_key_info() {
        return Err(
            "signed certificate public key does not match the key we generated".to_string(),
        );
    }
    Ok(facts)
}

/// Previous on-disk bytes, retained so a post-write failure can be undone.
#[derive(Debug, Clone)]
pub struct Rollback {
    cert_file: PathBuf,
    key_file: PathBuf,
    prev_cert: Option<Vec<u8>>,
    prev_key: Option<Vec<u8>>,
}

impl Rollback {
    /// Restore the bytes that were on disk before the install.
    pub fn restore(&self) -> Result<(), String> {
        if let Some(bytes) = &self.prev_cert {
            atomic_write(&self.cert_file, bytes, 0o644)?;
        }
        if let Some(bytes) = &self.prev_key {
            atomic_write(&self.key_file, bytes, 0o600)?;
        }
        Ok(())
    }
}

/// Install `chain_pem`/`key_pem` at the configured paths.
///
/// Both files go through write-temp-in-the-same-directory + fsync + rename, so
/// a reader never sees a partial file and a crash mid-install leaves either the
/// old or the new bytes, never a mixture.
pub fn install_chain(
    cert_file: &Path,
    key_file: &Path,
    chain_pem: &str,
    key_pem: &str,
) -> Result<Rollback, String> {
    let rollback = Rollback {
        cert_file: cert_file.to_path_buf(),
        key_file: key_file.to_path_buf(),
        prev_cert: std::fs::read(cert_file).ok(),
        prev_key: std::fs::read(key_file).ok(),
    };

    // Key first: a cert without its key is unusable, but so is the reverse,
    // and writing the key first means the window where they disagree is the
    // one where the *old* cert is still the one being served.
    atomic_write(key_file, key_pem.as_bytes(), 0o600)?;
    if let Err(e) = atomic_write(cert_file, chain_pem.as_bytes(), 0o644) {
        let _ = rollback.restore();
        return Err(e);
    }
    Ok(rollback)
}

/// Re-read the installed pair and confirm it is loadable and self-consistent.
pub fn verify_installed(
    cert_file: &Path,
    key_file: &Path,
    expected_serial: &str,
) -> Result<(), String> {
    let chain = std::fs::read_to_string(cert_file)
        .map_err(|e| format!("re-reading {}: {e}", cert_file.display()))?;
    let key_pem = std::fs::read_to_string(key_file)
        .map_err(|e| format!("re-reading {}: {e}", key_file.display()))?;
    let facts = facts_from_pem(&chain)?;
    if facts.serial_hex != expected_serial {
        return Err(format!(
            "installed certificate serial {} does not match the signed serial {expected_serial}",
            facts.serial_hex
        ));
    }
    let key = KeyPair::from_pem(&key_pem).map_err(|e| format!("re-reading private key: {e}"))?;
    if facts.spki_der != key.subject_public_key_info() {
        return Err("installed certificate and private key do not match".to_string());
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = path.with_extension(format!(
        "{}.kcore-new",
        path.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
    ));

    {
        let mut file =
            std::fs::File::create(&tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        file.write_all(bytes)
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        file.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }

    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("renaming {} -> {}: {e}", tmp.display(), path.display())
    })?;

    // fsync the directory so the rename itself survives a power cut.
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// Run one rotation attempt for this node.
///
/// Returns `Ok(outcome)` when the node is in a good state afterwards —
/// including the "not due, nothing done" case — and `Err` only when the
/// attempt failed. A failed attempt has left the previous certificate in
/// place; the caller logs and retries later.
pub async fn rotate_once(
    cfg: &Config,
    rotation: &CertRotationConfig,
    force: bool,
    reason: &str,
    reload: &ReloadHandle,
) -> Result<RotationOutcome, String> {
    let tls = cfg
        .tls
        .as_ref()
        .ok_or_else(|| "TLS is not configured; nothing to rotate".to_string())?;

    let cert_file = PathBuf::from(&tls.cert_file);
    let key_file = PathBuf::from(&tls.key_file);
    let current_pem = std::fs::read_to_string(&cert_file)
        .map_err(|e| format!("reading {}: {e}", cert_file.display()))?;
    let current = facts_from_pem(&current_pem)?;

    let now = OffsetDateTime::now_utc();
    let days_until_expiry = current.days_remaining(now) as i32;
    if !force
        && !is_due_for_renewal(
            current.not_before,
            current.not_after,
            now,
            rotation.renew_before_days,
            rotation.renew_at_lifetime_fraction,
        )
    {
        return Ok(RotationOutcome {
            rotated: false,
            skipped: true,
            serial_hex: String::new(),
            days_until_expiry,
            message: format!(
                "certificate has {days_until_expiry} days left, outside the renewal window"
            ),
        });
    }

    let (csr_pem, key) = build_csr(&current.subject_cn, &current.sans)?;

    let mut signed: Option<controller_proto::SignNodeCsrResponse> = None;
    let mut last_err = String::new();
    for endpoint in crate::registration::controller_endpoints(cfg) {
        let channel = match crate::registration::connect_channel(cfg, &endpoint).await {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("connecting to {endpoint}: {e}");
                continue;
            }
        };
        let mut client = controller_proto::controller_client::ControllerClient::new(channel);
        match client
            .sign_node_csr(controller_proto::SignNodeCsrRequest {
                node_id: cfg.node_id.clone(),
                csr_pem: csr_pem.clone(),
            })
            .await
        {
            Ok(resp) => {
                signed = Some(resp.into_inner());
                break;
            }
            Err(e) => last_err = format!("SignNodeCsr on {endpoint}: {e}"),
        }
    }
    let signed = signed.ok_or_else(|| {
        if last_err.is_empty() {
            "no controller endpoints configured".to_string()
        } else {
            last_err
        }
    })?;
    if !signed.success {
        return Err(format!("controller rejected the CSR: {}", signed.message));
    }

    // Everything below is validated before a single byte is written.
    let facts = validate_signed_chain(&signed.cert_chain_pem, &key, &current.subject_cn, now)?;

    let rollback = install_chain(
        &cert_file,
        &key_file,
        &signed.cert_chain_pem,
        &key.serialize_pem(),
    )?;
    if let Err(error) = verify_installed(&cert_file, &key_file, &facts.serial_hex) {
        if let Err(restore_error) = rollback.restore() {
            // Both the install check and the restore failed. Say so loudly:
            // this is the one path where an operator has to look.
            return Err(format!(
                "installed certificate failed verification ({error}) and rollback also failed ({restore_error})"
            ));
        }
        warn!(%error, "rotation rolled back; previous certificate restored");
        return Err(format!(
            "installed certificate failed verification: {error}"
        ));
    }

    let generation = reload.request();
    let days = facts.days_remaining(now) as i32;
    info!(
        node_id = %cfg.node_id,
        serial = %facts.serial_hex,
        previous_serial = %current.serial_hex,
        days_until_expiry = days,
        reload_generation = generation,
        reason = %reason,
        "rotated node certificate; private key never left the node"
    );

    Ok(RotationOutcome {
        rotated: true,
        skipped: false,
        serial_hex: facts.serial_hex,
        days_until_expiry: days,
        message: "certificate rotated and listener reload requested".to_string(),
    })
}

/// Background loop that rotates this node's certificate when it comes due.
///
/// The controller drives rotation too (`NodeAdmin.RotateNodeCert`); this loop
/// is the node's own safety net for when it cannot be reached, or reaches a
/// controller that has rotation disabled.
pub fn spawn_rotation_loop(cfg: Config, reload: ReloadHandle) {
    let rotation = cfg.cert_rotation.clone();
    if !rotation.enabled {
        info!("node certificate self-rotation disabled (certRotation.enabled: false)");
        return;
    }
    let interval = std::time::Duration::from_secs(rotation.check_interval_secs.max(60));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            match rotate_once(&cfg, &rotation, false, "node-timer", &reload).await {
                Ok(outcome) if outcome.rotated => {}
                Ok(_) => {}
                Err(error) => warn!(%error, "scheduled certificate rotation failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::test_support::{ensure_crypto_provider, node_leaf, test_ca};

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    #[test]
    fn not_due_well_inside_the_lifetime() {
        let n = now();
        assert!(!is_due_for_renewal(
            n - Duration::days(10),
            n + Duration::days(355),
            n,
            30,
            0.25
        ));
    }

    #[test]
    fn due_inside_the_fixed_day_window() {
        let n = now();
        assert!(is_due_for_renewal(
            n - Duration::days(340),
            n + Duration::days(20),
            n,
            30,
            0.0
        ));
    }

    #[test]
    fn due_inside_the_lifetime_fraction_window() {
        let n = now();
        // 10% of the lifetime remains, fraction threshold is 25%, and the
        // absolute day floor is far away.
        assert!(is_due_for_renewal(
            n - Duration::days(900),
            n + Duration::days(100),
            n,
            30,
            0.25
        ));
    }

    #[test]
    fn expired_certificates_are_always_due() {
        let n = now();
        assert!(is_due_for_renewal(
            n - Duration::days(400),
            n - Duration::days(1),
            n,
            30,
            0.25
        ));
    }

    #[test]
    fn build_csr_requests_the_current_identity() {
        let (csr_pem, key) = build_csr(
            "kcore-node-10.0.0.5",
            &["10.0.0.5".to_string(), "node5.example".to_string()],
        )
        .expect("build csr");
        assert!(csr_pem.contains("BEGIN CERTIFICATE REQUEST"));

        // The controller parses it with the same crate, so round-trip through
        // rcgen is the assertion that matters.
        let parsed = rcgen::CertificateSigningRequestParams::from_pem(&csr_pem).expect("parse csr");
        assert_eq!(parsed.params.subject_alt_names.len(), 2);
        assert_eq!(
            parsed.public_key.subject_public_key_info(),
            key.subject_public_key_info()
        );
    }

    #[test]
    fn build_csr_rejects_an_unusable_dns_san() {
        let err = build_csr("kcore-node-x", &["not a hostname \u{1F600}".to_string()])
            .expect_err("non-ASCII SAN must be rejected");
        assert!(err.contains("invalid DNS SAN"), "{err}");
    }

    /// Sign `csr_pem` the way the controller does, so rotation tests exercise
    /// the real CSR path without a live controller.
    fn sign_csr(
        ca_cert: &str,
        ca_key: &str,
        csr_pem: &str,
        host: &str,
        validity: Duration,
    ) -> String {
        let mut csr = rcgen::CertificateSigningRequestParams::from_pem(csr_pem).expect("parse csr");
        let mut params = CertificateParams::new(vec![host.to_string()]).expect("san");
        params
            .distinguished_name
            .push(DnType::CommonName, format!("kcore-node-{host}"));
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.not_before = OffsetDateTime::now_utc() - Duration::minutes(1);
        params.not_after = OffsetDateTime::now_utc() + validity;
        csr.params = params;

        let ca_key = KeyPair::from_pem(ca_key).expect("ca key");
        let issuer = rcgen::Issuer::from_ca_cert_pem(ca_cert, ca_key).expect("issuer");
        let cert = csr.signed_by(&issuer).expect("sign csr");
        format!("{}{}", cert.pem(), ca_cert)
    }

    #[test]
    fn validate_signed_chain_accepts_a_chain_bound_to_our_key() {
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (csr_pem, key) =
            build_csr("kcore-node-10.0.0.5", &["10.0.0.5".to_string()]).expect("csr");
        let chain = sign_csr(&ca_cert, &ca_key, &csr_pem, "10.0.0.5", Duration::days(7));

        let facts = validate_signed_chain(&chain, &key, "kcore-node-10.0.0.5", now())
            .expect("chain should validate");
        assert_eq!(facts.subject_cn, "kcore-node-10.0.0.5");
    }

    #[test]
    fn validate_signed_chain_rejects_a_chain_for_someone_elses_key() {
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (csr_pem, _key) =
            build_csr("kcore-node-10.0.0.5", &["10.0.0.5".to_string()]).expect("csr");
        let chain = sign_csr(&ca_cert, &ca_key, &csr_pem, "10.0.0.5", Duration::days(7));

        let other = KeyPair::generate().expect("other key");
        let err = validate_signed_chain(&chain, &other, "kcore-node-10.0.0.5", now())
            .expect_err("key mismatch must be rejected");
        assert!(err.contains("does not match the key we generated"), "{err}");
    }

    #[test]
    fn validate_signed_chain_rejects_a_wrong_cn() {
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (csr_pem, key) =
            build_csr("kcore-node-10.0.0.5", &["10.0.0.5".to_string()]).expect("csr");
        // Controller answers with a certificate for a different host.
        let chain = sign_csr(&ca_cert, &ca_key, &csr_pem, "10.0.0.9", Duration::days(7));
        let err = validate_signed_chain(&chain, &key, "kcore-node-10.0.0.5", now())
            .expect_err("CN mismatch must be rejected");
        assert!(err.contains("does not match expected"), "{err}");
    }

    #[test]
    fn validate_signed_chain_rejects_an_already_expired_certificate() {
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (csr_pem, key) =
            build_csr("kcore-node-10.0.0.5", &["10.0.0.5".to_string()]).expect("csr");
        let chain = sign_csr(&ca_cert, &ca_key, &csr_pem, "10.0.0.5", Duration::days(1));
        // Evaluate as if it were two days from now.
        let err = validate_signed_chain(
            &chain,
            &key,
            "kcore-node-10.0.0.5",
            now() + Duration::days(2),
        )
        .expect_err("expired certificate must be rejected");
        assert!(err.contains("already expired"), "{err}");
    }

    #[test]
    fn validate_signed_chain_rejects_garbage() {
        let key = KeyPair::generate().expect("key");
        assert!(validate_signed_chain("nonsense", &key, "kcore-node-1", now()).is_err());
    }

    #[test]
    fn install_chain_writes_atomically_and_restricts_the_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_file = dir.path().join("node.crt");
        let key_file = dir.path().join("node.key");

        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (chain, key_pem) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(30));
        install_chain(&cert_file, &key_file, &chain, &key_pem).expect("install");

        assert_eq!(std::fs::read_to_string(&cert_file).expect("cert"), chain);
        assert_eq!(std::fs::read_to_string(&key_file).expect("key"), key_pem);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_file)
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "private key must not be world-readable"
            );
        }
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("kcore-new"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn rollback_restores_the_previous_certificate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_file = dir.path().join("node.crt");
        let key_file = dir.path().join("node.key");

        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (old_chain, old_key) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(30));
        std::fs::write(&cert_file, &old_chain).expect("seed cert");
        std::fs::write(&key_file, &old_key).expect("seed key");

        let (new_chain, new_key) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(365));
        let rollback = install_chain(&cert_file, &key_file, &new_chain, &new_key).expect("install");
        assert_eq!(
            std::fs::read_to_string(&cert_file).expect("cert"),
            new_chain
        );

        rollback.restore().expect("restore");
        assert_eq!(
            std::fs::read_to_string(&cert_file).expect("cert"),
            old_chain,
            "the old certificate must come back byte for byte"
        );
        assert_eq!(std::fs::read_to_string(&key_file).expect("key"), old_key);
    }

    #[test]
    fn verify_installed_detects_a_mismatched_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_file = dir.path().join("node.crt");
        let key_file = dir.path().join("node.key");

        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (chain, _key) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(30));
        let (_other_chain, other_key) =
            node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(30));
        std::fs::write(&cert_file, &chain).expect("write cert");
        std::fs::write(&key_file, &other_key).expect("write mismatched key");

        let serial = facts_from_pem(&chain).expect("facts").serial_hex;
        let err = verify_installed(&cert_file, &key_file, &serial)
            .expect_err("mismatched pair must be caught");
        assert!(err.contains("do not match"), "{err}");
    }

    #[test]
    fn verify_installed_detects_the_wrong_serial() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_file = dir.path().join("node.crt");
        let key_file = dir.path().join("node.key");
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (chain, key_pem) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(30));
        install_chain(&cert_file, &key_file, &chain, &key_pem).expect("install");

        let err = verify_installed(&cert_file, &key_file, "DEADBEEF")
            .expect_err("serial mismatch must be caught");
        assert!(err.contains("does not match the signed serial"), "{err}");
    }

    #[test]
    fn verify_installed_accepts_what_install_chain_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_file = dir.path().join("node.crt");
        let key_file = dir.path().join("node.key");
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (chain, key_pem) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(30));
        install_chain(&cert_file, &key_file, &chain, &key_pem).expect("install");
        let serial = facts_from_pem(&chain).expect("facts").serial_hex;
        verify_installed(&cert_file, &key_file, &serial).expect("round trip");
    }

    #[tokio::test]
    async fn rotate_once_skips_a_certificate_outside_the_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_file = dir.path().join("node.crt");
        let key_file = dir.path().join("node.key");
        let ca_file = dir.path().join("ca.crt");
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (chain, key_pem) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(365));
        std::fs::write(&cert_file, &chain).expect("cert");
        std::fs::write(&key_file, &key_pem).expect("key");
        std::fs::write(&ca_file, &ca_cert).expect("ca");

        let cfg = Config {
            node_id: "node-1".to_string(),
            listen_addr: "0.0.0.0:9091".to_string(),
            controller_addr: String::new(),
            controllers: vec![],
            dc_id: "DC1".to_string(),
            tls: Some(crate::config::TlsConfig {
                ca_file: ca_file.display().to_string(),
                cert_file: cert_file.display().to_string(),
                key_file: key_file.display().to_string(),
            }),
            vm_socket_dir: "/run/kcore".to_string(),
            nix_config_path: "/etc/nixos/kcore-vms.nix".to_string(),
            storage: crate::config::StorageConfig::default(),
            cert_rotation: CertRotationConfig::default(),
            revocation: crate::config::NodeRevocationConfig::default(),
        };

        let reload = ReloadHandle::new();
        let outcome = rotate_once(&cfg, &cfg.cert_rotation, false, "test", &reload)
            .await
            .expect("skip is not a failure");
        assert!(outcome.skipped && !outcome.rotated);
        assert_eq!(reload.requests(), 0, "a skip must not reload the listener");
        // The certificate on disk is untouched.
        assert_eq!(std::fs::read_to_string(&cert_file).expect("cert"), chain);
    }

    #[tokio::test]
    async fn rotate_once_leaves_the_old_cert_usable_when_the_controller_is_unreachable() {
        ensure_crypto_provider();
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_file = dir.path().join("node.crt");
        let key_file = dir.path().join("node.key");
        let ca_file = dir.path().join("ca.crt");
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        // Inside the renewal window, so rotation is actually attempted.
        let (chain, key_pem) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(2));
        std::fs::write(&cert_file, &chain).expect("cert");
        std::fs::write(&key_file, &key_pem).expect("key");
        std::fs::write(&ca_file, &ca_cert).expect("ca");

        let cfg = Config {
            node_id: "node-1".to_string(),
            listen_addr: "0.0.0.0:9091".to_string(),
            controller_addr: String::new(),
            // Port 1 on loopback: connect fails fast and deterministically.
            controllers: vec!["127.0.0.1:1".to_string()],
            dc_id: "DC1".to_string(),
            tls: Some(crate::config::TlsConfig {
                ca_file: ca_file.display().to_string(),
                cert_file: cert_file.display().to_string(),
                key_file: key_file.display().to_string(),
            }),
            vm_socket_dir: "/run/kcore".to_string(),
            nix_config_path: "/etc/nixos/kcore-vms.nix".to_string(),
            storage: crate::config::StorageConfig::default(),
            cert_rotation: CertRotationConfig::default(),
            revocation: crate::config::NodeRevocationConfig::default(),
        };

        let reload = ReloadHandle::new();
        let err = rotate_once(&cfg, &cfg.cert_rotation, true, "test", &reload)
            .await
            .expect_err("unreachable controller must fail the attempt");
        assert!(!err.is_empty());
        assert_eq!(reload.requests(), 0, "failed rotation must not reload");
        assert_eq!(
            std::fs::read_to_string(&cert_file).expect("cert"),
            chain,
            "the working certificate must survive a failed rotation"
        );
        assert_eq!(std::fs::read_to_string(&key_file).expect("key"), key_pem);
        // And it is still a usable identity.
        verify_installed(
            &cert_file,
            &key_file,
            &facts_from_pem(&chain).expect("facts").serial_hex,
        )
        .expect("old pair still loads");
    }

    #[tokio::test]
    async fn rotate_once_requires_tls_config() {
        let cfg = Config {
            node_id: "node-1".to_string(),
            listen_addr: "0.0.0.0:9091".to_string(),
            controller_addr: String::new(),
            controllers: vec![],
            dc_id: "DC1".to_string(),
            tls: None,
            vm_socket_dir: "/run/kcore".to_string(),
            nix_config_path: "/etc/nixos/kcore-vms.nix".to_string(),
            storage: crate::config::StorageConfig::default(),
            cert_rotation: CertRotationConfig::default(),
            revocation: crate::config::NodeRevocationConfig::default(),
        };
        let reload = ReloadHandle::new();
        let err = rotate_once(&cfg, &cfg.cert_rotation, true, "test", &reload)
            .await
            .expect_err("no TLS, nothing to rotate");
        assert!(err.contains("TLS is not configured"), "{err}");
    }
}

/// Property tests for the renewal threshold, which is the one piece of
/// rotation logic that has to hold for every clock and lifetime.
#[cfg(test)]
mod prop_tests {
    use super::is_due_for_renewal;
    use proptest::prelude::*;
    use time::{Duration, OffsetDateTime};

    proptest! {
        #![proptest_config(ProptestConfig { cases: 1_000, .. ProptestConfig::default() })]

        /// A certificate is always due once `now` is at or past `not_after`,
        /// whatever the thresholds say.
        #[test]
        fn expired_is_always_due(
            lifetime_days in 1i64..3650,
            past_days in 0i64..1000,
            renew_before in 0i64..365,
            fraction in 0.0f64..1.0,
        ) {
            let now = OffsetDateTime::now_utc();
            let not_after = now - Duration::days(past_days);
            let not_before = not_after - Duration::days(lifetime_days);
            prop_assert!(is_due_for_renewal(not_before, not_after, now, renew_before, fraction));
        }

        /// Raising either threshold can only ever make more certificates due,
        /// never fewer. Operators rely on this monotonicity when they widen a
        /// window to force rotation earlier.
        #[test]
        fn widening_a_threshold_is_monotonic(
            lifetime_days in 2i64..3650,
            elapsed_frac in 0.0f64..1.0,
            renew_before in 0i64..180,
            extra_days in 0i64..180,
            fraction in 0.0f64..0.5,
            extra_fraction in 0.0f64..0.5,
        ) {
            let now = OffsetDateTime::now_utc();
            let elapsed = (lifetime_days as f64 * elapsed_frac) as i64;
            let not_before = now - Duration::days(elapsed);
            let not_after = not_before + Duration::days(lifetime_days);

            let narrow = is_due_for_renewal(not_before, not_after, now, renew_before, fraction);
            let wide = is_due_for_renewal(
                not_before,
                not_after,
                now,
                renew_before + extra_days,
                fraction + extra_fraction,
            );
            prop_assert!(!narrow || wide, "widening the window unset a due certificate");
        }

        /// With both thresholds at zero, a certificate is due only when it has
        /// actually expired: no threshold means no proactive rotation.
        #[test]
        fn zero_thresholds_only_fire_on_expiry(
            lifetime_days in 1i64..3650,
            remaining_days in 1i64..3650,
        ) {
            let now = OffsetDateTime::now_utc();
            let not_after = now + Duration::days(remaining_days);
            let not_before = not_after - Duration::days(lifetime_days + remaining_days);
            prop_assert!(!is_due_for_renewal(not_before, not_after, now, 0, 0.0));
        }
    }
}
