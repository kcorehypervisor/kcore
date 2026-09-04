//! RFC 6960 OCSP responder.
//!
//! The controller *is* the issuing CA, so responses are signed directly with
//! the sub-CA key (a "CA-signed" responder, RFC 6960 §4.2.2.2 first bullet) —
//! there is no delegated responder certificate to distribute or rotate.
//!
//! ASN.1 structures come from the RustCrypto `x509-ocsp` crate; the signature
//! over `tbsResponseData` is produced by `rcgen`'s `SigningKey`, i.e. the same
//! aws-lc-rs backend the TLS stack uses. Nothing here hand-rolls DER.
//!
//! ## What is not implemented
//!
//! OCSP **stapling** is not available: `tonic::transport::ServerTlsConfig`
//! offers no hook for a `rustls::sign::CertifiedKey`, so the controller cannot
//! attach a stapled response to its own handshake. Clients therefore query
//! this responder directly (see `crates/node-agent/src/pki/ocsp_client.rs`).

use const_oid::db::rfc5912::{ID_SHA_1, ID_SHA_256};
use der::asn1::{BitString, OctetString};
use der::{Decode, Encode};
use rcgen::{KeyPair, SigningKey};
use spki::AlgorithmIdentifierOwned;
use time::{Duration, OffsetDateTime};
use x509_cert::serial_number::SerialNumber;
use x509_ocsp::{
    BasicOcspResponse, CertId, CertStatus, OcspGeneralizedTime, OcspRequest, OcspResponse,
    ResponderId, ResponseData, RevokedInfo, SingleResponse,
};
use x509_parser::prelude::FromDer;

use crate::db::{Database, CERT_STATUS_REVOKED};
use crate::grpc::SubCaState;
use crate::pki::{hex_upper, parse_ts, sha1_identifier};

/// Revocation state of one serial, as far as this responder can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SerialStatus {
    /// Issued by us and not revoked.
    Good,
    Revoked {
        reason: i32,
        revoked_at: OffsetDateTime,
    },
    /// Not in our inventory. RFC 6960 §2.2 reserves `good` for certificates we
    /// know we issued, so anything unknown answers `unknown`.
    Unknown,
}

/// Look up a serial in the certificate inventory.
pub fn lookup_status(db: &Database, serial_hex: &str) -> SerialStatus {
    match db.get_issued_certificate(serial_hex) {
        Ok(Some(row)) if row.status == CERT_STATUS_REVOKED => SerialStatus::Revoked {
            reason: row.revocation_reason,
            revoked_at: parse_ts(&row.revoked_at).unwrap_or_else(OffsetDateTime::now_utc),
        },
        Ok(Some(_)) => SerialStatus::Good,
        Ok(None) => SerialStatus::Unknown,
        Err(_) => SerialStatus::Unknown,
    }
}

/// The `CertID` values a request is asking about.
pub fn parse_request(der: &[u8]) -> Result<Vec<CertId>, String> {
    let request = OcspRequest::from_der(der).map_err(|e| format!("malformed OCSP request: {e}"))?;
    let ids: Vec<CertId> = request
        .tbs_request
        .request_list
        .iter()
        .map(|r| r.req_cert.clone())
        .collect();
    if ids.is_empty() {
        return Err("OCSP request contains no certificates".to_string());
    }
    Ok(ids)
}

/// Uppercase-hex serial carried by a `CertID`.
pub fn cert_id_serial_hex(cert_id: &CertId) -> String {
    hex_upper(cert_id.serial_number.as_bytes())
}

/// Whether a `CertID` names our sub-CA as the issuer.
///
/// A request naming some other CA gets `unknown` rather than `good`, so this
/// responder never vouches for certificates outside its own scope.
pub fn cert_id_matches_issuer(cert_id: &CertId, sub_ca_cert_pem: &str) -> bool {
    let Ok(block) = pem::parse(sub_ca_cert_pem) else {
        return false;
    };
    let Ok((_, cert)) = x509_parser::certificate::X509Certificate::from_der(block.contents())
    else {
        return false;
    };
    let name_der = cert.subject().as_raw();
    let key_bits = cert.public_key().subject_public_key.data.as_ref();

    let (expected_name, expected_key) = if cert_id.hash_algorithm.oid == ID_SHA_1 {
        (sha1_identifier(name_der), sha1_identifier(key_bits))
    } else if cert_id.hash_algorithm.oid == ID_SHA_256 {
        (crate::pki::sha256(name_der), crate::pki::sha256(key_bits))
    } else {
        return false;
    };

    cert_id.issuer_name_hash.as_bytes() == expected_name.as_slice()
        && cert_id.issuer_key_hash.as_bytes() == expected_key.as_slice()
}

/// Build a `CertID` for `serial_hex` issued by `sub_ca_cert_pem`, using SHA-1
/// identifiers as RFC 6960 clients expect. Used by tests and by the
/// controller's own probes.
pub fn build_cert_id(sub_ca_cert_pem: &str, serial_hex: &str) -> Result<CertId, String> {
    let block = pem::parse(sub_ca_cert_pem).map_err(|e| format!("sub-CA PEM: {e}"))?;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(block.contents())
        .map_err(|e| format!("sub-CA X.509: {e}"))?;
    let serial = super::crl::serial_bytes(serial_hex).ok_or("unparseable serial")?;
    Ok(CertId {
        hash_algorithm: AlgorithmIdentifierOwned {
            oid: ID_SHA_1,
            parameters: None,
        },
        issuer_name_hash: OctetString::new(sha1_identifier(cert.subject().as_raw()))
            .map_err(|e| format!("issuerNameHash: {e}"))?,
        issuer_key_hash: OctetString::new(sha1_identifier(
            cert.public_key().subject_public_key.data.as_ref(),
        ))
        .map_err(|e| format!("issuerKeyHash: {e}"))?,
        serial_number: SerialNumber::new(&serial).map_err(|e| format!("serialNumber: {e}"))?,
    })
}

/// Build and sign a successful `OCSPResponse` (DER) covering `cert_ids`.
///
/// `validity` sets `nextUpdate` on each `SingleResponse`, telling clients how
/// long they may cache the answer.
pub fn build_response(
    db: &Database,
    sub_ca: &SubCaState,
    cert_ids: &[CertId],
    validity: Duration,
) -> Result<Vec<u8>, String> {
    if !sub_ca.is_available() {
        return Err("sub-CA is not configured; cannot sign OCSP responses".to_string());
    }
    let produced_at = OffsetDateTime::now_utc();
    let this_update = to_ocsp_time(produced_at)?;
    let next_update = to_ocsp_time(produced_at + validity)?;

    let mut responses = Vec::with_capacity(cert_ids.len());
    for cert_id in cert_ids {
        let status = if cert_id_matches_issuer(cert_id, &sub_ca.cert_pem) {
            lookup_status(db, &cert_id_serial_hex(cert_id))
        } else {
            SerialStatus::Unknown
        };
        let cert_status = match status {
            SerialStatus::Good => CertStatus::good(),
            SerialStatus::Unknown => CertStatus::unknown(),
            SerialStatus::Revoked { reason, revoked_at } => CertStatus::revoked(RevokedInfo {
                revocation_time: to_ocsp_time(revoked_at)?,
                revocation_reason: crl_reason(reason),
            }),
        };
        responses.push(SingleResponse {
            cert_id: cert_id.clone(),
            cert_status,
            this_update,
            next_update: Some(next_update),
            single_extensions: None,
        });
    }

    let key = KeyPair::from_pem(&sub_ca.key_pem).map_err(|e| format!("loading sub-CA key: {e}"))?;
    let responder_id = responder_id_by_key(&sub_ca.cert_pem)?;

    let tbs = ResponseData {
        version: x509_ocsp::Version::V1,
        responder_id,
        produced_at: to_ocsp_time(produced_at)?,
        responses,
        response_extensions: None,
    };
    let tbs_der = tbs
        .to_der()
        .map_err(|e| format!("encoding tbsResponseData: {e}"))?;
    let signature = key
        .sign(&tbs_der)
        .map_err(|e| format!("signing OCSP response: {e}"))?;

    let basic = BasicOcspResponse {
        tbs_response_data: tbs,
        signature_algorithm: signature_algorithm(&key)?,
        signature: BitString::new(0, signature)
            .map_err(|e| format!("signature BIT STRING: {e}"))?,
        certs: None,
    };
    OcspResponse::successful(basic)
        .map_err(|e| format!("wrapping OCSP response: {e}"))?
        .to_der()
        .map_err(|e| format!("encoding OCSPResponse: {e}"))
}

/// Pre-encoded `OCSPResponse` shells for the non-successful statuses.
pub fn malformed_request_der() -> Vec<u8> {
    OcspResponse::malformed_request()
        .to_der()
        .unwrap_or_default()
}

pub fn internal_error_der() -> Vec<u8> {
    OcspResponse::internal_error().to_der().unwrap_or_default()
}

pub fn try_later_der() -> Vec<u8> {
    OcspResponse::try_later().to_der().unwrap_or_default()
}

fn responder_id_by_key(sub_ca_cert_pem: &str) -> Result<ResponderId, String> {
    let block = pem::parse(sub_ca_cert_pem).map_err(|e| format!("sub-CA PEM: {e}"))?;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(block.contents())
        .map_err(|e| format!("sub-CA X.509: {e}"))?;
    let hash = sha1_identifier(cert.public_key().subject_public_key.data.as_ref());
    Ok(ResponderId::ByKey(
        OctetString::new(hash).map_err(|e| format!("responder KeyHash: {e}"))?,
    ))
}

/// `AlgorithmIdentifier` for the signature the sub-CA key produces.
///
/// rcgen picks the digest from the key type, so the mapping is by key
/// algorithm. Anything outside the project's approved set (P-256, P-384,
/// RSA-2048+, Ed25519) is refused rather than guessed.
fn signature_algorithm(key: &KeyPair) -> Result<AlgorithmIdentifierOwned, String> {
    let alg = key.algorithm();
    // ecdsa-with-SHA256 / ecdsa-with-SHA384 / sha256WithRSAEncryption / Ed25519.
    let oid = if alg == &rcgen::PKCS_ECDSA_P256_SHA256 {
        "1.2.840.10045.4.3.2"
    } else if alg == &rcgen::PKCS_ECDSA_P384_SHA384 {
        "1.2.840.10045.4.3.3"
    } else if alg == &rcgen::PKCS_RSA_SHA256 {
        "1.2.840.113549.1.1.11"
    } else if alg == &rcgen::PKCS_ED25519 {
        "1.3.101.112"
    } else {
        return Err(
            "sub-CA key algorithm is not supported for OCSP signing (expected ECDSA P-256/P-384, RSA-SHA256 or Ed25519)"
                .to_string(),
        );
    };
    let oid = der::asn1::ObjectIdentifier::new(oid).map_err(|e| format!("signature OID: {e}"))?;
    // RSA requires explicit NULL parameters (RFC 4055 §2.1); ECDSA and Ed25519
    // must omit them (RFC 5758 §3.2, RFC 8410 §3).
    let parameters = if alg == &rcgen::PKCS_RSA_SHA256 {
        Some(
            der::Any::from_der(&der::asn1::Null.to_der().map_err(|e| e.to_string())?)
                .map_err(|e| format!("NULL parameters: {e}"))?,
        )
    } else {
        None
    };
    Ok(AlgorithmIdentifierOwned { oid, parameters })
}

fn crl_reason(code: i32) -> Option<x509_cert::ext::pkix::CrlReason> {
    use x509_cert::ext::pkix::CrlReason;
    match code {
        0 => Some(CrlReason::Unspecified),
        1 => Some(CrlReason::KeyCompromise),
        2 => Some(CrlReason::CaCompromise),
        3 => Some(CrlReason::AffiliationChanged),
        4 => Some(CrlReason::Superseded),
        5 => Some(CrlReason::CessationOfOperation),
        6 => Some(CrlReason::CertificateHold),
        8 => Some(CrlReason::RemoveFromCRL),
        9 => Some(CrlReason::PrivilegeWithdrawn),
        10 => Some(CrlReason::AaCompromise),
        _ => None,
    }
}

fn to_ocsp_time(t: OffsetDateTime) -> Result<OcspGeneralizedTime, String> {
    let system: std::time::SystemTime = t.into();
    OcspGeneralizedTime::try_from(system).map_err(|e| format!("encoding GeneralizedTime: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::signing::sign_node_cert;
    use crate::grpc::signing::test_support::{generate_test_ca, generate_test_sub_ca};
    use crate::pki::{format_ts, inventory};

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

    /// Decode a DER OCSPResponse and return the first SingleResponse.
    fn single_response(der: &[u8]) -> SingleResponse {
        let response = OcspResponse::from_der(der).expect("decode OCSPResponse");
        assert_eq!(
            response.response_status,
            x509_ocsp::OcspResponseStatus::Successful
        );
        let bytes = response.response_bytes.expect("responseBytes");
        let basic = BasicOcspResponse::from_der(bytes.response.as_bytes()).expect("basic");
        basic
            .tbs_response_data
            .responses
            .first()
            .cloned()
            .expect("one SingleResponse")
    }

    fn verify_signature(der: &[u8], sub_ca_cert_pem: &str) -> bool {
        let response = OcspResponse::from_der(der).expect("decode");
        let bytes = response.response_bytes.expect("responseBytes");
        let basic = BasicOcspResponse::from_der(bytes.response.as_bytes()).expect("basic");
        let tbs = basic.tbs_response_data.to_der().expect("tbs der");
        let block = pem::parse(sub_ca_cert_pem).expect("pem");
        let (_, cert) =
            x509_parser::certificate::X509Certificate::from_der(block.contents()).expect("x509");
        let public_key = aws_lc_rs::signature::UnparsedPublicKey::new(
            &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
            cert.public_key().subject_public_key.data.as_ref(),
        );
        public_key.verify(&tbs, basic.signature.raw_bytes()).is_ok()
    }

    #[test]
    fn responds_good_for_an_issued_serial() {
        let db = Database::open(":memory:").expect("db");
        let sub = sub_ca_state();
        let (chain, _) = sign_node_cert(&sub.cert_pem, &sub.key_pem, "10.0.1.1").expect("sign");
        let meta = inventory::record_signed_chain(&db, &chain, "node-a").expect("record");

        let cert_id = build_cert_id(&sub.cert_pem, &meta.serial_hex).expect("cert id");
        let der = build_response(&db, &sub, &[cert_id], Duration::hours(1)).expect("response");

        let single = single_response(&der);
        assert!(matches!(single.cert_status, CertStatus::Good(_)));
        assert!(single.next_update.is_some());
        assert!(
            verify_signature(&der, &sub.cert_pem),
            "OCSP response must verify against the sub-CA public key"
        );
    }

    #[test]
    fn responds_revoked_with_reason_and_time() {
        let db = Database::open(":memory:").expect("db");
        let sub = sub_ca_state();
        let (chain, _) = sign_node_cert(&sub.cert_pem, &sub.key_pem, "10.0.1.2").expect("sign");
        let meta = inventory::record_signed_chain(&db, &chain, "node-b").expect("record");
        let revoked_at = OffsetDateTime::now_utc().replace_nanosecond(0).expect("ns");
        db.revoke_certificate_by_serial(&meta.serial_hex, 1, &format_ts(revoked_at))
            .expect("revoke");

        let cert_id = build_cert_id(&sub.cert_pem, &meta.serial_hex).expect("cert id");
        let der = build_response(&db, &sub, &[cert_id], Duration::hours(1)).expect("response");

        let single = single_response(&der);
        let CertStatus::Revoked(info) = single.cert_status else {
            panic!("expected revoked, got {:?}", single.cert_status);
        };
        assert_eq!(
            info.revocation_reason,
            Some(x509_cert::ext::pkix::CrlReason::KeyCompromise)
        );
        assert_eq!(
            info.revocation_time.0.to_unix_duration().as_secs() as i64,
            revoked_at.unix_timestamp()
        );
        assert!(verify_signature(&der, &sub.cert_pem));
    }

    #[test]
    fn responds_unknown_for_a_serial_we_never_issued() {
        let db = Database::open(":memory:").expect("db");
        let sub = sub_ca_state();
        let cert_id = build_cert_id(&sub.cert_pem, "DEADBEEF").expect("cert id");
        let der = build_response(&db, &sub, &[cert_id], Duration::hours(1)).expect("response");

        let single = single_response(&der);
        assert!(matches!(single.cert_status, CertStatus::Unknown(_)));
        assert!(verify_signature(&der, &sub.cert_pem));
    }

    #[test]
    fn responds_unknown_for_a_serial_issued_by_another_ca() {
        let db = Database::open(":memory:").expect("db");
        let ours = sub_ca_state();
        let theirs = sub_ca_state();

        // A serial we did issue, but asked about under a foreign issuer.
        let (chain, _) = sign_node_cert(&ours.cert_pem, &ours.key_pem, "10.0.1.3").expect("sign");
        let meta = inventory::record_signed_chain(&db, &chain, "node-c").expect("record");

        let foreign_id = build_cert_id(&theirs.cert_pem, &meta.serial_hex).expect("cert id");
        assert!(!cert_id_matches_issuer(&foreign_id, &ours.cert_pem));
        let der = build_response(&db, &ours, &[foreign_id], Duration::hours(1)).expect("response");
        assert!(matches!(
            single_response(&der).cert_status,
            CertStatus::Unknown(_)
        ));
    }

    #[test]
    fn responds_to_every_cert_id_in_a_batched_request() {
        let db = Database::open(":memory:").expect("db");
        let sub = sub_ca_state();
        let (chain, _) = sign_node_cert(&sub.cert_pem, &sub.key_pem, "10.0.1.4").expect("sign");
        let good = inventory::record_signed_chain(&db, &chain, "node-d").expect("record");
        let (chain2, _) = sign_node_cert(&sub.cert_pem, &sub.key_pem, "10.0.1.5").expect("sign");
        let bad = inventory::record_signed_chain(&db, &chain2, "node-e").expect("record");
        db.revoke_certificate_by_serial(&bad.serial_hex, 4, &format_ts(OffsetDateTime::now_utc()))
            .expect("revoke");

        let ids = vec![
            build_cert_id(&sub.cert_pem, &good.serial_hex).expect("id"),
            build_cert_id(&sub.cert_pem, &bad.serial_hex).expect("id"),
            build_cert_id(&sub.cert_pem, "AABBCC").expect("id"),
        ];
        let der = build_response(&db, &sub, &ids, Duration::hours(1)).expect("response");
        let response = OcspResponse::from_der(&der).expect("decode");
        let bytes = response.response_bytes.expect("responseBytes");
        let basic = BasicOcspResponse::from_der(bytes.response.as_bytes()).expect("basic");
        let statuses = &basic.tbs_response_data.responses;
        assert_eq!(statuses.len(), 3);
        assert!(matches!(statuses[0].cert_status, CertStatus::Good(_)));
        assert!(matches!(statuses[1].cert_status, CertStatus::Revoked(_)));
        assert!(matches!(statuses[2].cert_status, CertStatus::Unknown(_)));
    }

    #[test]
    fn build_response_requires_a_sub_ca() {
        let db = Database::open(":memory:").expect("db");
        let sub = sub_ca_state();
        let cert_id = build_cert_id(&sub.cert_pem, "01").expect("id");
        let err = build_response(&db, &SubCaState::default(), &[cert_id], Duration::hours(1))
            .expect_err("must fail without sub-CA");
        assert!(err.contains("sub-CA is not configured"), "got {err}");
    }

    #[test]
    fn parse_request_round_trips_a_generated_request() {
        let sub = sub_ca_state();
        let cert_id = build_cert_id(&sub.cert_pem, "0102FF").expect("id");
        let request = OcspRequest {
            tbs_request: x509_ocsp::TbsRequest {
                version: x509_ocsp::Version::V1,
                requestor_name: None,
                request_list: vec![x509_ocsp::Request {
                    req_cert: cert_id,
                    single_request_extensions: None,
                }],
                request_extensions: None,
            },
            optional_signature: None,
        };
        let der = request.to_der().expect("der");
        let parsed = parse_request(&der).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(cert_id_serial_hex(&parsed[0]), "0102FF");
    }

    #[test]
    fn parse_request_rejects_garbage_and_empty_lists() {
        assert!(parse_request(b"not der at all").is_err());
        let empty = OcspRequest {
            tbs_request: x509_ocsp::TbsRequest::default(),
            optional_signature: None,
        };
        let der = empty.to_der().expect("der");
        assert!(parse_request(&der).is_err());
    }

    #[test]
    fn non_successful_shells_decode_with_the_right_status() {
        for (der, expected) in [
            (
                malformed_request_der(),
                x509_ocsp::OcspResponseStatus::MalformedRequest,
            ),
            (
                internal_error_der(),
                x509_ocsp::OcspResponseStatus::InternalError,
            ),
            (try_later_der(), x509_ocsp::OcspResponseStatus::TryLater),
        ] {
            let response = OcspResponse::from_der(&der).expect("decode");
            assert_eq!(response.response_status, expected);
            assert!(response.response_bytes.is_none());
        }
    }
}
