//! Plain-HTTP PKI distribution endpoints.
//!
//! CRLs and OCSP responses are signed objects, so they are served over plain
//! HTTP on a dedicated port rather than over the mTLS gRPC listener. That is
//! deliberate and matches every public CA: a client that cannot yet trust its
//! own certificate must still be able to fetch revocation data, and requiring
//! mTLS to learn "your certificate is revoked" is circular.
//!
//! | Method | Path             | Content-Type            |
//! |--------|------------------|-------------------------|
//! | GET    | `/pki/crl.der`   | `application/pkix-crl`  |
//! | GET    | `/pki/crl.pem`   | `application/x-pem-file`|
//! | POST   | `/pki/ocsp`      | `application/ocsp-response` |
//! | GET    | `/pki/ocsp/{b64}`| `application/ocsp-response` |
//! | GET    | `/pki/healthz`   | `text/plain`            |

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use time::Duration;
use tracing::{info, warn};

use crate::db::Database;
use crate::grpc::SubCaState;
use crate::pki::crl::CrlCache;
use crate::pki::ocsp;

const CONTENT_TYPE_CRL: &str = "application/pkix-crl";
const CONTENT_TYPE_PEM: &str = "application/x-pem-file";
const CONTENT_TYPE_OCSP_RESPONSE: &str = "application/ocsp-response";
/// Refuse oversized OCSP requests outright; a legitimate one is a few hundred
/// bytes even when batched.
const MAX_OCSP_REQUEST_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct PkiHttpState {
    pub db: Database,
    pub sub_ca: Arc<Mutex<SubCaState>>,
    pub crl_cache: CrlCache,
    pub ocsp_validity: Duration,
}

pub fn router(state: PkiHttpState) -> Router {
    Router::new()
        .route("/pki/crl.der", get(crl_der))
        .route("/pki/crl.pem", get(crl_pem))
        .route("/pki/ocsp", post(ocsp_post))
        .route("/pki/ocsp/{encoded}", get(ocsp_get))
        .route("/pki/healthz", get(healthz))
        .with_state(state)
}

/// Serve the PKI endpoints until the process exits. Bind failures are logged
/// and the task returns: the controller must keep serving gRPC even if the
/// PKI port is unavailable.
pub async fn serve(addr: SocketAddr, state: PkiHttpState) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(error) => {
            warn!(%addr, %error, "failed to bind PKI HTTP listener; CRL/OCSP endpoints are unavailable");
            return;
        }
    };
    info!(%addr, "serving CRL and OCSP over HTTP at /pki");
    if let Err(error) = axum::serve(listener, router(state)).await {
        warn!(%error, "PKI HTTP server stopped");
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn crl_der(State(state): State<PkiHttpState>) -> Response {
    match state.crl_cache.get() {
        Some(crl) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, CONTENT_TYPE_CRL),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            crl.der,
        )
            .into_response(),
        None => crl_unavailable(),
    }
}

async fn crl_pem(State(state): State<PkiHttpState>) -> Response {
    match state.crl_cache.get() {
        Some(crl) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, CONTENT_TYPE_PEM),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            crl.pem,
        )
            .into_response(),
        None => crl_unavailable(),
    }
}

fn crl_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "no CRL has been generated yet (is a sub-CA configured?)\n",
    )
        .into_response()
}

async fn ocsp_post(State(state): State<PkiHttpState>, body: Bytes) -> Response {
    ocsp_answer(&state, &body)
}

/// RFC 6960 §A.1 GET form: base64 of the DER request in the path. Clients
/// commonly percent-encode `+` and `/`, so both standard and URL-safe
/// alphabets are accepted.
async fn ocsp_get(State(state): State<PkiHttpState>, Path(encoded): Path<String>) -> Response {
    let Some(der) = decode_ocsp_path(&encoded) else {
        return ocsp_response(StatusCode::BAD_REQUEST, ocsp::malformed_request_der());
    };
    ocsp_answer(&state, &der)
}

/// Decode the base64 path segment of an OCSP GET request.
pub fn decode_ocsp_path(encoded: &str) -> Option<Vec<u8>> {
    let trimmed = encoded.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_OCSP_REQUEST_BYTES * 2 {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(trimmed))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed))
        .ok()
}

fn ocsp_answer(state: &PkiHttpState, der: &[u8]) -> Response {
    if der.is_empty() || der.len() > MAX_OCSP_REQUEST_BYTES {
        return ocsp_response(StatusCode::BAD_REQUEST, ocsp::malformed_request_der());
    }
    let cert_ids = match ocsp::parse_request(der) {
        Ok(ids) => ids,
        Err(error) => {
            warn!(%error, "rejecting malformed OCSP request");
            return ocsp_response(StatusCode::BAD_REQUEST, ocsp::malformed_request_der());
        }
    };
    let sub_ca = match state.sub_ca.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => return ocsp_response(StatusCode::OK, ocsp::internal_error_der()),
    };
    if !sub_ca.is_available() {
        // No signing key: `tryLater` tells the client to retry rather than
        // treating the certificate as unknown.
        return ocsp_response(StatusCode::OK, ocsp::try_later_der());
    }
    match ocsp::build_response(&state.db, &sub_ca, &cert_ids, state.ocsp_validity) {
        Ok(body) => ocsp_response(StatusCode::OK, body),
        Err(error) => {
            warn!(%error, "failed to build OCSP response");
            ocsp_response(StatusCode::OK, ocsp::internal_error_der())
        }
    }
}

fn ocsp_response(status: StatusCode, body: Vec<u8>) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, CONTENT_TYPE_OCSP_RESPONSE),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::signing::sign_node_cert;
    use crate::grpc::signing::test_support::{generate_test_ca, generate_test_sub_ca};
    use crate::pki::{format_ts, inventory};
    use der::{Decode, Encode};
    use time::OffsetDateTime;
    use x509_ocsp::{BasicOcspResponse, CertStatus, OcspResponse, OcspResponseStatus};

    fn state_with_sub_ca() -> (PkiHttpState, SubCaState) {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let sub = SubCaState {
            cert_pem: sub_cert,
            key_pem: sub_key,
            cert_file: None,
            key_file: None,
        };
        let state = PkiHttpState {
            db: Database::open(":memory:").expect("db"),
            sub_ca: Arc::new(Mutex::new(sub.clone())),
            crl_cache: CrlCache::new(),
            ocsp_validity: Duration::hours(1),
        };
        (state, sub)
    }

    /// Drive one request through the router the way a real client would.
    async fn call(
        state: PkiHttpState,
        request: axum::http::Request<axum::body::Body>,
    ) -> (StatusCode, Vec<u8>, String) {
        use tower::ServiceExt;
        let response = router(state)
            .oneshot(request)
            .await
            .expect("router response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body")
            .to_vec();
        (status, body, content_type)
    }

    fn ocsp_request_der(sub_ca_cert_pem: &str, serial_hex: &str) -> Vec<u8> {
        let cert_id = ocsp::build_cert_id(sub_ca_cert_pem, serial_hex).expect("cert id");
        x509_ocsp::OcspRequest {
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
        }
        .to_der()
        .expect("der")
    }

    fn first_status(body: &[u8]) -> CertStatus {
        let response = OcspResponse::from_der(body).expect("decode");
        assert_eq!(response.response_status, OcspResponseStatus::Successful);
        let bytes = response.response_bytes.expect("responseBytes");
        BasicOcspResponse::from_der(bytes.response.as_bytes())
            .expect("basic")
            .tbs_response_data
            .responses
            .first()
            .expect("single response")
            .cert_status
    }

    #[tokio::test]
    async fn healthz_reports_ok() {
        let (state, _) = state_with_sub_ca();
        let (status, body, _) = call(
            state,
            axum::http::Request::builder()
                .uri("/pki/healthz")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"ok");
    }

    #[tokio::test]
    async fn crl_endpoints_return_503_before_a_crl_exists() {
        let (state, _) = state_with_sub_ca();
        for path in ["/pki/crl.der", "/pki/crl.pem"] {
            let (status, _, _) = call(
                state.clone(),
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "path {path}");
        }
    }

    #[tokio::test]
    async fn crl_endpoints_serve_der_and_pem_with_pkix_content_types() {
        let (state, sub) = state_with_sub_ca();
        crate::pki::crl::ensure_current(
            &state.db,
            &sub,
            &state.crl_cache,
            Duration::hours(24),
            Duration::hours(1),
            false,
        )
        .expect("ensure crl")
        .expect("crl");

        let (status, der, content_type) = call(
            state.clone(),
            axum::http::Request::builder()
                .uri("/pki/crl.der")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, CONTENT_TYPE_CRL);
        use x509_parser::prelude::FromDer;
        x509_parser::revocation_list::CertificateRevocationList::from_der(&der)
            .expect("served bytes must be a parseable CRL");

        let (status, pem_body, content_type) = call(
            state,
            axum::http::Request::builder()
                .uri("/pki/crl.pem")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, CONTENT_TYPE_PEM);
        assert!(String::from_utf8_lossy(&pem_body).contains("BEGIN X509 CRL"));
    }

    #[tokio::test]
    async fn ocsp_post_answers_good_and_revoked() {
        let (state, sub) = state_with_sub_ca();
        let (chain, _) = sign_node_cert(&sub.cert_pem, &sub.key_pem, "10.0.3.1").expect("sign");
        let good = inventory::record_signed_chain(&state.db, &chain, "node-a").expect("record");
        let (chain2, _) = sign_node_cert(&sub.cert_pem, &sub.key_pem, "10.0.3.2").expect("sign");
        let bad = inventory::record_signed_chain(&state.db, &chain2, "node-b").expect("record");
        state
            .db
            .revoke_certificate_by_serial(&bad.serial_hex, 1, &format_ts(OffsetDateTime::now_utc()))
            .expect("revoke");

        for (serial, expect_good) in [(&good.serial_hex, true), (&bad.serial_hex, false)] {
            let (status, body, content_type) = call(
                state.clone(),
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/pki/ocsp")
                    .header(header::CONTENT_TYPE, "application/ocsp-request")
                    .body(axum::body::Body::from(ocsp_request_der(
                        &sub.cert_pem,
                        serial,
                    )))
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(content_type, CONTENT_TYPE_OCSP_RESPONSE);
            match first_status(&body) {
                CertStatus::Good(_) => assert!(expect_good, "{serial} should not be good"),
                CertStatus::Revoked(_) => assert!(!expect_good, "{serial} should not be revoked"),
                other => panic!("unexpected status {other:?} for {serial}"),
            }
        }
    }

    #[tokio::test]
    async fn ocsp_get_accepts_base64_in_the_path() {
        let (state, sub) = state_with_sub_ca();
        let der = ocsp_request_der(&sub.cert_pem, "AABBCC");
        let encoded = base64::engine::general_purpose::URL_SAFE.encode(&der);
        let (status, body, _) = call(
            state,
            axum::http::Request::builder()
                .uri(format!("/pki/ocsp/{encoded}"))
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(matches!(first_status(&body), CertStatus::Unknown(_)));
    }

    #[tokio::test]
    async fn ocsp_rejects_garbage_with_malformed_request_status() {
        let (state, _) = state_with_sub_ca();
        let (status, body, _) = call(
            state.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/pki/ocsp")
                .body(axum::body::Body::from(vec![0xff, 0xfe, 0xfd]))
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let response = OcspResponse::from_der(&body).expect("decode");
        assert_eq!(
            response.response_status,
            OcspResponseStatus::MalformedRequest
        );

        let (status, _, _) = call(
            state,
            axum::http::Request::builder()
                .uri("/pki/ocsp/not-base64-!!!")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ocsp_answers_try_later_without_a_sub_ca() {
        let (mut state, sub) = state_with_sub_ca();
        let der = ocsp_request_der(&sub.cert_pem, "01");
        state.sub_ca = Arc::new(Mutex::new(SubCaState::default()));
        let (status, body, _) = call(
            state,
            axum::http::Request::builder()
                .method("POST")
                .uri("/pki/ocsp")
                .body(axum::body::Body::from(der))
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let response = OcspResponse::from_der(&body).expect("decode");
        assert_eq!(response.response_status, OcspResponseStatus::TryLater);
    }

    #[test]
    fn decode_ocsp_path_accepts_both_base64_alphabets() {
        let raw: Vec<u8> = (0u8..=255).collect();
        for encoded in [
            base64::engine::general_purpose::STANDARD.encode(&raw),
            base64::engine::general_purpose::URL_SAFE.encode(&raw),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw),
        ] {
            assert_eq!(decode_ocsp_path(&encoded).as_deref(), Some(raw.as_slice()));
        }
        assert_eq!(decode_ocsp_path(""), None);
        assert_eq!(decode_ocsp_path("   "), None);
    }
}
