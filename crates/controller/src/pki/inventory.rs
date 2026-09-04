//! Parse issued certificates into inventory rows.
//!
//! Every signing path in the controller funnels through here so the
//! `issued_certificates` table is the single source of truth for "what did we
//! issue, to whom, and when does it die".

use time::OffsetDateTime;
use x509_parser::prelude::FromDer;

use crate::db::{Database, IssuedCertRow, CERT_STATUS_ACTIVE};
use crate::pki::{format_ts, hex_lower, hex_upper, sha256, REASON_NONE};

/// Identity classes tracked in the inventory. The kind is derived from the
/// subject CN so it stays consistent with `auth.rs` peer classification.
pub const KIND_NODE: &str = "node";
pub const KIND_OPERATOR: &str = "operator";
pub const KIND_CONTROLLER: &str = "controller";
pub const KIND_SUB_CA: &str = "sub-ca";
pub const KIND_UNKNOWN: &str = "unknown";

/// Facts extracted from a signed certificate, before it is attributed to a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertMetadata {
    pub serial_hex: String,
    pub subject_cn: String,
    pub issuer_cn: String,
    pub fingerprint_sha256: String,
    pub not_before: OffsetDateTime,
    pub not_after: OffsetDateTime,
    pub is_ca: bool,
}

impl CertMetadata {
    /// Identity kind implied by the subject CN, matching the CN conventions in
    /// [`crate::auth`].
    pub fn identity_kind(&self) -> &'static str {
        if self.is_ca {
            return KIND_SUB_CA;
        }
        if self.subject_cn.starts_with(crate::auth::CN_NODE_PREFIX) {
            KIND_NODE
        } else if self
            .subject_cn
            .starts_with(crate::auth::CN_CONTROLLER_PREFIX)
        {
            KIND_CONTROLLER
        } else if self.subject_cn == crate::auth::CN_KCTL
            || self.subject_cn.starts_with(crate::auth::CN_KCTL_PREFIX)
        {
            KIND_OPERATOR
        } else {
            KIND_UNKNOWN
        }
    }

    /// Build an `active` inventory row for this certificate.
    pub fn to_row(&self, node_id: &str, issued_at: OffsetDateTime) -> IssuedCertRow {
        IssuedCertRow {
            serial_hex: self.serial_hex.clone(),
            subject_cn: self.subject_cn.clone(),
            identity_kind: self.identity_kind().to_string(),
            node_id: node_id.to_string(),
            issuer_cn: self.issuer_cn.clone(),
            fingerprint_sha256: self.fingerprint_sha256.clone(),
            not_before: format_ts(self.not_before),
            not_after: format_ts(self.not_after),
            issued_at: format_ts(issued_at),
            status: CERT_STATUS_ACTIVE.to_string(),
            revocation_reason: REASON_NONE,
            revoked_at: String::new(),
        }
    }
}

/// The first PEM certificate block of `chain_pem`, i.e. the leaf.
pub fn first_cert_pem(chain_pem: &str) -> Option<&str> {
    const END: &str = "-----END CERTIFICATE-----";
    let start = chain_pem.find("-----BEGIN CERTIFICATE-----")?;
    let end = chain_pem[start..].find(END)? + start + END.len();
    Some(&chain_pem[start..end])
}

/// Extract inventory metadata from a single DER certificate.
pub fn metadata_from_der(der: &[u8]) -> Result<CertMetadata, String> {
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der)
        .map_err(|e| format!("X.509 parse error: {e}"))?;
    let cn = |name: &x509_parser::x509::X509Name<'_>| {
        name.iter_common_name()
            .next()
            .and_then(|c| c.as_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let is_ca = cert
        .basic_constraints()
        .ok()
        .flatten()
        .map(|bc| bc.value.ca)
        .unwrap_or(false);
    Ok(CertMetadata {
        serial_hex: hex_upper(cert.raw_serial()),
        subject_cn: cn(cert.subject()),
        issuer_cn: cn(cert.issuer()),
        fingerprint_sha256: hex_lower(&sha256(der)),
        not_before: cert.validity().not_before.to_datetime(),
        not_after: cert.validity().not_after.to_datetime(),
        is_ca,
    })
}

/// Extract inventory metadata from the leaf of a PEM chain.
pub fn metadata_from_pem(chain_pem: &str) -> Result<CertMetadata, String> {
    let leaf = first_cert_pem(chain_pem).ok_or("no PEM certificate block found")?;
    let block = pem::parse(leaf).map_err(|e| format!("PEM parse error: {e}"))?;
    metadata_from_der(block.contents())
}

/// Record a freshly signed chain in the inventory and demote any previously
/// active certificate for the same subject to `rotated`.
///
/// Inventory bookkeeping is deliberately best-effort at the call sites: a
/// failure here must not fail an otherwise successful signing operation, so
/// callers log the error rather than propagate it.
pub fn record_signed_chain(
    db: &Database,
    chain_pem: &str,
    node_id: &str,
) -> Result<CertMetadata, String> {
    let meta = metadata_from_pem(chain_pem)?;
    let row = meta.to_row(node_id, OffsetDateTime::now_utc());
    db.record_issued_certificate(&row)
        .map_err(|e| format!("recording issued certificate: {e}"))?;
    db.mark_superseded_certificates(&meta.subject_cn, &meta.serial_hex)
        .map_err(|e| format!("marking superseded certificates: {e}"))?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::signing::test_support::{generate_test_ca, generate_test_sub_ca};
    use crate::grpc::signing::{sign_node_cert, sign_operator_cert};

    #[test]
    fn first_cert_pem_takes_only_the_leaf() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (chain, _) = sign_node_cert(&sub_cert, &sub_key, "10.0.0.7").expect("sign");
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 2);
        let leaf = first_cert_pem(&chain).expect("leaf");
        assert_eq!(leaf.matches("BEGIN CERTIFICATE").count(), 1);
    }

    #[test]
    fn metadata_from_pem_reads_subject_serial_and_validity() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (chain, _) = sign_node_cert(&sub_cert, &sub_key, "10.0.0.8").expect("sign");
        let meta = metadata_from_pem(&chain).expect("metadata");
        assert_eq!(meta.subject_cn, "kcore-node-10.0.0.8");
        assert_eq!(meta.identity_kind(), KIND_NODE);
        assert!(!meta.serial_hex.is_empty());
        assert!(meta
            .serial_hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_lowercase()));
        assert_eq!(meta.fingerprint_sha256.len(), 64);
        assert!(meta.not_after > meta.not_before);
        assert!(!meta.is_ca);
    }

    #[test]
    fn identity_kind_covers_every_cn_convention() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);

        let (node_chain, _) = sign_node_cert(&sub_cert, &sub_key, "10.0.0.9").expect("node");
        assert_eq!(
            metadata_from_pem(&node_chain)
                .expect("meta")
                .identity_kind(),
            KIND_NODE
        );

        let (op_chain, _) = sign_operator_cert(&sub_cert, &sub_key, "alice").expect("operator");
        assert_eq!(
            metadata_from_pem(&op_chain).expect("meta").identity_kind(),
            KIND_OPERATOR
        );

        assert_eq!(
            metadata_from_pem(&sub_cert).expect("meta").identity_kind(),
            KIND_SUB_CA
        );
    }

    #[test]
    fn record_signed_chain_demotes_previous_active_cert() {
        let db = Database::open(":memory:").expect("db");
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);

        let (first, _) = sign_node_cert(&sub_cert, &sub_key, "10.0.0.10").expect("first");
        let first_meta = record_signed_chain(&db, &first, "node-a").expect("record first");
        let (second, _) = sign_node_cert(&sub_cert, &sub_key, "10.0.0.10").expect("second");
        let second_meta = record_signed_chain(&db, &second, "node-a").expect("record second");
        assert_ne!(first_meta.serial_hex, second_meta.serial_hex);

        let old = db
            .get_issued_certificate(&first_meta.serial_hex)
            .expect("get")
            .expect("row");
        assert_eq!(old.status, crate::db::CERT_STATUS_ROTATED);

        let new = db
            .get_issued_certificate(&second_meta.serial_hex)
            .expect("get")
            .expect("row");
        assert_eq!(new.status, CERT_STATUS_ACTIVE);
        assert_eq!(new.node_id, "node-a");
    }

    #[test]
    fn metadata_from_pem_rejects_non_pem_input() {
        assert!(metadata_from_pem("not a certificate").is_err());
    }
}
