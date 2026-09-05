//! Client-side OCSP: direct point queries against the controller's responder.
//!
//! ## Why this exists, and what it is not
//!
//! OCSP **stapling** is not reachable from this codebase. `tonic` 0.12 builds
//! its `rustls` config from `ServerTlsConfig`/`ClientTlsConfig`, neither of
//! which exposes a `rustls::sign::CertifiedKey` (server side, needed to attach
//! `ocsp` bytes to a handshake) or a `ServerCertVerifier` (client side, needed
//! to consume a stapled response). So no stapled response is produced or
//! consumed during a KCore handshake.
//!
//! What *is* implemented is a direct RFC 6960 query: build an `OCSPRequest`
//! for one serial, POST it to the controller's `/pki/ocsp` endpoint, verify
//! the response signature against the issuing CA, and read the status. The
//! node-agent uses it as an escape hatch when the CRL has gone stale: a live
//! `good` answer for the one serial in front of us is better than failing the
//! whole connection closed, and a live `revoked` answer is better than a CRL
//! we have not managed to fetch.
//!
//! ASN.1 comes from the RustCrypto `x509-ocsp` crate and signature
//! verification from `aws-lc-rs`, the same FIPS-validated backend the TLS
//! stack uses. Nothing here hand-rolls DER.

use der::asn1::OctetString;
use der::{Decode, Encode};
use time::OffsetDateTime;
use x509_ocsp::{
    BasicOcspResponse, CertId, CertStatus, OcspRequest, OcspResponse, OcspResponseStatus, Request,
    TbsRequest, Version,
};
use x509_parser::prelude::FromDer;

/// Status of one serial as reported by a verified OCSP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcspStatus {
    Good,
    Revoked {
        reason: i32,
        revoked_at: OffsetDateTime,
    },
    /// The responder does not know this serial. Per RFC 6960 §2.2 that is not
    /// a statement that the certificate is fine.
    Unknown,
}

/// Build a single-serial `OCSPRequest` for `serial_bytes` issued by
/// `issuer_cert_pem`.
///
/// `CertID` identifies the issuer by SHA-1 hashes of its subject name and
/// public key. RFC 6960 pins those to SHA-1; no security decision rests on it,
/// they are only lookup keys.
pub fn build_request(issuer_cert_pem: &str, serial_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let cert_id = build_cert_id(issuer_cert_pem, serial_bytes)?;
    let request = OcspRequest {
        tbs_request: TbsRequest {
            version: Version::V1,
            requestor_name: None,
            request_list: vec![Request {
                req_cert: cert_id,
                single_request_extensions: None,
            }],
            request_extensions: None,
        },
        optional_signature: None,
    };
    request
        .to_der()
        .map_err(|e| format!("encoding OCSP request: {e}"))
}

/// `CertID` for `serial_bytes` under `issuer_cert_pem`.
pub fn build_cert_id(issuer_cert_pem: &str, serial_bytes: &[u8]) -> Result<CertId, String> {
    use spki::AlgorithmIdentifierOwned;

    let block = pem::parse(issuer_cert_pem).map_err(|e| format!("issuer PEM: {e}"))?;
    let (_, issuer) = x509_parser::certificate::X509Certificate::from_der(block.contents())
        .map_err(|e| format!("issuer X.509: {e}"))?;

    let name_hash = sha1_identifier(issuer.subject().as_raw());
    let key_hash = sha1_identifier(issuer.public_key().subject_public_key.data.as_ref());

    Ok(CertId {
        hash_algorithm: AlgorithmIdentifierOwned {
            oid: const_oid::db::rfc5912::ID_SHA_1,
            // SHA-1 takes explicit NULL parameters (RFC 3279 §2.2.1).
            parameters: Some(
                der::Any::from_der(&der::asn1::Null.to_der().map_err(|e| e.to_string())?)
                    .map_err(|e| format!("NULL parameters: {e}"))?,
            ),
        },
        issuer_name_hash: OctetString::new(name_hash)
            .map_err(|e| format!("issuerNameHash: {e}"))?,
        issuer_key_hash: OctetString::new(key_hash).map_err(|e| format!("issuerKeyHash: {e}"))?,
        serial_number: x509_cert::serial_number::SerialNumber::new(serial_bytes)
            .map_err(|e| format!("serialNumber: {e}"))?,
    })
}

/// Parse an OCSP response, verify its signature against one of `issuer_pems`,
/// and return the status of `expected_serial`.
///
/// An unverified response is discarded: without the signature check anyone who
/// can answer on the responder's address could mark a certificate `good`.
pub fn parse_and_verify_response(
    der: &[u8],
    issuer_pems: &[String],
    expected_serial_hex: &str,
) -> Result<OcspStatus, String> {
    let response =
        OcspResponse::from_der(der).map_err(|e| format!("malformed OCSP response: {e}"))?;
    if response.response_status != OcspResponseStatus::Successful {
        return Err(format!(
            "responder returned status {:?}",
            response.response_status
        ));
    }
    let bytes = response
        .response_bytes
        .ok_or("successful OCSP response carried no responseBytes")?;
    let basic = BasicOcspResponse::from_der(bytes.response.as_bytes())
        .map_err(|e| format!("malformed BasicOCSPResponse: {e}"))?;

    let tbs_der = basic
        .tbs_response_data
        .to_der()
        .map_err(|e| format!("re-encoding tbsResponseData: {e}"))?;
    let signature = basic
        .signature
        .as_bytes()
        .ok_or("OCSP signature is not a whole number of bytes")?;
    verify_signature(
        &tbs_der,
        signature,
        &basic.signature_algorithm.oid.to_string(),
        issuer_pems,
    )?;

    for single in &basic.tbs_response_data.responses {
        if crate::pki::hex_upper(single.cert_id.serial_number.as_bytes()) != expected_serial_hex {
            continue;
        }
        return Ok(match &single.cert_status {
            CertStatus::Good(_) => OcspStatus::Good,
            CertStatus::Unknown(_) => OcspStatus::Unknown,
            CertStatus::Revoked(info) => OcspStatus::Revoked {
                reason: info.revocation_reason.map(|r| r as i32).unwrap_or(0),
                revoked_at: OffsetDateTime::from_unix_timestamp(
                    info.revocation_time
                        .0
                        .to_unix_duration()
                        .as_secs()
                        .try_into()
                        .unwrap_or(0),
                )
                .unwrap_or_else(|_| OffsetDateTime::now_utc()),
            },
        });
    }
    Err(format!(
        "OCSP response does not cover serial {expected_serial_hex}"
    ))
}

/// Verify `signature` over `tbs` using the public key of whichever candidate
/// certificate validates it.
fn verify_signature(
    tbs: &[u8],
    signature: &[u8],
    algorithm_oid: &str,
    issuer_pems: &[String],
) -> Result<(), String> {
    // ecdsa-with-SHA256 / ecdsa-with-SHA384 / sha256WithRSAEncryption.
    let algorithm: &dyn aws_lc_rs::signature::VerificationAlgorithm = match algorithm_oid {
        "1.2.840.10045.4.3.2" => &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
        "1.2.840.10045.4.3.3" => &aws_lc_rs::signature::ECDSA_P384_SHA384_ASN1,
        "1.2.840.113549.1.1.11" => &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256,
        other => {
            return Err(format!(
                "OCSP response signed with unsupported algorithm {other}"
            ))
        }
    };

    let mut last_err = String::from("no candidate issuer certificate was supplied");
    for bundle in issuer_pems {
        for block in pem::parse_many(bundle).unwrap_or_default() {
            if block.tag() != "CERTIFICATE" {
                continue;
            }
            let Ok((_, candidate)) =
                x509_parser::certificate::X509Certificate::from_der(block.contents())
            else {
                continue;
            };
            // ECDSA verification wants the raw public key bits, which is what
            // subject_public_key holds; RSA wants the DER RSAPublicKey, which
            // is also the payload of that BIT STRING.
            let key_bytes = candidate.public_key().subject_public_key.data.as_ref();
            let key = aws_lc_rs::signature::UnparsedPublicKey::new(algorithm, key_bytes);
            match key.verify(tbs, signature) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = format!("{e}"),
            }
        }
    }
    Err(format!(
        "OCSP response signature did not verify: {last_err}"
    ))
}

fn sha1_identifier(data: &[u8]) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, data)
        .as_ref()
        .to_vec()
}

/// Serial hex string to the DER integer bytes a `CertID` carries.
pub fn serial_bytes(serial_hex: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = serial_hex
        .trim()
        .trim_start_matches("0x")
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if cleaned.is_empty() {
        return Err("serial is empty".to_string());
    }
    let padded = if cleaned.len() % 2 == 1 {
        format!("0{cleaned}")
    } else {
        cleaned
    };
    (0..padded.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&padded[i..i + 2], 16).map_err(|e| format!("bad serial hex: {e}"))
        })
        .collect()
}

/// POST an OCSP request to `base_url` and return the raw DER response.
///
/// Plain HTTP by design: the response is signed, so RFC 6960 §5 explicitly
/// does not require transport security, and requiring TLS here would mean a
/// node whose own certificate has just expired could not ask about anyone.
pub async fn post(base_url: &str, request_der: Vec<u8>) -> Result<Vec<u8>, String> {
    use http_body_util::BodyExt;
    use hyper::body::Bytes;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let url = format!("{}/pki/ocsp", base_url.trim_end_matches('/'));
    let client: Client<_, http_body_util::Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let request = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(&url)
        .header(hyper::header::CONTENT_TYPE, "application/ocsp-request")
        .body(http_body_util::Full::new(Bytes::from(request_der)))
        .map_err(|e| format!("building request to {url}: {e}"))?;

    let response = client
        .request(request)
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("POST {url} returned {}", response.status()));
    }
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("reading response from {url}: {e}"))?
        .to_bytes();
    Ok(body.to_vec())
}

/// Full point query: build, POST, verify, interpret.
pub async fn query(
    base_url: &str,
    issuer_pems: &[String],
    serial_hex: &str,
) -> Result<OcspStatus, String> {
    let issuer = issuer_pems
        .first()
        .ok_or("no issuer certificate available for an OCSP query")?;
    let serial = serial_bytes(serial_hex)?;
    let request_der = build_request(issuer, &serial)?;
    let response_der = post(base_url, request_der).await?;
    parse_and_verify_response(&response_der, issuer_pems, serial_hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::test_support::test_ca;

    #[test]
    fn serial_bytes_pads_odd_length_and_strips_decoration() {
        assert_eq!(serial_bytes("0A1B").expect("even"), vec![0x0a, 0x1b]);
        assert_eq!(serial_bytes("A1B").expect("odd"), vec![0x0a, 0x1b]);
        assert_eq!(
            serial_bytes("0x0a:1b").expect("decorated"),
            vec![0x0a, 0x1b]
        );
        assert!(serial_bytes("  ").is_err());
    }

    #[test]
    fn build_request_round_trips_through_the_responder_parser() {
        let (ca_cert, _ca_key) = test_ca("test-sub-ca");
        let der = build_request(&ca_cert, &[0x0a, 0x1b]).expect("build");

        // Decode it the way the controller's responder does.
        let parsed = OcspRequest::from_der(&der).expect("parse");
        assert_eq!(parsed.tbs_request.request_list.len(), 1);
        let cert_id = &parsed.tbs_request.request_list[0].req_cert;
        assert_eq!(cert_id.serial_number.as_bytes(), &[0x0a, 0x1b]);
        assert_eq!(cert_id.hash_algorithm.oid, const_oid::db::rfc5912::ID_SHA_1);
    }

    #[test]
    fn build_cert_id_binds_to_the_issuer_it_was_built_from() {
        let (ca_a, _) = test_ca("ca-a");
        let (ca_b, _) = test_ca("ca-b");
        let id_a = build_cert_id(&ca_a, &[0x01]).expect("a");
        let id_b = build_cert_id(&ca_b, &[0x01]).expect("b");
        assert_ne!(
            id_a.issuer_key_hash, id_b.issuer_key_hash,
            "different CAs must produce different issuerKeyHash"
        );
        assert_ne!(id_a.issuer_name_hash, id_b.issuer_name_hash);
    }

    #[test]
    fn build_cert_id_rejects_non_pem_issuers() {
        assert!(build_cert_id("not a pem", &[0x01]).is_err());
    }

    #[test]
    fn parse_and_verify_response_rejects_garbage() {
        let (ca_cert, _) = test_ca("test-sub-ca");
        assert!(parse_and_verify_response(&[0, 1, 2, 3], &[ca_cert], "0A").is_err());
    }

    #[test]
    fn verify_signature_rejects_unsupported_algorithms() {
        let (ca_cert, _) = test_ca("test-sub-ca");
        let err = verify_signature(b"tbs", b"sig", "1.3.101.112", &[ca_cert])
            .expect_err("Ed25519 is not in the accepted set");
        assert!(err.contains("unsupported algorithm"), "{err}");
    }

    #[test]
    fn verify_signature_rejects_a_forged_signature() {
        let (ca_cert, _) = test_ca("test-sub-ca");
        let err = verify_signature(
            b"tbs",
            b"not a signature",
            "1.2.840.10045.4.3.2",
            &[ca_cert],
        )
        .expect_err("garbage signature must not verify");
        assert!(err.contains("did not verify"), "{err}");
    }

    #[tokio::test]
    async fn post_fails_cleanly_against_a_dead_responder() {
        let err = post("http://127.0.0.1:1", vec![0x30, 0x00])
            .await
            .expect_err("nothing listens on port 1");
        assert!(err.contains("POST"), "{err}");
    }

    #[tokio::test]
    async fn query_requires_an_issuer() {
        let err = query("http://127.0.0.1:1", &[], "0A")
            .await
            .expect_err("no issuer, no query");
        assert!(err.contains("no issuer certificate"), "{err}");
    }
}
