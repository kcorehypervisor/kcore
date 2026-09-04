//! Revocation enforcement for peers connecting to the node-agent.
//!
//! `tonic` 0.12 builds its `rustls` `ServerConfig` internally and takes no
//! custom `ClientCertVerifier`, so rustls' own CRL support
//! (`WebPkiClientVerifier::with_crls`) cannot be reached through
//! `ServerTlsConfig`. Enforcement happens one layer up instead: a `tonic`
//! interceptor checks the serial of the presented client certificate against
//! the revocation set before any handler runs. The node-agent's only peers are
//! the controller and `kctl`, both of which present client certificates, so
//! this covers the whole inbound surface.
//!
//! The revocation set is a CRL fetched from the controller over the existing
//! mTLS gRPC channel (`Controller.GetCrl`) and verified against the issuing
//! CA before it is trusted. Nodes therefore need no extra network path, no
//! extra credential, and no plain-HTTP dependency; the controller's
//! `/pki/crl.der` endpoint exists for external tooling.
//!
//! When the CRL cannot be refreshed, `fail_mode` decides:
//! `soft-fail` (default) keeps serving on the last known set and warns, so a
//! controller outage cannot lock every node out of its own cluster;
//! `hard-fail` rejects every peer until fresh data arrives.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use time::{Duration, OffsetDateTime};
use tracing::{info, warn};

use crate::config::{Config, NodeRevocationConfig};
use crate::controller_proto;

/// What to do when revocation data cannot be refreshed within
/// `max_staleness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailMode {
    /// Keep accepting peers absent from the last known revocation set, and
    /// warn. The default.
    #[default]
    SoftFail,
    /// Reject every peer until revocation data is fresh again.
    HardFail,
}

impl FailMode {
    pub fn from_config_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "soft-fail" | "soft" => Some(FailMode::SoftFail),
            "hard-fail" | "hard" => Some(FailMode::HardFail),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FailMode::SoftFail => "soft-fail",
            FailMode::HardFail => "hard-fail",
        }
    }
}

/// Outcome of a revocation check, kept separate from `tonic::Status` so the
/// decision logic is testable without a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Revoked { serial_hex: String },
    Stale { age_secs: i64 },
    AllowStale { age_secs: i64 },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow | Decision::AllowStale { .. })
    }
}

/// Pure decision function. `refreshed_at` is `None` when no CRL has ever been
/// loaded, which counts as maximally stale.
pub fn decide(
    serial_hex: &str,
    revoked: &HashSet<String>,
    refreshed_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
    max_staleness: Duration,
    fail_mode: FailMode,
) -> Decision {
    // A serial known to be revoked is rejected regardless of freshness: stale
    // data can miss new revocations, never invent them.
    if revoked.contains(serial_hex) {
        return Decision::Revoked {
            serial_hex: serial_hex.to_string(),
        };
    }
    let age = match refreshed_at {
        Some(at) => now - at,
        None => Duration::MAX,
    };
    if age > max_staleness {
        let age_secs = age.whole_seconds();
        return match fail_mode {
            FailMode::HardFail => Decision::Stale { age_secs },
            FailMode::SoftFail => Decision::AllowStale { age_secs },
        };
    }
    Decision::Allow
}

/// A verified CRL reduced to what enforcement needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrlSummary {
    pub serials: HashSet<String>,
    pub this_update: OffsetDateTime,
    pub next_update: Option<OffsetDateTime>,
}

/// Parse a DER CRL and verify its signature against one of `issuer_pems`.
///
/// An unverified CRL is worse than none: it would let anyone who can answer
/// `GetCrl` (or tamper with the response) decide which certificates this node
/// rejects. Any candidate certificate whose public key validates the
/// signature is accepted, which lets callers pass the whole trust bundle
/// without knowing whether the root or the sub-CA signed the list.
pub fn parse_and_verify_crl(der: &[u8], issuer_pems: &[String]) -> Result<CrlSummary, String> {
    use x509_parser::prelude::FromDer;

    let (_, crl) = x509_parser::revocation_list::CertificateRevocationList::from_der(der)
        .map_err(|e| format!("CRL parse error: {e}"))?;

    let mut verified = false;
    let mut last_err = String::from("no candidate issuer certificate was supplied");
    for pem_bundle in issuer_pems {
        for block in pem::parse_many(pem_bundle).unwrap_or_default() {
            if block.tag() != "CERTIFICATE" {
                continue;
            }
            let Ok((_, candidate)) =
                x509_parser::certificate::X509Certificate::from_der(block.contents())
            else {
                continue;
            };
            match crl.verify_signature(candidate.public_key()) {
                Ok(()) => {
                    verified = true;
                    break;
                }
                Err(e) => last_err = format!("{e}"),
            }
        }
        if verified {
            break;
        }
    }
    if !verified {
        return Err(format!("CRL signature did not verify: {last_err}"));
    }

    let serials = crl
        .iter_revoked_certificates()
        .map(|entry| super::hex_upper(entry.raw_serial()))
        .collect();

    Ok(CrlSummary {
        serials,
        this_update: crl.last_update().to_datetime(),
        next_update: crl.next_update().map(|t| t.to_datetime()),
    })
}

#[derive(Default)]
struct Snapshot {
    serials: HashSet<String>,
    refreshed_at: Option<OffsetDateTime>,
    crl_next_update: Option<OffsetDateTime>,
    /// Serials confirmed `good` by a live OCSP query, with the time of the
    /// answer. A direct answer about one serial is stronger evidence than the
    /// bulk CRL, so it lets `hard-fail` admit that peer even while the CRL is
    /// stale.
    ocsp_good: std::collections::HashMap<String, OffsetDateTime>,
    /// Peer serials seen recently, so the refresh loop knows which ones to ask
    /// OCSP about when it cannot fetch a CRL.
    seen: HashSet<String>,
}

/// Cap on [`Snapshot::seen`]. The node-agent's only peers are the controllers
/// and `kctl`, so this is generous; the bound exists so a hostile peer cannot
/// grow the set without limit by reconnecting with fresh certificates.
const MAX_SEEN_SERIALS: usize = 256;

/// Shared revocation set used by the node-agent's inbound authorization path.
#[derive(Clone)]
pub struct RevocationState {
    inner: Arc<RwLock<Snapshot>>,
    fail_mode: FailMode,
    max_staleness: Duration,
}

impl RevocationState {
    pub fn new(fail_mode: FailMode, max_staleness: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Snapshot::default())),
            fail_mode,
            max_staleness,
        }
    }

    /// Enforcement disabled: nothing is revoked and staleness never bites.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Snapshot {
                serials: HashSet::new(),
                refreshed_at: Some(OffsetDateTime::now_utc() + Duration::days(36_500)),
                crl_next_update: None,
                ..Snapshot::default()
            })),
            fail_mode: FailMode::SoftFail,
            max_staleness: Duration::days(36_500),
        }
    }

    pub fn from_config(cfg: &NodeRevocationConfig) -> Self {
        if !cfg.enabled {
            return Self::disabled();
        }
        Self::new(
            FailMode::from_config_str(&cfg.fail_mode).unwrap_or_default(),
            Duration::seconds(cfg.max_staleness_secs as i64),
        )
    }

    pub fn fail_mode(&self) -> FailMode {
        self.fail_mode
    }

    /// Replace the revocation set from a verified CRL.
    pub fn apply(&self, summary: CrlSummary) -> usize {
        let count = summary.serials.len();
        if let Ok(mut guard) = self.inner.write() {
            guard.serials = summary.serials;
            guard.refreshed_at = Some(OffsetDateTime::now_utc());
            guard.crl_next_update = summary.next_update;
        }
        count
    }

    /// Number of revoked serials currently known, and how stale the data is in
    /// seconds (`None` when it has never been loaded). Used by status output.
    pub fn stats(&self) -> (usize, Option<i64>) {
        match self.inner.read() {
            Ok(guard) => (
                guard.serials.len(),
                guard
                    .refreshed_at
                    .map(|at| (OffsetDateTime::now_utc() - at).whole_seconds()),
            ),
            Err(_) => (0, None),
        }
    }

    /// Serials seen on inbound connections, for the OCSP backfill.
    pub fn seen_serials(&self) -> Vec<String> {
        match self.inner.read() {
            Ok(guard) => guard.seen.iter().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Record a live OCSP answer for one serial.
    pub fn apply_ocsp(&self, serial_hex: &str, status: &crate::pki::ocsp_client::OcspStatus) {
        use crate::pki::ocsp_client::OcspStatus;
        if let Ok(mut guard) = self.inner.write() {
            match status {
                OcspStatus::Good => {
                    guard
                        .ocsp_good
                        .insert(serial_hex.to_string(), OffsetDateTime::now_utc());
                }
                OcspStatus::Revoked { .. } => {
                    guard.ocsp_good.remove(serial_hex);
                    guard.serials.insert(serial_hex.to_string());
                }
                // `unknown` is not a statement that the certificate is fine
                // (RFC 6960 §2.2), so it changes nothing either way.
                OcspStatus::Unknown => {}
            }
        }
    }

    fn note_seen(&self, serial_hex: &str) {
        if let Ok(guard) = self.inner.read() {
            if guard.seen.contains(serial_hex) || guard.seen.len() >= MAX_SEEN_SERIALS {
                return;
            }
        }
        if let Ok(mut guard) = self.inner.write() {
            if guard.seen.len() < MAX_SEEN_SERIALS {
                guard.seen.insert(serial_hex.to_string());
            }
        }
    }

    pub fn check(&self, serial_hex: &str) -> Decision {
        self.note_seen(serial_hex);
        let (serials, refreshed_at, ocsp_good_at) = match self.inner.read() {
            Ok(guard) => (
                guard.serials.clone(),
                guard.refreshed_at,
                guard.ocsp_good.get(serial_hex).copied(),
            ),
            // A poisoned lock is indistinguishable from "no data"; the fail
            // mode decides what that means.
            Err(_) => (HashSet::new(), None, None),
        };
        let now = OffsetDateTime::now_utc();
        let decision = decide(
            serial_hex,
            &serials,
            refreshed_at,
            now,
            self.max_staleness,
            self.fail_mode,
        );
        match decision {
            // Only hard-fail rejections are worth overriding: soft-fail already
            // allowed the peer, and a revocation is never overridden.
            Decision::Stale { age_secs } => {
                let fresh_good = ocsp_good_at
                    .map(|at| now - at <= self.max_staleness)
                    .unwrap_or(false);
                if fresh_good {
                    Decision::Allow
                } else {
                    Decision::Stale { age_secs }
                }
            }
            other => other,
        }
    }

    /// Map [`Self::check`] onto a `tonic::Status`, logging soft-fail cases.
    #[allow(clippy::result_large_err)]
    pub fn check_status(&self, serial_hex: &str, peer_cn: &str) -> Result<(), tonic::Status> {
        match self.check(serial_hex) {
            Decision::Allow => Ok(()),
            Decision::AllowStale { age_secs } => {
                warn!(
                    peer = %peer_cn,
                    age_secs,
                    "revocation data is stale; allowing peer under soft-fail policy"
                );
                Ok(())
            }
            Decision::Revoked { serial_hex } => Err(tonic::Status::permission_denied(format!(
                "certificate {serial_hex} presented by '{peer_cn}' has been revoked"
            ))),
            Decision::Stale { age_secs } => Err(tonic::Status::unavailable(format!(
                "revocation data is {age_secs}s stale and the failure mode is hard-fail; rejecting '{peer_cn}'"
            ))),
        }
    }
}

/// Serial (uppercase hex) and CN of the peer's client certificate. `None` when
/// TLS is not in use or no client certificate was presented, in which case the
/// existing `require_peer` controls already decide the outcome.
pub fn peer_cert_identity<T>(request: &tonic::Request<T>) -> Option<(String, String)> {
    use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};

    let tls_info = request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()?;
    let certs = tls_info.peer_certs()?;
    let der = certs.first()?;
    let facts = super::facts_from_der(der.as_ref()).ok()?;
    Some((facts.serial_hex, facts.subject_cn))
}

/// A `tonic` interceptor that rejects revoked peer certificates. Applying it
/// once per service covers every RPC, rather than each handler remembering.
pub fn interceptor(
    state: RevocationState,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |request: tonic::Request<()>| match peer_cert_identity(&request) {
        Some((serial, cn)) => state.check_status(&serial, &cn).map(|()| request),
        None => Ok(request),
    }
}

/// Trust anchors the CRL signature may be checked against: the configured CA
/// bundle plus the sub-CA that shipped with our own certificate chain.
pub fn issuer_candidates(cfg: &Config) -> Vec<String> {
    let Some(tls) = cfg.tls.as_ref() else {
        return Vec::new();
    };
    [&tls.ca_file, &tls.cert_file]
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect()
}

/// Fetch the CRL from the first responsive controller and install it.
pub async fn refresh_once(cfg: &Config, state: &RevocationState) -> Result<usize, String> {
    let candidates = issuer_candidates(cfg);
    if candidates.is_empty() {
        return Err("no CA material configured; cannot verify a CRL".to_string());
    }

    let mut last_err = String::from("no controller endpoints configured");
    for endpoint in crate::registration::controller_endpoints(cfg) {
        let channel = match crate::registration::connect_channel(cfg, &endpoint).await {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("connecting to {endpoint}: {e}");
                continue;
            }
        };
        let mut client = controller_proto::controller_client::ControllerClient::new(channel);
        let resp = match client.get_crl(controller_proto::GetCrlRequest {}).await {
            Ok(r) => r.into_inner(),
            Err(e) => {
                last_err = format!("GetCrl on {endpoint}: {e}");
                continue;
            }
        };
        if !resp.success || resp.crl_der.is_empty() {
            last_err = format!("controller {endpoint} has no CRL: {}", resp.message);
            continue;
        }
        let summary = match parse_and_verify_crl(&resp.crl_der, &candidates) {
            Ok(s) => s,
            Err(e) => {
                // A CRL we cannot verify is discarded, not applied. Trying the
                // next controller is pointless if the material is bad, but
                // harmless, and covers a single misconfigured peer.
                last_err = format!("CRL from {endpoint} rejected: {e}");
                continue;
            }
        };
        let count = state.apply(summary);
        return Ok(count);
    }
    Err(last_err)
}

/// Ask the controller's OCSP responder about every peer serial we have seen.
///
/// Used only when the CRL could not be fetched. The CRL is the right shape for
/// bulk enforcement; OCSP is the right shape for "I cannot get the list, but I
/// can still ask about the handful of peers actually talking to me".
pub async fn ocsp_backfill(cfg: &Config, state: &RevocationState) -> Result<usize, String> {
    if !cfg.revocation.ocsp_enabled || cfg.revocation.ocsp_url.trim().is_empty() {
        return Err("OCSP point queries are not configured (revocation.ocspUrl)".to_string());
    }
    let candidates = issuer_candidates(cfg);
    if candidates.is_empty() {
        return Err("no CA material configured; cannot verify an OCSP response".to_string());
    }
    let serials = state.seen_serials();
    if serials.is_empty() {
        return Ok(0);
    }

    let mut answered = 0usize;
    for serial in serials {
        match crate::pki::ocsp_client::query(&cfg.revocation.ocsp_url, &candidates, &serial).await {
            Ok(status) => {
                if let crate::pki::ocsp_client::OcspStatus::Revoked { reason, .. } = &status {
                    warn!(
                        serial = %serial,
                        reason,
                        "OCSP reports a peer certificate as revoked"
                    );
                }
                state.apply_ocsp(&serial, &status);
                answered += 1;
            }
            Err(error) => warn!(serial = %serial, %error, "OCSP query failed"),
        }
    }
    Ok(answered)
}

/// Background loop that keeps the revocation set fresh.
pub fn spawn_crl_refresh_loop(cfg: Config, state: RevocationState) {
    if !cfg.revocation.enabled {
        warn!("peer certificate revocation checking is DISABLED (revocation.enabled: false)");
        return;
    }
    let interval = std::time::Duration::from_secs(cfg.revocation.fetch_interval_secs.max(30));
    info!(
        fail_mode = FailMode::from_config_str(&cfg.revocation.fail_mode)
            .unwrap_or_default()
            .as_str(),
        max_staleness_secs = cfg.revocation.max_staleness_secs,
        fetch_interval_secs = cfg.revocation.fetch_interval_secs,
        "peer certificate revocation checking enabled"
    );
    tokio::spawn(async move {
        loop {
            match refresh_once(&cfg, &state).await {
                Ok(count) => info!(revoked_serials = count, "refreshed CRL from controller"),
                Err(error) => {
                    warn!(%error, "failed to refresh CRL from controller");
                    // The bulk list is unavailable; fall back to asking about
                    // the peers we actually talk to.
                    match ocsp_backfill(&cfg, &state).await {
                        Ok(0) => {}
                        Ok(answered) => info!(
                            answered,
                            "CRL unavailable; refreshed peer status via OCSP point queries"
                        ),
                        Err(error) => warn!(%error, "OCSP fallback unavailable"),
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::test_support::{ensure_crypto_provider, node_leaf, test_ca};
    use crate::pki::{facts_from_pem, hex_upper};

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Build a CRL over `serials`, signed by `ca`, the way the controller does.
    fn build_crl(ca_cert: &str, ca_key: &str, serials: &[&str]) -> Vec<u8> {
        use rcgen::{
            CertificateRevocationListParams, Issuer, KeyPair, RevocationReason, RevokedCertParams,
            SerialNumber,
        };

        let now = OffsetDateTime::now_utc();
        let revoked = serials
            .iter()
            .map(|hex| {
                let bytes: Vec<u8> = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                    .collect();
                RevokedCertParams {
                    serial_number: SerialNumber::from_slice(&bytes),
                    revocation_time: now,
                    reason_code: Some(RevocationReason::KeyCompromise),
                    invalidity_date: None,
                }
            })
            .collect();

        let params = CertificateRevocationListParams {
            this_update: now,
            next_update: now + Duration::hours(24),
            crl_number: SerialNumber::from(1u64),
            issuing_distribution_point: None,
            revoked_certs: revoked,
            key_identifier_method: rcgen::KeyIdMethod::Sha256,
        };
        let key = KeyPair::from_pem(ca_key).expect("ca key");
        let issuer = Issuer::from_ca_cert_pem(ca_cert, key).expect("issuer");
        params.signed_by(&issuer).expect("sign crl").der().to_vec()
    }

    #[test]
    fn fail_mode_parses_config_spellings() {
        assert_eq!(
            FailMode::from_config_str("soft-fail"),
            Some(FailMode::SoftFail)
        );
        assert_eq!(
            FailMode::from_config_str("Hard-Fail"),
            Some(FailMode::HardFail)
        );
        assert_eq!(FailMode::from_config_str("nonsense"), None);
        assert_eq!(FailMode::default(), FailMode::SoftFail);
        assert_eq!(FailMode::HardFail.as_str(), "hard-fail");
    }

    #[test]
    fn revoked_serial_is_denied_under_both_fail_modes() {
        let now = OffsetDateTime::now_utc();
        for mode in [FailMode::SoftFail, FailMode::HardFail] {
            let decision = decide(
                "0A",
                &set(&["0A"]),
                Some(now),
                now,
                Duration::minutes(5),
                mode,
            );
            assert_eq!(
                decision,
                Decision::Revoked {
                    serial_hex: "0A".into()
                },
                "mode {mode:?}"
            );
        }
    }

    #[test]
    fn stale_data_soft_fails_open_and_hard_fails_closed() {
        let now = OffsetDateTime::now_utc();
        let refreshed = Some(now - Duration::hours(9));

        let soft = decide(
            "0C",
            &set(&["0A"]),
            refreshed,
            now,
            Duration::hours(6),
            FailMode::SoftFail,
        );
        assert!(soft.is_allowed());
        assert!(matches!(soft, Decision::AllowStale { age_secs } if age_secs == 9 * 3600));

        let hard = decide(
            "0C",
            &set(&["0A"]),
            refreshed,
            now,
            Duration::hours(6),
            FailMode::HardFail,
        );
        assert!(!hard.is_allowed());
        assert!(matches!(hard, Decision::Stale { age_secs } if age_secs == 9 * 3600));
    }

    #[test]
    fn a_node_that_never_fetched_a_crl_is_stale() {
        let now = OffsetDateTime::now_utc();
        assert!(matches!(
            decide(
                "0C",
                &HashSet::new(),
                None,
                now,
                Duration::hours(6),
                FailMode::HardFail
            ),
            Decision::Stale { .. }
        ));
        assert!(decide(
            "0C",
            &HashSet::new(),
            None,
            now,
            Duration::hours(6),
            FailMode::SoftFail
        )
        .is_allowed());
    }

    #[test]
    fn parse_and_verify_crl_extracts_serials_and_window() {
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let der = build_crl(&ca_cert, &ca_key, &["0A1B", "00FF00"]);

        let summary = parse_and_verify_crl(&der, &[ca_cert]).expect("verify");
        assert_eq!(summary.serials, set(&["0A1B", "00FF00"]));
        let next = summary.next_update.expect("next_update must be present");
        assert!(next > summary.this_update);
    }

    #[test]
    fn parse_and_verify_crl_rejects_a_crl_from_a_foreign_ca() {
        let (real_ca, real_key) = test_ca("test-sub-ca");
        let (rogue_ca, rogue_key) = test_ca("rogue-ca");
        let der = build_crl(&rogue_ca, &rogue_key, &["0A1B"]);

        let err = parse_and_verify_crl(&der, std::slice::from_ref(&real_ca))
            .expect_err("a CRL from an untrusted CA must be refused");
        assert!(err.contains("did not verify"), "{err}");
        // Sanity: the same list verifies against its own issuer.
        parse_and_verify_crl(&der, &[rogue_ca]).expect("own issuer verifies");
        let _ = real_key;
        let _ = rogue_key;
    }

    #[test]
    fn parse_and_verify_crl_rejects_garbage_and_missing_issuers() {
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        assert!(
            parse_and_verify_crl(&[0xde, 0xad, 0xbe, 0xef], std::slice::from_ref(&ca_cert))
                .is_err()
        );

        let der = build_crl(&ca_cert, &ca_key, &["0A"]);
        let err = parse_and_verify_crl(&der, &[]).expect_err("no issuers, no trust");
        assert!(err.contains("no candidate issuer"), "{err}");
    }

    #[test]
    fn parse_and_verify_crl_finds_the_issuer_inside_a_bundle() {
        let (root_cert, _root_key) = test_ca("test-root-ca");
        let (sub_cert, sub_key) = test_ca("test-sub-ca");
        let der = build_crl(&sub_cert, &sub_key, &["0A"]);

        // A realistic ca_file: several anchors concatenated, in any order.
        let bundle = format!("{root_cert}{sub_cert}");
        let summary = parse_and_verify_crl(&der, &[bundle]).expect("verify from bundle");
        assert_eq!(summary.serials, set(&["0A"]));
    }

    #[test]
    fn state_apply_makes_a_revoked_peer_rejected() {
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (chain, _key) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(30));
        let serial = facts_from_pem(&chain).expect("facts").serial_hex;

        let state = RevocationState::new(FailMode::HardFail, Duration::hours(6));
        // Before any CRL: hard-fail treats the node as having no data.
        assert!(matches!(state.check(&serial), Decision::Stale { .. }));

        let empty = build_crl(&ca_cert, &ca_key, &[]);
        state.apply(parse_and_verify_crl(&empty, std::slice::from_ref(&ca_cert)).expect("verify"));
        assert_eq!(state.check(&serial), Decision::Allow);
        assert!(state.check_status(&serial, "kcore-node-10.0.0.5").is_ok());

        let der = build_crl(&ca_cert, &ca_key, &[&serial]);
        let count = state.apply(parse_and_verify_crl(&der, &[ca_cert]).expect("verify"));
        assert_eq!(count, 1);
        assert!(matches!(state.check(&serial), Decision::Revoked { .. }));
        let status = state
            .check_status(&serial, "kcore-node-10.0.0.5")
            .expect_err("revoked peer must be rejected");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
        assert!(status.message().contains("revoked"), "{status:?}");
    }

    #[test]
    fn stats_reports_size_and_age() {
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let state = RevocationState::new(FailMode::SoftFail, Duration::hours(6));
        assert_eq!(state.stats(), (0, None));

        let der = build_crl(&ca_cert, &ca_key, &["0A", "0B"]);
        state.apply(parse_and_verify_crl(&der, &[ca_cert]).expect("verify"));
        let (count, age) = state.stats();
        assert_eq!(count, 2);
        assert!(age.expect("age") < 5);
    }

    #[test]
    fn hard_fail_state_reports_unavailable_not_permission_denied() {
        let state = RevocationState::new(FailMode::HardFail, Duration::seconds(1));
        let status = state
            .check_status("00FF", "kctl:alice")
            .expect_err("stale hard-fail must reject");
        assert_eq!(
            status.code(),
            tonic::Code::Unavailable,
            "stale data is an availability problem, not an authz decision"
        );
    }

    #[test]
    fn disabled_state_allows_everything() {
        let state = RevocationState::disabled();
        assert_eq!(state.check("00FF"), Decision::Allow);
        assert!(state.check_status("00FF", "kctl:alice").is_ok());
        assert_eq!(state.fail_mode(), FailMode::SoftFail);
    }

    #[test]
    fn from_config_honours_enabled_and_fail_mode() {
        let disabled = RevocationState::from_config(&NodeRevocationConfig {
            enabled: false,
            ..Default::default()
        });
        assert_eq!(disabled.check("00FF"), Decision::Allow);

        let hard = RevocationState::from_config(&NodeRevocationConfig {
            enabled: true,
            fail_mode: "hard-fail".to_string(),
            max_staleness_secs: 1,
            ..Default::default()
        });
        assert_eq!(hard.fail_mode(), FailMode::HardFail);
        assert!(matches!(hard.check("00FF"), Decision::Stale { .. }));
    }

    #[test]
    fn a_live_ocsp_good_answer_overrides_a_stale_crl_under_hard_fail() {
        use crate::pki::ocsp_client::OcspStatus;

        // Never fetched a CRL, so hard-fail would reject everything.
        let state = RevocationState::new(FailMode::HardFail, Duration::hours(6));
        assert!(matches!(state.check("00FF"), Decision::Stale { .. }));

        state.apply_ocsp("00FF", &OcspStatus::Good);
        assert_eq!(
            state.check("00FF"),
            Decision::Allow,
            "a direct answer about this serial beats an unavailable bulk list"
        );
        // Another serial is unaffected: the override is per-serial.
        assert!(matches!(state.check("00AA"), Decision::Stale { .. }));
    }

    #[test]
    fn an_ocsp_revoked_answer_denies_even_without_a_crl() {
        use crate::pki::ocsp_client::OcspStatus;

        let state = RevocationState::new(FailMode::SoftFail, Duration::hours(6));
        // Soft-fail would otherwise let this through.
        assert!(state.check("00FF").is_allowed());
        state.apply_ocsp(
            "00FF",
            &OcspStatus::Revoked {
                reason: 1,
                revoked_at: OffsetDateTime::now_utc(),
            },
        );
        assert!(matches!(state.check("00FF"), Decision::Revoked { .. }));
    }

    #[test]
    fn an_ocsp_unknown_answer_changes_nothing() {
        use crate::pki::ocsp_client::OcspStatus;

        let state = RevocationState::new(FailMode::SoftFail, Duration::hours(6));
        state.apply_ocsp("00FF", &OcspStatus::Unknown);
        // Still soft-fail-allowed, but not recorded as verified good either.
        assert!(matches!(state.check("00FF"), Decision::AllowStale { .. }));

        let hard = RevocationState::new(FailMode::HardFail, Duration::hours(6));
        hard.apply_ocsp("00FF", &OcspStatus::Unknown);
        assert!(
            matches!(hard.check("00FF"), Decision::Stale { .. }),
            "RFC 6960 unknown is not a statement that the certificate is fine"
        );
    }

    #[test]
    fn ocsp_good_does_not_override_a_revocation() {
        use crate::pki::ocsp_client::OcspStatus;

        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let state = RevocationState::new(FailMode::SoftFail, Duration::hours(6));
        let der = build_crl(&ca_cert, &ca_key, &["00FF"]);
        state.apply(parse_and_verify_crl(&der, &[ca_cert]).expect("verify"));

        state.apply_ocsp("00FF", &OcspStatus::Good);
        assert!(
            matches!(state.check("00FF"), Decision::Revoked { .. }),
            "a stale responder must not resurrect a revoked certificate"
        );
    }

    #[test]
    fn checking_a_serial_records_it_for_the_ocsp_backfill() {
        let state = RevocationState::new(FailMode::SoftFail, Duration::hours(6));
        assert!(state.seen_serials().is_empty());
        state.check("00FF");
        state.check("00AA");
        state.check("00FF");
        let mut seen = state.seen_serials();
        seen.sort();
        assert_eq!(seen, vec!["00AA".to_string(), "00FF".to_string()]);
    }

    #[test]
    fn the_seen_set_is_bounded() {
        let state = RevocationState::new(FailMode::SoftFail, Duration::hours(6));
        for i in 0..(MAX_SEEN_SERIALS + 50) {
            state.check(&format!("{i:08X}"));
        }
        assert_eq!(
            state.seen_serials().len(),
            MAX_SEEN_SERIALS,
            "a peer reconnecting with fresh certificates must not grow the set without limit"
        );
    }

    #[tokio::test]
    async fn ocsp_backfill_is_skipped_when_not_configured() {
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
            cert_rotation: crate::config::CertRotationConfig::default(),
            revocation: NodeRevocationConfig::default(),
        };
        let state = RevocationState::new(FailMode::SoftFail, Duration::hours(6));
        let err = ocsp_backfill(&cfg, &state)
            .await
            .expect_err("no ocspUrl configured");
        assert!(err.contains("revocation.ocspUrl"), "{err}");
    }

    #[test]
    fn interceptor_passes_requests_without_a_peer_certificate() {
        let state = RevocationState::new(FailMode::HardFail, Duration::hours(6));
        let mut intercept = interceptor(state);
        assert!(intercept(tonic::Request::new(())).is_ok());
        assert!(peer_cert_identity(&tonic::Request::new(())).is_none());
    }

    #[test]
    fn issuer_candidates_is_empty_without_tls() {
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
            cert_rotation: crate::config::CertRotationConfig::default(),
            revocation: NodeRevocationConfig::default(),
        };
        assert!(issuer_candidates(&cfg).is_empty());
    }

    #[tokio::test]
    async fn refresh_once_reports_an_unreachable_controller_without_clearing_the_set() {
        ensure_crypto_provider();
        let dir = tempfile::tempdir().expect("tempdir");
        let (ca_cert, ca_key) = test_ca("test-sub-ca");
        let (chain, key_pem) = node_leaf(&ca_cert, &ca_key, "10.0.0.5", Duration::days(30));
        let ca_file = dir.path().join("ca.crt");
        let cert_file = dir.path().join("node.crt");
        let key_file = dir.path().join("node.key");
        std::fs::write(&ca_file, &ca_cert).expect("ca");
        std::fs::write(&cert_file, &chain).expect("cert");
        std::fs::write(&key_file, &key_pem).expect("key");

        let cfg = Config {
            node_id: "node-1".to_string(),
            listen_addr: "0.0.0.0:9091".to_string(),
            controller_addr: String::new(),
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
            cert_rotation: crate::config::CertRotationConfig::default(),
            revocation: NodeRevocationConfig::default(),
        };

        // Seed a known-good set, then fail a refresh: the set must survive so
        // soft-fail keeps enforcing the revocations it already knows about.
        let state = RevocationState::new(FailMode::SoftFail, Duration::hours(6));
        let der = build_crl(&ca_cert, &ca_key, &["0A1B"]);
        state.apply(parse_and_verify_crl(&der, &[ca_cert]).expect("verify"));

        let err = refresh_once(&cfg, &state)
            .await
            .expect_err("unreachable controller");
        assert!(!err.is_empty(), "{err}");
        assert_eq!(state.stats().0, 1, "the last known CRL must be retained");
        assert!(matches!(state.check("0A1B"), Decision::Revoked { .. }));
        assert_eq!(hex_upper(&[0x0a, 0x1b]), "0A1B");
    }
}

/// Property tests for the fail-mode decision table.
#[cfg(test)]
mod prop_tests {
    use super::{decide, Decision, FailMode};
    use proptest::prelude::*;
    use std::collections::HashSet;
    use time::{Duration, OffsetDateTime};

    proptest! {
        #![proptest_config(ProptestConfig { cases: 1_000, .. ProptestConfig::default() })]

        /// A revoked serial is denied for every age, staleness bound and fail
        /// mode. Stale data can miss revocations; it can never invent them, so
        /// there is no case where a known-revoked serial is allowed.
        #[test]
        fn revoked_is_never_allowed(
            serial in "[0-9A-F]{2,16}",
            age_secs in 0i64..1_000_000,
            max_stale in 0i64..1_000_000,
            hard in any::<bool>(),
        ) {
            let now = OffsetDateTime::now_utc();
            let mut revoked = HashSet::new();
            revoked.insert(serial.clone());
            let decision = decide(
                &serial,
                &revoked,
                Some(now - Duration::seconds(age_secs)),
                now,
                Duration::seconds(max_stale),
                if hard { FailMode::HardFail } else { FailMode::SoftFail },
            );
            prop_assert!(!decision.is_allowed());
        }

        /// Soft-fail always allows an unrevoked serial, no matter how stale the
        /// data is. That is the whole point: a fetch outage must not brick the
        /// cluster.
        #[test]
        fn soft_fail_never_denies_an_unrevoked_serial(
            serial in "[0-9A-F]{2,16}",
            age_secs in 0i64..10_000_000,
            max_stale in 0i64..1_000_000,
        ) {
            let now = OffsetDateTime::now_utc();
            let decision = decide(
                &serial,
                &HashSet::new(),
                Some(now - Duration::seconds(age_secs)),
                now,
                Duration::seconds(max_stale),
                FailMode::SoftFail,
            );
            prop_assert!(decision.is_allowed());
        }

        /// Hard-fail allows an unrevoked serial exactly while the data is
        /// within the staleness bound, and denies it otherwise.
        #[test]
        fn hard_fail_tracks_the_staleness_bound(
            serial in "[0-9A-F]{2,16}",
            age_secs in 0i64..1_000_000,
            max_stale in 0i64..1_000_000,
        ) {
            let now = OffsetDateTime::now_utc();
            let decision = decide(
                &serial,
                &HashSet::new(),
                Some(now - Duration::seconds(age_secs)),
                now,
                Duration::seconds(max_stale),
                FailMode::HardFail,
            );
            if age_secs <= max_stale {
                prop_assert_eq!(decision, Decision::Allow);
            } else {
                prop_assert!(
                    matches!(decision, Decision::Stale { .. }),
                    "expected Stale, got {:?}",
                    decision
                );
            }
        }
    }
}
