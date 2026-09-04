//! CRL generation, caching and distribution state.
//!
//! The controller is the CA, so the authoritative revocation set lives in
//! `issued_certificates`. The CRL is a signed, cacheable projection of that
//! set: regenerated whenever the revocation set changes or `nextUpdate` gets
//! close, persisted in `crl_state` so `crlNumber` stays monotonic across
//! restarts, and served over gRPC (`GetCrl`) and HTTP (`/pki/crl.der`).

use std::sync::{Arc, RwLock};

use rcgen::{
    CertificateRevocationList, CertificateRevocationListParams, Issuer, KeyIdMethod, KeyPair,
    RevokedCertParams, SerialNumber,
};
use time::{Duration, OffsetDateTime};
use tracing::{info, warn};

use crate::db::{CrlStateRow, Database};
use crate::grpc::SubCaState;
use crate::pki::{format_ts, hex_lower, parse_ts, rcgen_reason, sha256};

/// Signed CRL plus the metadata operators and HTTP clients need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedCrl {
    pub crl_number: i64,
    pub this_update: OffsetDateTime,
    pub next_update: OffsetDateTime,
    pub pem: String,
    pub der: Vec<u8>,
    pub revoked_count: i32,
}

impl SignedCrl {
    fn from_row(row: &CrlStateRow) -> Option<Self> {
        Some(Self {
            crl_number: row.crl_number,
            this_update: parse_ts(&row.this_update)?,
            next_update: parse_ts(&row.next_update)?,
            pem: row.crl_pem.clone(),
            der: row.crl_der.clone(),
            revoked_count: row.revoked_count,
        })
    }

    fn to_row(&self, issuer_fingerprint: &str) -> CrlStateRow {
        CrlStateRow {
            crl_number: self.crl_number,
            this_update: format_ts(self.this_update),
            next_update: format_ts(self.next_update),
            crl_pem: self.pem.clone(),
            crl_der: self.der.clone(),
            revoked_count: self.revoked_count,
            issuer_fingerprint: issuer_fingerprint.to_string(),
        }
    }
}

/// In-memory copy of the current CRL, shared between the gRPC handlers, the
/// HTTP responder and the regeneration loop.
#[derive(Clone, Default)]
pub struct CrlCache {
    inner: Arc<RwLock<Option<SignedCrl>>>,
}

impl CrlCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> Option<SignedCrl> {
        self.inner.read().ok().and_then(|g| g.clone())
    }

    pub fn set(&self, crl: SignedCrl) {
        if let Ok(mut g) = self.inner.write() {
            *g = Some(crl);
        }
    }

    /// Load the persisted CRL into memory at startup so restarts serve the
    /// last known-good list immediately, before the first regeneration tick.
    pub fn load_from_db(&self, db: &Database) {
        match db.get_crl_state() {
            Ok(Some(row)) => match SignedCrl::from_row(&row) {
                Some(crl) => {
                    info!(
                        crl_number = crl.crl_number,
                        revoked = crl.revoked_count,
                        "loaded persisted CRL"
                    );
                    self.set(crl);
                }
                None => warn!("persisted CRL has unparseable timestamps; ignoring"),
            },
            Ok(None) => {}
            Err(error) => warn!(%error, "reading persisted CRL state"),
        }
    }
}

/// Build and sign a CRL covering `revoked` using the sub-CA.
///
/// `revoked` is `(serial_hex, reason_code, revoked_at_rfc3339)`. Entries whose
/// serial is not valid hex are skipped with a warning rather than failing the
/// whole CRL — one bad row must not stop revocation of everything else.
pub fn build_crl(
    sub_ca_cert_pem: &str,
    sub_ca_key_pem: &str,
    revoked: &[(String, i32, String)],
    crl_number: i64,
    validity: Duration,
) -> Result<SignedCrl, String> {
    let now = OffsetDateTime::now_utc();
    // Second precision keeps the DER GeneralizedTime and the persisted
    // timestamp string identical.
    let this_update = now.replace_nanosecond(0).unwrap_or(now);
    let next_update = this_update + validity;

    let mut revoked_certs = Vec::with_capacity(revoked.len());
    for (serial_hex, reason, revoked_at) in revoked {
        let Some(bytes) = serial_bytes(serial_hex) else {
            warn!(serial = %serial_hex, "skipping CRL entry with unparseable serial");
            continue;
        };
        revoked_certs.push(RevokedCertParams {
            serial_number: SerialNumber::from(bytes),
            revocation_time: parse_ts(revoked_at).unwrap_or(this_update),
            reason_code: Some(rcgen_reason(*reason)),
            invalidity_date: None,
        });
    }
    let revoked_count = revoked_certs.len() as i32;

    let params = CertificateRevocationListParams {
        this_update,
        next_update,
        crl_number: SerialNumber::from(crl_number.max(0) as u64),
        issuing_distribution_point: None,
        revoked_certs,
        key_identifier_method: KeyIdMethod::Sha256,
    };

    let key = KeyPair::from_pem(sub_ca_key_pem).map_err(|e| format!("loading sub-CA key: {e}"))?;
    let issuer = Issuer::from_ca_cert_pem(sub_ca_cert_pem, key)
        .map_err(|e| format!("loading sub-CA cert: {e}"))?;
    let crl: CertificateRevocationList = params
        .signed_by(&issuer)
        .map_err(|e| format!("signing CRL: {e}"))?;

    Ok(SignedCrl {
        crl_number,
        this_update,
        next_update,
        pem: crl.pem().map_err(|e| format!("encoding CRL PEM: {e}"))?,
        der: crl.der().to_vec(),
        revoked_count,
    })
}

/// Hex serial to big-endian DER integer bytes. An odd-length string is
/// left-padded, so `A` and `0A` denote the same serial.
pub fn serial_bytes(serial_hex: &str) -> Option<Vec<u8>> {
    let s = crate::pki::normalize_serial(serial_hex);
    if s.is_empty() {
        return None;
    }
    let padded = if s.len() % 2 == 1 { format!("0{s}") } else { s };
    let mut out = Vec::with_capacity(padded.len() / 2);
    let bytes = padded.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Regenerate the CRL if needed and publish it to `cache` and `crl_state`.
///
/// Regeneration happens when there is no CRL yet, when the revoked set no
/// longer matches the cached count, when the sub-CA changed, or when
/// `nextUpdate` is inside `refresh_before`. Returns the CRL in force, or
/// `None` when no sub-CA is configured (revocation is then enforced from the
/// database only — see `docs/mtls-bootstrap-and-auth.md`).
pub fn ensure_current(
    db: &Database,
    sub_ca: &SubCaState,
    cache: &CrlCache,
    validity: Duration,
    refresh_before: Duration,
    force: bool,
) -> Result<Option<SignedCrl>, String> {
    if !sub_ca.is_available() {
        return Ok(None);
    }
    let revoked = db
        .list_revoked_certificates()
        .map_err(|e| format!("listing revoked certificates: {e}"))?;
    let fingerprint = hex_lower(&sha256(sub_ca.cert_pem.as_bytes()));

    let persisted = db
        .get_crl_state()
        .map_err(|e| format!("reading CRL state: {e}"))?;
    let current = cache
        .get()
        .or_else(|| persisted.as_ref().and_then(SignedCrl::from_row));

    let now = OffsetDateTime::now_utc();
    let issuer_changed = persisted
        .as_ref()
        .map(|p| p.issuer_fingerprint != fingerprint)
        .unwrap_or(true);
    let needs_regen = force
        || issuer_changed
        || match &current {
            None => true,
            Some(crl) => {
                crl.revoked_count != revoked.len() as i32 || crl.next_update - now <= refresh_before
            }
        };

    if !needs_regen {
        return Ok(current);
    }

    let next_number = current.as_ref().map(|c| c.crl_number + 1).unwrap_or(1);
    let crl = build_crl(
        &sub_ca.cert_pem,
        &sub_ca.key_pem,
        &revoked,
        next_number,
        validity,
    )?;
    db.put_crl_state(&crl.to_row(&fingerprint))
        .map_err(|e| format!("persisting CRL state: {e}"))?;
    cache.set(crl.clone());
    info!(
        crl_number = crl.crl_number,
        revoked = crl.revoked_count,
        next_update = %format_ts(crl.next_update),
        "signed new CRL"
    );
    Ok(Some(crl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::signing::sign_node_cert;
    use crate::grpc::signing::test_support::{generate_test_ca, generate_test_sub_ca};
    use crate::pki::inventory;

    fn sub_ca_state() -> SubCaState {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        SubCaState {
            cert_pem: sub_cert,
            key_pem: sub_key,
            cert_file: None,
            key_file: None,
        }
    }

    #[test]
    fn serial_bytes_pads_odd_length_and_rejects_empty() {
        assert_eq!(serial_bytes("0A"), Some(vec![0x0a]));
        assert_eq!(serial_bytes("A"), Some(vec![0x0a]));
        assert_eq!(serial_bytes("0x01ff"), Some(vec![0x01, 0xff]));
        assert_eq!(serial_bytes(""), None);
        assert_eq!(serial_bytes("zz"), None);
    }

    #[test]
    fn build_crl_signature_verifies_against_the_sub_ca() {
        let sub = sub_ca_state();
        let crl = build_crl(
            &sub.cert_pem,
            &sub.key_pem,
            &[(
                "0A1B2C".to_string(),
                1,
                format_ts(OffsetDateTime::now_utc()),
            )],
            1,
            Duration::hours(24),
        )
        .expect("build crl");

        use x509_parser::prelude::FromDer;
        let (_, parsed) =
            x509_parser::revocation_list::CertificateRevocationList::from_der(&crl.der)
                .expect("parse crl");
        let sub_der = pem::parse(&sub.cert_pem).expect("pem");
        let (_, sub_cert) =
            x509_parser::certificate::X509Certificate::from_der(sub_der.contents()).expect("x509");
        parsed
            .verify_signature(sub_cert.public_key())
            .expect("CRL must be signed by the sub-CA");

        // A different CA must not validate the same CRL.
        let other = sub_ca_state();
        let other_der = pem::parse(&other.cert_pem).expect("pem");
        let (_, other_cert) =
            x509_parser::certificate::X509Certificate::from_der(other_der.contents())
                .expect("x509");
        assert!(parsed.verify_signature(other_cert.public_key()).is_err());
    }

    #[test]
    fn build_crl_lists_revoked_serials_with_reason_and_update_window() {
        let sub = sub_ca_state();
        let revoked_at = format_ts(OffsetDateTime::now_utc());
        let crl = build_crl(
            &sub.cert_pem,
            &sub.key_pem,
            &[
                ("01".to_string(), 1, revoked_at.clone()),
                ("02FF".to_string(), 4, revoked_at),
            ],
            7,
            Duration::hours(12),
        )
        .expect("build crl");
        assert_eq!(crl.revoked_count, 2);
        assert_eq!(crl.crl_number, 7);
        assert!(crl.pem.contains("BEGIN X509 CRL"));

        use x509_parser::prelude::FromDer;
        let (_, parsed) =
            x509_parser::revocation_list::CertificateRevocationList::from_der(&crl.der)
                .expect("parse crl");

        let serials: Vec<String> = parsed
            .iter_revoked_certificates()
            .map(|rc| crate::pki::hex_upper(rc.raw_serial()))
            .collect();
        assert!(serials.contains(&"01".to_string()), "got {serials:?}");
        assert!(serials.contains(&"02FF".to_string()), "got {serials:?}");

        let reasons: Vec<u8> = parsed
            .iter_revoked_certificates()
            .filter_map(|rc| rc.reason_code().map(|(_, r)| r.0))
            .collect();
        assert!(reasons.contains(&1), "keyCompromise missing: {reasons:?}");
        assert!(reasons.contains(&4), "superseded missing: {reasons:?}");

        // thisUpdate <= now < nextUpdate, and the window is what we asked for.
        let this_update = parsed.last_update().timestamp();
        let next_update = parsed.next_update().expect("nextUpdate").timestamp();
        assert!(next_update - this_update == 12 * 3600);
        assert_eq!(crl.next_update - crl.this_update, Duration::hours(12));
    }

    #[test]
    fn build_crl_skips_unparseable_serials() {
        let sub = sub_ca_state();
        let now = format_ts(OffsetDateTime::now_utc());
        let crl = build_crl(
            &sub.cert_pem,
            &sub.key_pem,
            &[
                ("".to_string(), 0, now.clone()),
                ("zzz".to_string(), 0, now.clone()),
                ("0F".to_string(), 0, now),
            ],
            1,
            Duration::hours(1),
        )
        .expect("build crl");
        assert_eq!(crl.revoked_count, 1);
    }

    #[test]
    fn ensure_current_is_a_no_op_without_a_sub_ca() {
        let db = Database::open(":memory:").expect("db");
        let cache = CrlCache::new();
        let got = ensure_current(
            &db,
            &SubCaState::default(),
            &cache,
            Duration::hours(24),
            Duration::hours(1),
            false,
        )
        .expect("ensure");
        assert!(got.is_none());
        assert!(cache.get().is_none());
    }

    #[test]
    fn ensure_current_increments_crl_number_only_when_regenerating() {
        let db = Database::open(":memory:").expect("db");
        let sub = sub_ca_state();
        let cache = CrlCache::new();

        let first = ensure_current(
            &db,
            &sub,
            &cache,
            Duration::hours(24),
            Duration::hours(1),
            false,
        )
        .expect("first")
        .expect("crl");
        assert_eq!(first.crl_number, 1);
        assert_eq!(first.revoked_count, 0);

        // Nothing changed: same CRL, same number.
        let second = ensure_current(
            &db,
            &sub,
            &cache,
            Duration::hours(24),
            Duration::hours(1),
            false,
        )
        .expect("second")
        .expect("crl");
        assert_eq!(second.crl_number, 1);

        // A new revocation forces a regeneration with the next number.
        let (chain, _) = sign_node_cert(&sub.cert_pem, &sub.key_pem, "10.0.0.30").expect("sign");
        let meta = inventory::record_signed_chain(&db, &chain, "node-a").expect("record");
        db.revoke_certificate_by_serial(&meta.serial_hex, 1, &format_ts(OffsetDateTime::now_utc()))
            .expect("revoke");

        let third = ensure_current(
            &db,
            &sub,
            &cache,
            Duration::hours(24),
            Duration::hours(1),
            false,
        )
        .expect("third")
        .expect("crl");
        assert_eq!(third.crl_number, 2);
        assert_eq!(third.revoked_count, 1);
    }

    #[test]
    fn ensure_current_regenerates_when_next_update_is_close() {
        let db = Database::open(":memory:").expect("db");
        let sub = sub_ca_state();
        let cache = CrlCache::new();

        let first = ensure_current(
            &db,
            &sub,
            &cache,
            Duration::minutes(10),
            Duration::hours(1),
            false,
        )
        .expect("first")
        .expect("crl");
        // Validity (10 min) is inside the refresh window (1 h), so the next
        // pass must roll a new CRL immediately.
        let second = ensure_current(
            &db,
            &sub,
            &cache,
            Duration::minutes(10),
            Duration::hours(1),
            false,
        )
        .expect("second")
        .expect("crl");
        assert_eq!(second.crl_number, first.crl_number + 1);
    }

    #[test]
    fn ensure_current_persists_and_reloads_the_crl() {
        let db = Database::open(":memory:").expect("db");
        let sub = sub_ca_state();
        let cache = CrlCache::new();
        let signed = ensure_current(
            &db,
            &sub,
            &cache,
            Duration::hours(24),
            Duration::hours(1),
            false,
        )
        .expect("ensure")
        .expect("crl");

        let reloaded = CrlCache::new();
        reloaded.load_from_db(&db);
        let got = reloaded.get().expect("reloaded crl");
        assert_eq!(got.crl_number, signed.crl_number);
        assert_eq!(got.der, signed.der);
    }

    #[test]
    fn ensure_current_regenerates_after_sub_ca_rotation() {
        let db = Database::open(":memory:").expect("db");
        let cache = CrlCache::new();
        let first = ensure_current(
            &db,
            &sub_ca_state(),
            &cache,
            Duration::hours(24),
            Duration::hours(1),
            false,
        )
        .expect("first")
        .expect("crl");

        let rotated = ensure_current(
            &db,
            &sub_ca_state(),
            &cache,
            Duration::hours(24),
            Duration::hours(1),
            false,
        )
        .expect("rotated")
        .expect("crl");
        assert_eq!(rotated.crl_number, first.crl_number + 1);
    }
}
