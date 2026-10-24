//! The web listener: gRPC-web and gRPC with bearer tokens, plus the static
//! dashboard. It never asks for a client certificate.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Context as _;
use http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tonic::body::BoxBody;
use tonic::service::Routes;
use tonic::Status;
use tower::{Layer, Service, ServiceExt};
use tower_http::services::{ServeDir, ServeFile};
use tracing::{debug, info, warn};

use crate::auth::{OperatorRole, WebIdentity, WebPeer, WebTokenKind};
use crate::db::Database;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type WebBody = UnsyncBoxBody<Bytes, BoxError>;

const PUBLIC: &[&str] = &["/kcore.controller.Auth/Login"];
const SECOND_FACTOR: &[&str] = &[
    "/kcore.controller.Auth/VerifySecondFactor",
    "/kcore.controller.Auth/BeginTotpEnrollment",
    "/kcore.controller.Auth/ConfirmTotpEnrollment",
];
const PENDING_ALLOWED: &[&str] = &[
    "/kcore.controller.Auth/VerifySecondFactor",
    "/kcore.controller.Auth/BeginTotpEnrollment",
    "/kcore.controller.Auth/ConfirmTotpEnrollment",
    "/kcore.controller.Auth/Logout",
];
/// Writing `last_used_at` on every RPC would serialize the dashboard's polling
/// on the SQLite lock for no gain.
const TOUCH_EVERY_SECS: i64 = 15;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; font-src 'self' data:; connect-src 'self'; object-src 'none'; \
base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, PartialEq, Eq)]
pub enum GateDecision {
    Public,
    Identity(WebIdentity),
    Deny(tonic::Code, String),
}

/// Bearer-token check run before any service on the web listener.
#[derive(Clone)]
pub struct TokenGate {
    db: Database,
    idle_secs: i64,
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
        .filter(|t| !t.is_empty())
}

impl TokenGate {
    pub fn new(db: Database, idle_secs: i64) -> Self {
        Self { db, idle_secs }
    }

    pub fn check(&self, path: &str, headers: &HeaderMap, now: i64) -> GateDecision {
        use tonic::Code;
        let deny = |code: Code, msg: &str| GateDecision::Deny(code, msg.to_string());
        if PUBLIC.contains(&path) {
            return GateDecision::Public;
        }
        let Some(token) = bearer(headers) else {
            return deny(Code::Unauthenticated, "sign in first");
        };
        let hash = super::secrets::sha256_hex(token.as_bytes());
        let row = match self.db.find_auth_token_by_hash(&hash) {
            Ok(Some(row)) => row,
            Ok(None) => return deny(Code::Unauthenticated, "session ended; sign in again"),
            Err(e) => return GateDecision::Deny(Code::Internal, format!("auth store: {e}")),
        };
        let Some(kind) = WebTokenKind::from_db_str(&row.kind) else {
            return deny(Code::Unauthenticated, "unknown token kind");
        };
        if row.expires_at <= now {
            let _ = self.db.delete_auth_token(&row.id);
            return deny(Code::Unauthenticated, "session expired; sign in again");
        }
        if kind == WebTokenKind::Session && now - row.last_used_at > self.idle_secs {
            let _ = self.db.delete_auth_token(&row.id);
            return deny(Code::Unauthenticated, "session timed out; sign in again");
        }
        match kind {
            WebTokenKind::Pending if !PENDING_ALLOWED.contains(&path) => {
                return deny(
                    Code::PermissionDenied,
                    "finish the second sign-in step first",
                )
            }
            WebTokenKind::Api if SECOND_FACTOR.contains(&path) => {
                return deny(Code::PermissionDenied, "an API token cannot change TOTP")
            }
            _ => {}
        }
        if kind != WebTokenKind::Pending && now - row.last_used_at >= TOUCH_EVERY_SECS {
            let _ = self.db.touch_auth_token(&row.id, now);
        }
        GateDecision::Identity(WebIdentity {
            operator: row.operator_name,
            token_id: row.id,
            kind,
            max_role: OperatorRole::from_db_str(&row.max_role),
            expires_at: row.expires_at,
        })
    }
}

/// Runs the token gate, then the controller services.
#[derive(Clone)]
pub struct Gated {
    routes: Routes,
    gate: TokenGate,
}

impl Gated {
    pub fn new(routes: Routes, gate: TokenGate) -> Self {
        Self { routes, gate }
    }
}

impl Service<Request<BoxBody>> for Gated {
    type Response = Response<BoxBody>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.routes.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<BoxBody>) -> Self::Future {
        let path = req.uri().path().to_string();
        match self.gate.check(&path, req.headers(), now()) {
            GateDecision::Deny(code, msg) => {
                debug!(%path, %msg, "web listener refused a call");
                return Box::pin(async move { Ok(Status::new(code, msg).into_http()) });
            }
            GateDecision::Identity(identity) => {
                req.extensions_mut().insert(identity);
            }
            GateDecision::Public => {}
        }
        let fut = self.routes.call(req);
        Box::pin(fut)
    }
}

type GrpcStack = tonic_web::GrpcWebService<Gated>;

#[derive(Clone)]
struct State {
    grpc: GrpcStack,
    ui_dir: Option<PathBuf>,
    tls: bool,
}

pub struct WebServer {
    addr: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    state: State,
}

/// TLS for the web listener: the controller's server certificate, no client
/// certificate, HTTP/2 and HTTP/1.1 offered.
pub fn tls_acceptor(cert_file: &str, key_file: &str) -> anyhow::Result<TlsAcceptor> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_file_iter(cert_file)
        .with_context(|| format!("read {cert_file}"))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parse {cert_file}"))?;
    let key = PrivateKeyDer::from_pem_file(key_file).with_context(|| format!("read {key_file}"))?;
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("web listener TLS config")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

impl WebServer {
    pub fn new(
        addr: SocketAddr,
        acceptor: Option<TlsAcceptor>,
        routes: Routes,
        gate: TokenGate,
        ui_dir: Option<PathBuf>,
    ) -> Self {
        let grpc = tonic_web::GrpcWebLayer::new().layer(Gated::new(routes, gate));
        let tls = acceptor.is_some();
        Self {
            addr,
            acceptor,
            state: State { grpc, ui_dir, tls },
        }
    }

    pub async fn run(self, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
        let listener = TcpListener::bind(self.addr)
            .await
            .with_context(|| format!("bind web listener {}", self.addr))?;
        self.serve(listener, shutdown).await
    }

    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> anyhow::Result<()> {
        if self.acceptor.is_some() {
            info!(addr = %self.addr, "web listener (gRPC-web, TLS) started");
        } else {
            warn!(addr = %self.addr, "web listener started WITHOUT TLS (--allow-insecure); passwords and tokens travel in clear text");
        }
        let mut conns = tokio::task::JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => {
                    let (stream, remote) = match accepted {
                        Ok(pair) => pair,
                        Err(e) => {
                            warn!(error = %e, "web listener accept failed");
                            continue;
                        }
                    };
                    let state = self.state.clone();
                    let acceptor = self.acceptor.clone();
                    conns.spawn(async move {
                        match acceptor {
                            Some(acceptor) => {
                                match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                                    Ok(Ok(tls)) => serve_conn(tls, remote, state).await,
                                    Ok(Err(e)) => debug!(%remote, error = %e, "web TLS handshake failed"),
                                    Err(_) => debug!(%remote, "web TLS handshake timed out"),
                                }
                            }
                            None => serve_conn(stream, remote, state).await,
                        }
                    });
                }
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
        conns.abort_all();
        Ok(())
    }
}

async fn serve_conn<IO>(io: IO, remote: SocketAddr, state: State)
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req| handle(req, remote, state.clone()));
    if let Err(e) = auto::Builder::new(TokioExecutor::new())
        .serve_connection(TokioIo::new(io), service)
        .await
    {
        debug!(%remote, error = %e, "web connection closed with an error");
    }
}

fn is_grpc(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/grpc"))
}

fn plain(status: StatusCode, text: &'static str) -> Response<WebBody> {
    let mut resp = Response::new(
        Full::new(Bytes::from_static(text.as_bytes()))
            .map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

async fn handle(
    req: Request<Incoming>,
    remote: SocketAddr,
    state: State,
) -> Result<Response<WebBody>, Infallible> {
    if is_grpc(&req) {
        let mut req = req.map(tonic::body::boxed);
        req.extensions_mut().insert(WebPeer {
            remote_addr: remote,
        });
        return Ok(match state.grpc.clone().oneshot(req).await {
            Ok(resp) => resp.map(|body| body.map_err(BoxError::from).boxed_unsync()),
            Err(e) => {
                warn!(error = %e, "web gRPC service failed");
                plain(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        });
    }
    let Some(dir) = state.ui_dir.clone() else {
        return Ok(plain(
            StatusCode::NOT_FOUND,
            "kcore controller web listener: no dashboard is installed",
        ));
    };
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Ok(plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"));
    }
    let immutable = req.uri().path().starts_with("/assets/");
    let index = dir.join("index.html");
    let serve = ServeDir::new(&dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(index));
    let mut resp = match serve.oneshot(req).await {
        Ok(resp) => resp.map(|body| body.map_err(BoxError::from).boxed_unsync()),
        Err(never) => match never {},
    };
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    if state.tls {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::AuthTokenRow;

    fn gate_with(kind: &str, last_used: i64, max_role: &str) -> (TokenGate, String) {
        let db = Database::open(":memory:").unwrap();
        db.create_operator("alice").unwrap();
        let (token, hash) = super::super::secrets::new_token("kcs_");
        db.insert_auth_token(&AuthTokenRow {
            id: "t1".into(),
            token_hash: hash,
            operator_name: "alice".into(),
            kind: kind.into(),
            name: String::new(),
            max_role: max_role.into(),
            created_at: 1000,
            expires_at: 5000,
            last_used_at: last_used,
            created_from: String::new(),
        })
        .unwrap();
        (TokenGate::new(db, 600), token)
    }

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    const LIST_VMS: &str = "/kcore.controller.Controller/ListVms";

    #[test]
    fn login_is_public_and_everything_else_needs_a_token() {
        let (gate, _) = gate_with("session", 1000, "");
        assert_eq!(
            gate.check("/kcore.controller.Auth/Login", &HeaderMap::new(), 1000),
            GateDecision::Public
        );
        assert!(matches!(
            gate.check(LIST_VMS, &HeaderMap::new(), 1000),
            GateDecision::Deny(tonic::Code::Unauthenticated, _)
        ));
        assert!(matches!(
            gate.check(LIST_VMS, &headers("kcs_wrong"), 1000),
            GateDecision::Deny(tonic::Code::Unauthenticated, _)
        ));
    }

    #[test]
    fn session_maps_to_the_operator() {
        let (gate, token) = gate_with("session", 1000, "");
        let GateDecision::Identity(id) = gate.check(LIST_VMS, &headers(&token), 1100) else {
            panic!("expected identity");
        };
        assert_eq!(id.operator, "alice");
        assert_eq!(id.kind, WebTokenKind::Session);
        assert_eq!(id.max_role, None);
    }

    #[test]
    fn idle_and_expired_sessions_are_dropped() {
        let (gate, token) = gate_with("session", 1000, "");
        assert!(matches!(
            gate.check(LIST_VMS, &headers(&token), 1601),
            GateDecision::Deny(tonic::Code::Unauthenticated, _)
        ));
        let (gate, token) = gate_with("api", 1000, "");
        assert!(matches!(
            gate.check(LIST_VMS, &headers(&token), 4000),
            GateDecision::Identity(_)
        ));
        assert!(matches!(
            gate.check(LIST_VMS, &headers(&token), 5000),
            GateDecision::Deny(tonic::Code::Unauthenticated, _)
        ));
        assert!(matches!(
            gate.check(LIST_VMS, &headers(&token), 4000),
            GateDecision::Deny(tonic::Code::Unauthenticated, _)
        ));
    }

    #[test]
    fn pending_token_only_reaches_the_second_factor() {
        let (gate, token) = gate_with("pending", 1000, "");
        assert!(matches!(
            gate.check(LIST_VMS, &headers(&token), 1100),
            GateDecision::Deny(tonic::Code::PermissionDenied, _)
        ));
        assert!(matches!(
            gate.check(
                "/kcore.controller.Auth/VerifySecondFactor",
                &headers(&token),
                1100
            ),
            GateDecision::Identity(_)
        ));
        assert!(matches!(
            gate.check("/kcore.controller.Auth/WhoAmI", &headers(&token), 1100),
            GateDecision::Deny(tonic::Code::PermissionDenied, _)
        ));
    }

    #[test]
    fn api_token_carries_its_cap_and_cannot_touch_totp() {
        let (gate, token) = gate_with("api", 1000, "read-only");
        let GateDecision::Identity(id) = gate.check(LIST_VMS, &headers(&token), 1100) else {
            panic!("expected identity");
        };
        assert_eq!(id.max_role, Some(OperatorRole::ReadOnly));
        assert!(matches!(
            gate.check(
                "/kcore.controller.Auth/BeginTotpEnrollment",
                &headers(&token),
                1100
            ),
            GateDecision::Deny(tonic::Code::PermissionDenied, _)
        ));
    }

    async fn http(addr: SocketAddr, request: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(&request).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).to_ascii_lowercase()
    }

    fn grpc_web(path: &str, token: Option<&str>, message: &impl prost::Message) -> Vec<u8> {
        let payload = message.encode_to_vec();
        let mut body = vec![0u8];
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(&payload);
        let auth = token
            .map(|t| format!("authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let mut req = format!(
            "POST {path} HTTP/1.1\r\nhost: test\r\nconnection: close\r\n\
             content-type: application/grpc-web+proto\r\nx-grpc-web: 1\r\n{auth}\
             content-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        req.extend_from_slice(&body);
        req
    }

    #[tokio::test]
    async fn web_listener_serves_grpc_web_and_the_dashboard() {
        use crate::controller_proto as pb;
        use std::sync::Mutex;

        let db = Database::open(":memory:").unwrap();
        let controller = crate::grpc::ControllerService::new(
            db.clone(),
            crate::node_client::NodeClients::new(None),
            crate::config::NetworkConfig {
                gateway_interface: "eno1".into(),
                external_ip: "203.0.113.10".into(),
                gateway_ip: "10.0.0.1".into(),
                internal_netmask: "255.255.255.0".into(),
            },
            Arc::new(Mutex::new(crate::grpc::SubCaState::default())),
            None,
            false,
            false,
        );
        let auth = super::super::AuthService::new(
            db.clone(),
            controller.clone(),
            crate::config::WebConfig::default(),
            false,
            false,
        );
        let routes = Routes::new(pb::controller_server::ControllerServer::new(controller))
            .add_service(pb::auth_server::AuthServer::new(auth));
        let ui = tempfile::tempdir().unwrap();
        std::fs::write(ui.path().join("index.html"), "<title>kcore</title>").unwrap();
        let server = WebServer::new(
            "127.0.0.1:0".parse().unwrap(),
            None,
            routes,
            TokenGate::new(db.clone(), 600),
            Some(ui.path().to_path_buf()),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.serve(listener, async {
            let _ = stop_rx.await;
        }));

        let page = http(
            addr,
            b"GET /vms/web-01 HTTP/1.1\r\nhost: test\r\nconnection: close\r\n\r\n".to_vec(),
        )
        .await;
        assert!(page.starts_with("http/1.1 200"), "{page}");
        assert!(
            page.contains("<title>kcore</title>"),
            "SPA fallback: {page}"
        );
        assert!(page.contains("content-security-policy: default-src 'self'"));

        let no_token = http(
            addr,
            grpc_web(
                "/kcore.controller.Controller/ListVms",
                None,
                &pb::ListVmsRequest::default(),
            ),
        )
        .await;
        assert!(no_token.contains("grpc-status: 16"), "{no_token}");

        let bad_login = http(
            addr,
            grpc_web(
                "/kcore.controller.Auth/Login",
                None,
                &pb::LoginRequest {
                    username: "nobody".into(),
                    password: "whatever-password".into(),
                },
            ),
        )
        .await;
        assert!(bad_login.contains("grpc-status: 16"), "{bad_login}");
        assert!(
            bad_login.contains("invalid%20username%20or%20password"),
            "{bad_login}"
        );

        db.create_operator("alice").unwrap();
        db.grant_operator_role_str("alice", "read-only").unwrap();
        let (token, hash) = super::super::secrets::new_token("kcs_");
        db.insert_auth_token(&AuthTokenRow {
            id: "s1".into(),
            token_hash: hash,
            operator_name: "alice".into(),
            kind: "session".into(),
            name: String::new(),
            max_role: String::new(),
            created_at: now(),
            expires_at: now() + 600,
            last_used_at: now(),
            created_from: String::new(),
        })
        .unwrap();
        let listed = http(
            addr,
            grpc_web(
                "/kcore.controller.Controller/ListVms",
                Some(&token),
                &pb::ListVmsRequest::default(),
            ),
        )
        .await;
        assert!(listed.contains("grpc-status:0"), "{listed}");

        let _ = stop_tx.send(());
        task.await.unwrap().unwrap();
    }

    #[test]
    fn bearer_parsing() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer  abc "),
        );
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer(&h), None);
    }
}
