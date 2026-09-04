//! Revocation enforcement for inbound peer certificates.
//!
//! `tonic` 0.12 builds its `rustls` `ServerConfig` internally and exposes no
//! hook for a custom `ClientCertVerifier`, so rustls' own CRL support
//! (`WebPkiClientVerifier::with_crls`) is unreachable from
//! `ServerTlsConfig`. Enforcement therefore happens one layer up: every
//! authenticated RPC checks the serial of the presented client certificate
//! against the revocation set before the handler runs.
//!
//! On the controller the revocation set comes straight from
//! `issued_certificates`, so it is authoritative and always fresh. The
//! staleness policy exists for symmetry with the node-agent (which fetches a
//! CRL over the network) and to cover the case where the database itself
//! cannot be read.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use time::{Duration, OffsetDateTime};
use tracing::warn;

use crate::db::Database;

/// What to do when revocation data cannot be refreshed within
/// `max_staleness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailMode {
    /// Keep accepting peers whose serial is not in the last known revocation
    /// set, and log a warning. The default: a transient database or CRL
    /// problem must not lock every node out of the cluster.
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
    /// The serial is in the revocation set.
    Revoked {
        serial_hex: String,
    },
    /// Revocation data is too old and the fail mode is `hard-fail`.
    Stale {
        age_secs: i64,
    },
    /// Revocation data is too old but the fail mode is `soft-fail`.
    AllowStale {
        age_secs: i64,
    },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow | Decision::AllowStale { .. })
    }
}

/// Pure decision function: given a revocation snapshot, decide about a serial.
///
/// `refreshed_at` is `None` when the set has never been loaded, which counts
/// as maximally stale.
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

#[derive(Default)]
struct Snapshot {
    serials: HashSet<String>,
    refreshed_at: Option<OffsetDateTime>,
}

/// Shared revocation set used by the controller's authorization path.
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

    /// Enforcement disabled: nothing is ever considered revoked and staleness
    /// never bites. Used when `revocation.enabled` is false.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Snapshot {
                serials: HashSet::new(),
                // Far-future timestamp so the set never looks stale.
                refreshed_at: Some(OffsetDateTime::now_utc() + Duration::days(36_500)),
            })),
            fail_mode: FailMode::SoftFail,
            max_staleness: Duration::days(36_500),
        }
    }

    pub fn fail_mode(&self) -> FailMode {
        self.fail_mode
    }

    /// Reload the revoked serial set from the certificate inventory.
    pub fn refresh(&self, db: &Database) -> Result<usize, String> {
        let serials = db
            .revoked_serial_set()
            .map_err(|e| format!("loading revoked serials: {e}"))?;
        let count = serials.len();
        if let Ok(mut guard) = self.inner.write() {
            guard.serials = serials;
            guard.refreshed_at = Some(OffsetDateTime::now_utc());
        }
        Ok(count)
    }

    /// Immediately add a serial to the in-memory set so a revocation takes
    /// effect on the next RPC without waiting for the refresh tick.
    pub fn insert_revoked(&self, serial_hex: &str) {
        if let Ok(mut guard) = self.inner.write() {
            guard.serials.insert(serial_hex.to_string());
        }
    }

    pub fn check(&self, serial_hex: &str) -> Decision {
        let (serials, refreshed_at) = match self.inner.read() {
            Ok(guard) => (guard.serials.clone(), guard.refreshed_at),
            // A poisoned lock is indistinguishable from "no data"; the fail
            // mode decides what that means.
            Err(_) => (HashSet::new(), None),
        };
        decide(
            serial_hex,
            &serials,
            refreshed_at,
            OffsetDateTime::now_utc(),
            self.max_staleness,
            self.fail_mode,
        )
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

/// Serial (uppercase hex) and CN of the peer's client certificate.
///
/// `None` when TLS is not in use or no client certificate was presented; the
/// caller then has nothing to check and the existing `--allow-insecure` /
/// `require_peer` controls apply.
pub fn peer_cert_identity<T>(request: &tonic::Request<T>) -> Option<(String, String)> {
    use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
    use x509_parser::prelude::FromDer;

    let tls_info = request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()?;
    let certs = tls_info.peer_certs()?;
    let der = certs.first()?;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der.as_ref()).ok()?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|c| c.as_str().ok())
        .unwrap_or_default()
        .to_string();
    Some((crate::pki::hex_upper(cert.raw_serial()), cn))
}

/// A `tonic` interceptor that rejects revoked peer certificates.
///
/// Applying enforcement as an interceptor means every RPC on the wrapped
/// service is covered from one wiring point, instead of each handler having to
/// remember the check.
pub fn interceptor(
    state: RevocationState,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |request: tonic::Request<()>| match peer_cert_identity(&request) {
        Some((serial, cn)) => state.check_status(&serial, &cn).map(|()| request),
        None => Ok(request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::signing::sign_node_cert;
    use crate::grpc::signing::test_support::{generate_test_ca, generate_test_sub_ca};
    use crate::pki::{format_ts, inventory};

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
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
        assert_eq!(FailMode::from_config_str("hard"), Some(FailMode::HardFail));
        assert_eq!(FailMode::from_config_str("maybe"), None);
        assert_eq!(FailMode::default(), FailMode::SoftFail);
        assert_eq!(FailMode::SoftFail.as_str(), "soft-fail");
    }

    #[test]
    fn revoked_serial_is_denied_under_both_fail_modes() {
        let now = OffsetDateTime::now_utc();
        for mode in [FailMode::SoftFail, FailMode::HardFail] {
            let decision = decide(
                "0A",
                &set(&["0A", "0B"]),
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
            assert!(!decision.is_allowed());
        }
    }

    #[test]
    fn fresh_data_allows_an_unrevoked_serial() {
        let now = OffsetDateTime::now_utc();
        let decision = decide(
            "0C",
            &set(&["0A"]),
            Some(now - Duration::seconds(10)),
            now,
            Duration::minutes(5),
            FailMode::HardFail,
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn stale_data_soft_fails_open_and_hard_fails_closed() {
        let now = OffsetDateTime::now_utc();
        let refreshed = Some(now - Duration::hours(2));

        let soft = decide(
            "0C",
            &set(&["0A"]),
            refreshed,
            now,
            Duration::minutes(5),
            FailMode::SoftFail,
        );
        assert!(soft.is_allowed(), "soft-fail must keep the cluster running");
        assert!(matches!(soft, Decision::AllowStale { age_secs } if age_secs == 7200));

        let hard = decide(
            "0C",
            &set(&["0A"]),
            refreshed,
            now,
            Duration::minutes(5),
            FailMode::HardFail,
        );
        assert!(!hard.is_allowed(), "hard-fail must reject on stale data");
        assert!(matches!(hard, Decision::Stale { age_secs } if age_secs == 7200));
    }

    #[test]
    fn never_refreshed_data_counts_as_stale() {
        let now = OffsetDateTime::now_utc();
        assert!(matches!(
            decide(
                "0C",
                &HashSet::new(),
                None,
                now,
                Duration::minutes(5),
                FailMode::HardFail
            ),
            Decision::Stale { .. }
        ));
        assert!(matches!(
            decide(
                "0C",
                &HashSet::new(),
                None,
                now,
                Duration::minutes(5),
                FailMode::SoftFail
            ),
            Decision::AllowStale { .. }
        ));
    }

    #[test]
    fn state_refresh_picks_up_database_revocations() {
        let db = Database::open(":memory:").expect("db");
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (chain, _) = sign_node_cert(&sub_cert, &sub_key, "10.0.2.1").expect("sign");
        let meta = inventory::record_signed_chain(&db, &chain, "node-a").expect("record");

        let state = RevocationState::new(FailMode::HardFail, Duration::minutes(5));
        assert_eq!(state.refresh(&db).expect("refresh"), 0);
        assert_eq!(state.check(&meta.serial_hex), Decision::Allow);

        db.revoke_certificate_by_serial(&meta.serial_hex, 1, &format_ts(OffsetDateTime::now_utc()))
            .expect("revoke");
        assert_eq!(state.refresh(&db).expect("refresh"), 1);
        assert!(matches!(
            state.check(&meta.serial_hex),
            Decision::Revoked { .. }
        ));
        assert!(state
            .check_status(&meta.serial_hex, "kcore-node-10.0.2.1")
            .is_err());
    }

    #[test]
    fn insert_revoked_takes_effect_without_a_refresh() {
        let db = Database::open(":memory:").expect("db");
        let state = RevocationState::new(FailMode::SoftFail, Duration::minutes(5));
        state.refresh(&db).expect("refresh");
        assert_eq!(state.check("00FF"), Decision::Allow);
        state.insert_revoked("00FF");
        assert!(matches!(state.check("00FF"), Decision::Revoked { .. }));
    }

    #[test]
    fn disabled_state_allows_everything_forever() {
        let state = RevocationState::disabled();
        assert_eq!(state.check("00FF"), Decision::Allow);
        assert!(state.check_status("00FF", "kcore-node-1").is_ok());
    }

    #[test]
    fn check_status_reports_unavailable_for_stale_hard_fail() {
        let state = RevocationState::new(FailMode::HardFail, Duration::seconds(1));
        let status = state
            .check_status("00FF", "kcore-node-1")
            .expect_err("stale hard-fail must reject");
        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert!(status.message().contains("hard-fail"), "{status:?}");
    }

    #[test]
    fn interceptor_passes_requests_without_a_peer_certificate() {
        let db = Database::open(":memory:").expect("db");
        let state = RevocationState::new(FailMode::HardFail, Duration::minutes(5));
        state.refresh(&db).expect("refresh");
        let mut intercept = interceptor(state);
        // No TLS extension: nothing to check, and the transport-level controls
        // already decided whether this connection is allowed at all.
        assert!(intercept(tonic::Request::new(())).is_ok());
    }

    #[test]
    fn peer_cert_identity_is_none_without_tls_info() {
        assert!(peer_cert_identity(&tonic::Request::new(())).is_none());
    }
}
