//! The loopback HTTP server: the embedded UI, a token-guarded JSON API, and one SSE stream per session.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use axum::Router;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{json, Map, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use super::auth::AuthStore;
use super::chatgpt::CALLBACK_PATH;
use super::clusters::Clusters;
use super::session::{Live, Session};
use super::transcript::{list_transcripts, read_transcript};
use super::util::{constant_time_eq, random_bytes};

const INDEX_HTML: &str = include_str!("../../assets/agent/index.html");
const APP_JS: &str = include_str!("../../assets/agent/app.js");
const STYLE_CSS: &str = include_str!("../../assets/agent/style.css");

const MAX_BODY: usize = 1_000_000;
const MAX_APP_FRAMES: usize = 200;
const APP_FRAME_TTL: Duration = Duration::from_secs(60 * 60);
const KEEP_ALIVE: Duration = Duration::from_secs(15);

const PAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
// An app view gets script and style of its own and nothing else: no network, no frames, no storage.
// `sandbox` here also makes it an opaque origin if someone opens the URL directly.
const APP_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'self'; sandbox allow-scripts";

static APP_FRAME_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/apps/([0-9a-f]{48})$").unwrap());
static PROVIDER_ROUTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/api/providers/([a-z0-9-]+)/(models|key)$").unwrap());
static SESSION_ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^/api/sessions/([0-9a-f-]{36})(?:/(messages|events|cancel|apps|call|forms/f\d+))?$",
    )
    .unwrap()
});

pub struct AppOptions {
    pub token: String,
    pub auth: Arc<AuthStore>,
    pub clusters: Arc<Clusters>,
    pub log_dir: Option<PathBuf>,
}

pub struct AppState {
    token: String,
    auth: Arc<AuthStore>,
    clusters: Arc<Clusters>,
    log_dir: Option<PathBuf>,
    port: u16,
    pub sessions: Mutex<HashMap<String, Arc<Session>>>,
    frames: Mutex<HashMap<String, (String, Instant)>>,
}

pub struct Running {
    pub port: u16,
    pub state: Arc<AppState>,
    stop: CancellationToken,
    done: tokio::task::JoinHandle<()>,
}

impl Running {
    pub async fn close(self) {
        let sessions: Vec<Arc<Session>> = self
            .state
            .sessions
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for session in sessions {
            session.cancel();
        }
        self.stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.done).await;
    }
}

/// Binds 127.0.0.1 only. There is no option to listen anywhere else.
pub async fn start(options: AppOptions, port: u16) -> Result<Running> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .with_context(|| format!("listening on 127.0.0.1:{port}"))?;
    let port = listener.local_addr()?.port();

    let mut sessions = HashMap::new();
    if let Some(dir) = &options.log_dir {
        for id in list_transcripts(dir) {
            if let Some(restore) = read_transcript(dir, &id) {
                let cluster = restore.cluster.clone();
                let toolbox = options.clusters.toolbox(cluster.as_deref()).await;
                let session = Session::new(
                    id.clone(),
                    cluster,
                    toolbox,
                    Some(dir.clone()),
                    Some(restore),
                );
                sessions.insert(id, session);
            }
        }
    }
    let state = Arc::new(AppState {
        token: options.token,
        auth: options.auth,
        clusters: options.clusters,
        log_dir: options.log_dir,
        port,
        sessions: Mutex::new(sessions),
        frames: Mutex::new(HashMap::new()),
    });

    let app = Router::new().fallback(handle).with_state(state.clone());
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let done = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
    });
    Ok(Running {
        port,
        state,
        stop,
        done,
    })
}

fn reply(status: u16, content_type: &str, body: impl Into<Body>) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
    response
}

fn json_reply(status: u16, body: &Value) -> Response {
    reply(
        status,
        "application/json; charset=utf-8",
        serde_json::to_vec(body).unwrap_or_default(),
    )
}

fn error_reply(status: u16, message: impl std::fmt::Display) -> Response {
    json_reply(status, &json!({ "error": message.to_string() }))
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn callback_page(ok: bool, message: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>kcore agent</title><body style=\"font:16px system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem\"><h1>{}</h1><p>{}</p>",
        if ok { "Signed in" } else { "Not signed in" },
        escape_html(message)
    )
}

async fn handle(State(app): State<Arc<AppState>>, request: Request) -> Response {
    let framed = APP_FRAME_PATH.is_match(request.uri().path());
    let mut response = route(&app, request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("content-security-policy", PAGE_CSP),
    ] {
        headers
            .entry(HeaderName::from_static(name))
            .or_insert(HeaderValue::from_static(value));
    }
    if !framed {
        headers
            .entry(HeaderName::from_static("x-frame-options"))
            .or_insert(HeaderValue::from_static("DENY"));
    }
    response
}

fn host_allowed(app: &AppState, request: &Request) -> bool {
    let host = request
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    host == format!("127.0.0.1:{}", app.port) || host == format!("localhost:{}", app.port)
}

fn origin_allowed(app: &AppState, request: &Request) -> bool {
    match request.headers().get(ORIGIN) {
        None => true,
        Some(origin) => {
            let origin = origin.to_str().unwrap_or("");
            origin == format!("http://127.0.0.1:{}", app.port)
                || origin == format!("http://localhost:{}", app.port)
        }
    }
}

fn authorized(app: &AppState, request: &Request) -> bool {
    request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| constant_time_eq(given.as_bytes(), app.token.as_bytes()))
}

async fn read_json(body: Body) -> Result<Map<String, Value>, Response> {
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| error_reply(413, "request body too large"))?;
    if bytes.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Ok(Map::new()),
        Err(error) => Err(error_reply(400, format!("invalid JSON: {error}"))),
    }
}

fn query_param(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

fn str_field<'a>(body: &'a Map<String, Value>, name: &str) -> &'a str {
    body.get(name).and_then(Value::as_str).unwrap_or("")
}

async fn route(app: &Arc<AppState>, request: Request) -> Response {
    if !host_allowed(app, &request) || !origin_allowed(app, &request) {
        return error_reply(403, "forbidden host or origin");
    }
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();

    if method == Method::GET {
        match path.as_str() {
            "/" => return reply(200, "text/html; charset=utf-8", INDEX_HTML),
            "/app.js" => return reply(200, "text/javascript; charset=utf-8", APP_JS),
            "/style.css" => return reply(200, "text/css; charset=utf-8", STYLE_CSS),
            "/favicon.ico" => return reply(204, "image/x-icon", Body::empty()),
            _ => {}
        }
        if let Some(found) = APP_FRAME_PATH.captures(&path) {
            let key = found[1].to_string();
            let html = {
                let frames = app.frames.lock().unwrap();
                frames
                    .get(&key)
                    .filter(|(_, at)| at.elapsed() <= APP_FRAME_TTL)
                    .map(|(html, _)| html.clone())
            };
            let Some(html) = html else {
                return error_reply(404, "not found");
            };
            let mut response = reply(200, "text/html; charset=utf-8", html);
            response.headers_mut().insert(
                HeaderName::from_static("content-security-policy"),
                HeaderValue::from_static(APP_CSP),
            );
            return response;
        }
        if path == CALLBACK_PATH {
            let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect();
            let outcome = app.auth.chatgpt.handle_callback(&params).await;
            if let Some(url) = &outcome.retry_url {
                let mut response = reply(
                    302,
                    "text/html; charset=utf-8",
                    callback_page(false, &outcome.message),
                );
                if let Ok(location) = HeaderValue::from_str(url) {
                    response
                        .headers_mut()
                        .insert(axum::http::header::LOCATION, location);
                }
                return response;
            }
            return reply(
                if outcome.ok { 200 } else { 400 },
                "text/html; charset=utf-8",
                callback_page(outcome.ok, &outcome.message),
            );
        }
    }

    if !path.starts_with("/api/") {
        return error_reply(404, "not found");
    }
    if !authorized(app, &request) {
        return error_reply(401, "missing or wrong session token");
    }
    let body = request.into_body();
    api(app, method, &path, &query, body).await
}

fn providers_reply(app: &AppState, extra: Value) -> Response {
    let mut out = json!({ "providers": app.auth.describe() });
    if let (Some(out), Some(extra)) = (out.as_object_mut(), extra.as_object()) {
        out.extend(extra.clone());
    }
    json_reply(200, &out)
}

async fn api(app: &Arc<AppState>, method: Method, path: &str, query: &str, body: Body) -> Response {
    match (method.as_str(), path) {
        ("GET", "/api/cluster/summary") => {
            let cluster = query_param(query, "cluster");
            return json_reply(200, &app.clusters.summary(cluster.as_deref()).await);
        }
        ("GET", "/api/clusters") => return json_reply(200, &app.clusters.describe()),
        ("PUT", "/api/clusters/active") => {
            let body = match read_json(body).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            return match app.clusters.select(str_field(&body, "name")) {
                Ok(()) => json_reply(200, &app.clusters.describe()),
                Err(message) => error_reply(400, message),
            };
        }
        ("GET", "/api/providers") => return providers_reply(app, json!({})),
        ("GET", "/api/import/opencode") => {
            return json_reply(
                200,
                &serde_json::to_value(app.auth.scan_open_code()).unwrap(),
            )
        }
        ("POST", "/api/import/opencode") => {
            let imported = app.auth.import_open_code();
            return providers_reply(
                app,
                json!({ "imported": imported, "scan": app.auth.scan_open_code() }),
            );
        }
        ("POST", "/api/providers/chatgpt/signin") => {
            let body = match read_json(body).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            let new_account = body.get("newAccount") == Some(&Value::Bool(true));
            return match app.auth.chatgpt.start(app.port, new_account) {
                Ok(url) => json_reply(200, &json!({ "url": url })),
                Err(error) => error_reply(400, format!("{error:#}")),
            };
        }
        ("DELETE", "/api/providers/chatgpt/signin") => {
            let revoked = app.auth.chatgpt.sign_out().await;
            return providers_reply(app, json!({ "revoked": revoked }));
        }
        ("GET", "/api/sessions") => {
            let mut infos: Vec<_> = app
                .sessions
                .lock()
                .unwrap()
                .values()
                .map(|s| s.info())
                .collect();
            infos.sort_by_key(|info| std::cmp::Reverse(info.created_at));
            let sessions: Vec<Value> = infos
                .into_iter()
                .map(|i| json!({ "id": i.id, "title": i.title, "cluster": i.cluster, "createdAt": i.created_at, "busy": i.busy }))
                .collect();
            return json_reply(200, &json!({ "sessions": sessions }));
        }
        ("POST", "/api/sessions") => {
            let body = match read_json(body).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            let names = app.clusters.names();
            let cluster = match str_field(&body, "cluster") {
                "" => app.clusters.active(),
                wanted if names.iter().any(|n| n == wanted) => Some(wanted.to_string()),
                wanted => {
                    return error_reply(
                        400,
                        format!("There is no cluster named {wanted} in the kctl config."),
                    )
                }
            };
            let toolbox = app.clusters.toolbox(cluster.as_deref()).await;
            let id = uuid::Uuid::new_v4().to_string();
            let session = Session::new(
                id.clone(),
                cluster.clone(),
                toolbox,
                app.log_dir.clone(),
                None,
            );
            app.sessions.lock().unwrap().insert(id.clone(), session);
            return json_reply(201, &json!({ "id": id, "cluster": cluster }));
        }
        _ => {}
    }

    if let Some(found) = PROVIDER_ROUTE.captures(path) {
        let (id, what) = (&found[1], &found[2]);
        match (method.as_str(), what) {
            ("GET", "models") => {
                let Some(provider) = app.auth.provider(id) else {
                    return error_reply(404, "provider is not connected");
                };
                return json_reply(200, &json!({ "models": provider.models().await }));
            }
            ("PUT", "key") => {
                let body = match read_json(body).await {
                    Ok(body) => body,
                    Err(response) => return response,
                };
                return match app.auth.set_key(id, str_field(&body, "key")) {
                    Ok(()) => providers_reply(app, json!({})),
                    Err(error) => error_reply(400, format!("{error:#}")),
                };
            }
            ("DELETE", "key") => {
                return match app.auth.remove_key(id) {
                    Ok(()) => providers_reply(app, json!({})),
                    Err(error) => error_reply(400, format!("{error:#}")),
                };
            }
            _ => {}
        }
    }

    let Some(found) = SESSION_ROUTE.captures(path) else {
        return error_reply(404, "not found");
    };
    let Some(session) = app.sessions.lock().unwrap().get(&found[1]).cloned() else {
        return error_reply(404, "no such session");
    };
    let sub = found.get(2).map(|m| m.as_str());
    session_api(app, &session, method, sub, query, body).await
}

async fn session_api(
    app: &Arc<AppState>,
    session: &Arc<Session>,
    method: Method,
    sub: Option<&str>,
    query: &str,
    body: Body,
) -> Response {
    let get = method == Method::GET;
    let post = method == Method::POST;
    match sub {
        None if get => {
            let info = session.info();
            json_reply(
                200,
                &json!({ "id": info.id, "title": info.title, "busy": info.busy, "events": session.history(0) }),
            )
        }
        Some("events") if get => {
            let after = url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == "after")
                .and_then(|(_, v)| v.parse::<u64>().ok())
                .unwrap_or(0);
            event_stream(session, after)
        }
        Some("messages") if post => {
            let body = match read_json(body).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            let text = str_field(&body, "text").trim().to_string();
            let provider_id = str_field(&body, "provider").to_string();
            let model = str_field(&body, "model").trim().to_string();
            if text.is_empty() {
                return error_reply(400, "text is required");
            }
            let Some(provider) = app.auth.provider(&provider_id) else {
                let shown = if provider_id.is_empty() {
                    "(none)"
                } else {
                    &provider_id
                };
                return error_reply(400, format!("provider {shown} is not connected"));
            };
            if model.is_empty() {
                return error_reply(400, "model is required");
            }
            if session.is_busy() {
                return error_reply(409, "a turn is already running");
            }
            let session = session.clone();
            tokio::spawn(async move {
                let _ = session.send(&text, &provider, &model).await;
            });
            json_reply(202, &json!({ "accepted": true }))
        }
        Some("cancel") if post => {
            session.cancel();
            json_reply(200, &json!({ "ok": true }))
        }
        Some("apps") if post => {
            let body = match read_json(body).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            let html = match session.read_app(str_field(&body, "uri")) {
                Ok(html) => html,
                Err(error) => return error_reply(400, error),
            };
            let Ok(random) = random_bytes(24) else {
                return error_reply(500, "no randomness");
            };
            let key: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let mut frames = app.frames.lock().unwrap();
            frames.retain(|_, (_, at)| at.elapsed() <= APP_FRAME_TTL);
            while frames.len() >= MAX_APP_FRAMES {
                let Some(oldest) = frames
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                frames.remove(&oldest);
            }
            frames.insert(key.clone(), (html, Instant::now()));
            json_reply(200, &json!({ "url": format!("/apps/{key}") }))
        }
        Some("call") if post => {
            let body = match read_json(body).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            let arguments = body.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match session
                .call_from_app(str_field(&body, "name"), &arguments)
                .await
            {
                Ok(result) => json_reply(200, &result),
                Err(error) => error_reply(400, error),
            }
        }
        Some(form) if form.starts_with("forms/") && post => {
            let body = match read_json(body).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            let action = str_field(&body, "action");
            if !["accept", "decline", "cancel"].contains(&action) {
                return error_reply(400, "action must be accept, decline, or cancel");
            }
            let content = body.get("content").cloned().unwrap_or(Value::Null);
            match session.answer_form(&form["forms/".len()..], action, &content) {
                Ok(()) => json_reply(200, &json!({ "ok": true })),
                Err(error) => error_reply(400, error),
            }
        }
        _ => error_reply(405, "method not allowed"),
    }
}

fn event_stream(session: &Arc<Session>, after: u64) -> Response {
    let (past, mut live) = session.subscribe(after);
    let (out, rx) = mpsc::channel::<String>(256);
    tokio::spawn(async move {
        for event in past {
            let line = format!(
                "id: {}\ndata: {}\n\n",
                event.id,
                serde_json::to_string(&event).unwrap_or_default()
            );
            if out.send(line).await.is_err() {
                return;
            }
        }
        let mut tick = tokio::time::interval(KEEP_ALIVE);
        tick.tick().await;
        loop {
            let line = tokio::select! {
                received = live.recv() => match received {
                    Ok(Live::Event(event)) => format!("id: {}\ndata: {}\n\n", event.id, serde_json::to_string(&event).unwrap_or_default()),
                    Ok(Live::Delta(text)) => format!("event: delta\ndata: {}\n\n", json!({ "type": "delta", "text": text })),
                    Err(broadcast::error::RecvError::Lagged(_) | broadcast::error::RecvError::Closed) => return,
                },
                _ = tick.tick() => ": keep-alive\n\n".to_string(),
                () = out.closed() => return,
            };
            if out.send(line).await.is_err() {
                return;
            }
        }
    });
    let stream =
        ReceiverStream::new(rx).map(|line| Ok::<_, Infallible>(axum::body::Bytes::from(line)));
    let mut response = reply(200, "text/event-stream", Body::from_stream(stream));
    response.headers_mut().insert(
        HeaderName::from_static("connection"),
        HeaderValue::from_static("keep-alive"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::auth::AuthOptions;
    use crate::agent::providers::{ModelEvent, Provider, Scripted, ToolCall};
    use crate::agent::tools::Toolbox;
    use crate::agent::transcript::EventBody;
    use crate::agent::util::testutil::{mock, Reply};
    use reqwest::Method as ReqMethod;

    const TOKEN: &str = "tttttttttttttttttttttttttttttttttttttttt";

    struct Harness {
        running: Running,
        base: String,
        home: tempfile::TempDir,
        client: reqwest::Client,
    }

    impl Harness {
        fn port(&self) -> u16 {
            self.running.port
        }

        fn api(&self, method: ReqMethod, path: &str) -> reqwest::RequestBuilder {
            self.client
                .request(method, format!("{}{path}", self.base))
                .bearer_auth(TOKEN)
                .header("content-type", "application/json")
        }

        fn session(&self, id: &str) -> Arc<Session> {
            self.running
                .state
                .sessions
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .expect("session")
        }

        async fn new_session(&self) -> String {
            let made: Value = self
                .api(ReqMethod::POST, "/api/sessions")
                .body("{}")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            made["id"].as_str().unwrap().to_string()
        }
    }

    async fn harness(env: &[(&str, &str)], log_dir: Option<PathBuf>) -> Harness {
        let home = tempfile::tempdir().unwrap();
        let options = AuthOptions {
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            open_code_auth_path: Some(home.path().join("none.json")),
            chatgpt_issuer: None,
            chatgpt_base_url: None,
        };
        let auth = Arc::new(AuthStore::new(home.path().to_path_buf(), options));
        let config = home.path().join("kctl-config");
        std::fs::write(
            &config,
            "current-context: lab\ncontexts:\n  lab:\n    controller: 10.0.0.1:9090\n  home:\n    controller: 10.0.0.2:9090\n",
        )
        .unwrap();
        let clusters = Arc::new(Clusters::new(
            config,
            Some(home.path()),
            Arc::new(|name| {
                Box::pin(async move {
                    match name.as_deref() {
                        Some("home") => Err("no certificates for home".to_string()),
                        _ => Ok(Toolbox::unreachable().endpoints),
                    }
                })
            }),
        ));
        let running = start(
            AppOptions {
                token: TOKEN.to_string(),
                auth,
                clusters,
                log_dir,
            },
            0,
        )
        .await
        .unwrap();
        let base = format!("http://127.0.0.1:{}", running.port);
        Harness {
            running,
            base,
            home,
            client: reqwest::Client::new(),
        }
    }

    async fn wait_for_turn_end(session: &Session) -> Vec<String> {
        for _ in 0..500 {
            let history = session.history(0);
            if history
                .iter()
                .any(|e| matches!(e.body, EventBody::TurnEnd { .. }))
            {
                return history
                    .iter()
                    .map(|e| {
                        serde_json::to_value(e).unwrap()["type"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("turn did not end");
    }

    #[tokio::test]
    async fn the_api_needs_the_session_token_and_a_loopback_host() {
        let h = harness(&[], None).await;
        let get = |path: &str| h.client.get(format!("{}{path}", h.base));
        assert_eq!(get("/api/providers").send().await.unwrap().status(), 401);
        assert_eq!(
            get("/api/providers")
                .bearer_auth("nope")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let port = h.port();
        let with = |host: String, origin: Option<&str>| {
            let mut request = get("/api/providers")
                .bearer_auth(TOKEN)
                .header("host", host);
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            request
        };
        assert_eq!(
            with(format!("evil.example:{port}"), None)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            with(format!("127.0.0.1:{port}"), Some("https://evil.example"))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            with(format!("127.0.0.1:{port}"), None)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            with(format!("localhost:{port}"), None)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let page = get("/").send().await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
        assert_eq!(page.headers()["x-frame-options"], "DENY");
        assert!(page.text().await.unwrap().contains("<"));
        assert_eq!(get("/app.js").send().await.unwrap().status(), 200);
        assert_eq!(get("/style.css").send().await.unwrap().status(), 200);
        assert_eq!(get("/nope").send().await.unwrap().status(), 404);
        h.running.close().await;
    }

    #[tokio::test]
    async fn the_server_listens_on_loopback_only() {
        let h = harness(&[], None).await;
        let bound = std::net::TcpStream::connect(("127.0.0.1", h.port()));
        assert!(bound.is_ok());
        h.running.close().await;
    }

    #[tokio::test]
    async fn a_provider_key_is_stored_0600_and_never_returned() {
        use std::os::unix::fs::PermissionsExt;
        let models =
            mock(|_| Reply::json(json!({ "data": [{ "id": "gpt-b" }, { "id": "gpt-a" }] }))).await;
        let h = harness(&[("OPENAI_BASE_URL", &models.base)], None).await;
        let put = h
            .api(ReqMethod::PUT, "/api/providers/openai/key")
            .body(json!({ "key": "sk-secret-123" }).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), 200);
        assert!(!put.text().await.unwrap().contains("sk-secret-123"));
        let mode = std::fs::metadata(h.home.path().join("auth.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let listed: Value = h
            .api(ReqMethod::GET, "/api/providers/openai/models")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["models"], json!(["gpt-a", "gpt-b"]));
        let nope = h
            .api(ReqMethod::PUT, "/api/providers/nope/key")
            .body(json!({ "key": "x" }).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(nope.status(), 400);
        let removed = h
            .api(ReqMethod::DELETE, "/api/providers/openai/key")
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), 200);
        h.running.close().await;
    }

    #[tokio::test]
    async fn a_chat_turn_streams_from_the_provider_runs_a_tool_and_lands_in_the_session() {
        let llm = mock(|seen| {
            if seen.path.ends_with("/models") {
                return Reply::json(json!({ "data": [{ "id": "gpt-a" }] }));
            }
            let body: Value = serde_json::from_str(&seen.body).unwrap();
            assert!(!body["tools"].as_array().unwrap().is_empty());
            let has_tool_result = body["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool");
            if has_tool_result {
                return Reply::sse("data: {\"choices\":[{\"delta\":{\"content\":\"Done.\"}}]}\n\ndata: [DONE]\n\n");
            }
            Reply::sse(concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"kcore_cat\",\"arguments\":\"\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"alog\",\"arguments\":\"{}\"}}]}}]}\n\n",
                "data: [DONE]\n\n"
            ))
        })
        .await;
        let h = harness(
            &[("OPENAI_API_KEY", "sk-x"), ("OPENAI_BASE_URL", &llm.base)],
            None,
        )
        .await;
        let id = h.new_session().await;
        let post = |body: Value| {
            h.api(ReqMethod::POST, &format!("/api/sessions/{id}/messages"))
                .body(body.to_string())
        };
        assert_eq!(
            post(json!({ "text": "hi", "provider": "deepseek", "model": "m" }))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            post(json!({ "text": "hi", "provider": "openai", "model": "" }))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            post(json!({ "text": " ", "provider": "openai", "model": "m" }))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        let sent =
            post(json!({ "text": "what do you manage?", "provider": "openai", "model": "gpt-a" }))
                .send()
                .await
                .unwrap();
        assert_eq!(sent.status(), 202);
        let types = wait_for_turn_end(&h.session(&id)).await;
        assert_eq!(
            types,
            [
                "user",
                "turn_start",
                "tool_call",
                "tool_result",
                "assistant",
                "turn_end"
            ]
        );
        let detail: Value = h
            .api(ReqMethod::GET, &format!("/api/sessions/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail["events"].as_array().unwrap().len(), 6);
        assert_eq!(detail["title"], "what do you manage?");
        let listed: Value = h
            .api(ReqMethod::GET, "/api/sessions")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["sessions"][0]["id"], id.as_str());
        h.running.close().await;
    }

    #[tokio::test]
    async fn the_event_stream_replays_history_then_follows_live_events() {
        let h = harness(&[], None).await;
        let id = h.new_session().await;
        let session = h.session(&id);
        let scripted = Scripted::new(|_| vec![ModelEvent::Text("hello".into())]);
        session
            .send("hi", &Provider::Scripted(scripted), "m")
            .await
            .unwrap();

        let response = h
            .api(
                ReqMethod::GET,
                &format!("/api/sessions/{id}/events?after=1"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut stream = response.bytes_stream();
        let mut seen = String::new();
        while !seen.contains("turn_end") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(
            !seen.contains("\"type\":\"user\""),
            "after=1 skips the first event: {seen}"
        );
        assert!(seen.contains("id: 4\n"));

        let live_session = session.clone();
        let live = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let scripted = Scripted::new(|_| vec![ModelEvent::Text("again".into())]);
            live_session
                .send("more", &Provider::Scripted(scripted), "m")
                .await
                .unwrap();
        });
        let mut later = String::new();
        while !later.contains("event: delta") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            later.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(later.contains("\"text\":\"again\""));
        live.await.unwrap();
        h.running.close().await;
    }

    #[tokio::test]
    async fn answering_a_form_over_http_validates_the_content() {
        let h = harness(&[], None).await;
        let id = h.new_session().await;
        let session = h.session(&id);
        let scripted = Scripted::new(|turn| {
            if turn == 0 {
                vec![ModelEvent::ToolCall(ToolCall {
                    id: "c".into(),
                    name: "kcore_delete".into(),
                    arguments: json!({ "kind": "vm", "name": "web" }).to_string(),
                })]
            } else {
                Vec::new()
            }
        });
        let pending = tokio::spawn({
            let session = session.clone();
            async move { session.send("x", &Provider::Scripted(scripted), "m").await }
        });
        let form_id = loop {
            if let Some(EventBody::Form { form_id, .. }) = session
                .history(0)
                .into_iter()
                .map(|e| e.body)
                .find(|b| matches!(b, EventBody::Form { .. }))
            {
                break form_id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let answer = |body: Value| {
            h.api(
                ReqMethod::POST,
                &format!("/api/sessions/{id}/forms/{form_id}"),
            )
            .body(body.to_string())
        };
        assert_eq!(
            answer(json!({ "action": "accept", "content": { "confirm": "perhaps" } }))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            answer(json!({ "action": "shrug" }))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            answer(json!({ "action": "accept", "content": { "confirm": "no" } }))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        pending.await.unwrap().unwrap();
        h.running.close().await;
    }

    #[tokio::test]
    async fn the_chatgpt_sign_in_starts_behind_the_token_and_the_callback_needs_a_known_state() {
        let h = harness(&[], None).await;
        let unauth = h
            .client
            .post(format!("{}/api/providers/chatgpt/signin", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), 401);
        let flow = h
            .api(ReqMethod::POST, "/api/providers/chatgpt/signin")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(flow.status(), 200);
        let started: Value = flow.json().await.unwrap();
        let url = url::Url::parse(started["url"].as_str().unwrap()).unwrap();
        let redirect = url
            .query_pairs()
            .find(|(k, _)| k == "redirect_uri")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(
            redirect,
            format!("http://127.0.0.1:{}/auth/callback", h.port())
        );
        let providers: Value = h
            .api(ReqMethod::GET, "/api/providers")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let ids: Vec<&str> = providers["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "chatgpt",
                "openai",
                "anthropic",
                "deepseek",
                "opencode",
                "opencode-go"
            ]
        );

        let unknown = h
            .client
            .get(format!("{}/auth/callback?code=x&state=unknown", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), 400);
        let rebound = h
            .client
            .get(format!("{}/auth/callback?state=x", h.base))
            .header("host", format!("evil.example:{}", h.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status(), 403);
        h.running.close().await;
    }

    #[tokio::test]
    async fn a_plan_result_carries_its_app_served_sandboxed_and_apps_make_read_only_calls() {
        let h = harness(&[], None).await;
        let id = h.new_session().await;
        let session = h.session(&id);
        let scripted = Scripted::new(|turn| {
            if turn == 0 {
                vec![Scripted::call(
                    "p1",
                    "kcore_plan",
                    &json!({ "kind": "cluster", "spec": { "name": "lab", "controller": "10.0.0.1" } }),
                )]
            } else {
                Vec::new()
            }
        });
        session
            .send("plan a cluster", &Provider::Scripted(scripted), "m")
            .await
            .unwrap();
        let app = session.history(0).into_iter().find_map(|e| match e.body {
            EventBody::ToolResult { app, .. } => Some(app),
            _ => None,
        });
        assert_eq!(
            app.flatten().map(|a| a.uri),
            Some("ui://kcore/plan".to_string())
        );

        let post = |path: String, body: Value| h.api(ReqMethod::POST, &path).body(body.to_string());
        let refused = post(
            format!("/api/sessions/{id}/apps"),
            json!({ "uri": "ui://evil/app" }),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(refused.status(), 400);
        let made: Value = post(
            format!("/api/sessions/{id}/apps"),
            json!({ "uri": "ui://kcore/plan" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        let url = made["url"].as_str().unwrap().to_string();
        assert!(Regex::new(r"^/apps/[0-9a-f]{48}$").unwrap().is_match(&url));

        let page = h
            .client
            .get(format!("{}{url}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let csp = page.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("sandbox allow-scripts") && csp.contains("default-src 'none'"));
        assert!(page.headers().get("x-frame-options").is_none());
        assert!(page.text().await.unwrap().contains("ui/initialize"));
        assert_eq!(
            h.client
                .get(format!("{}/apps/{}", h.base, "0".repeat(48)))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let index = h.client.get(format!("{}/", h.base)).send().await.unwrap();
        assert!(index.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-src 'self'"));

        let read = post(
            format!("/api/sessions/{id}/call"),
            json!({ "name": "kcore_catalog", "arguments": {} }),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(read.status(), 200);
        let blocked = post(
            format!("/api/sessions/{id}/call"),
            json!({ "name": "kcore_apply", "arguments": { "kind": "vm", "confirm": true } }),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(blocked.status(), 400);
        let error: Value = blocked.json().await.unwrap();
        assert!(error["error"]
            .as_str()
            .unwrap()
            .contains("may not call kcore_apply"));
        h.running.close().await;
    }

    #[tokio::test]
    async fn the_app_lists_sessions_it_finds_on_disk() {
        let logs = tempfile::tempdir().unwrap();
        let id = "11111111-2222-3333-4444-555555555555";
        let first = Session::new(
            id.to_string(),
            Some("lab".to_string()),
            Arc::new(Toolbox::unreachable()),
            Some(logs.path().to_path_buf()),
            None,
        );
        let scripted = Scripted::new(|_| vec![ModelEvent::Text("hi".into())]);
        first
            .send("hello", &Provider::Scripted(scripted), "m")
            .await
            .unwrap();
        drop(first);

        let h = harness(&[], Some(logs.path().to_path_buf())).await;
        let listed: Value = h
            .api(ReqMethod::GET, "/api/sessions")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(listed["sessions"][0]["id"], id);
        assert_eq!(listed["sessions"][0]["title"], "hello");
        let detail: Value = h
            .api(ReqMethod::GET, &format!("/api/sessions/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail["events"].as_array().unwrap().len(), 4);
        h.running.close().await;
    }

    #[tokio::test]
    async fn bodies_over_the_limit_and_bad_json_are_refused() {
        let h = harness(&[], None).await;
        let big = "x".repeat(MAX_BODY + 10);
        let too_big = h
            .api(ReqMethod::PUT, "/api/providers/openai/key")
            .body(big)
            .send()
            .await
            .unwrap();
        assert_eq!(too_big.status(), 413);
        let bad = h
            .api(ReqMethod::PUT, "/api/providers/openai/key")
            .body("{oops")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
        h.running.close().await;
    }

    #[tokio::test]
    async fn the_clusters_are_listed_and_the_choice_is_remembered() {
        let h = harness(&[], None).await;
        let listed: Value = h
            .api(ReqMethod::GET, "/api/clusters")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["active"], "lab");
        assert_eq!(listed["clusters"][0]["name"], "home");
        assert_eq!(listed["clusters"][1]["controller"], "10.0.0.1:9090");

        let refused = h
            .api(ReqMethod::PUT, "/api/clusters/active")
            .body(r#"{"name":"nowhere"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 400);

        let chosen: Value = h
            .api(ReqMethod::PUT, "/api/clusters/active")
            .body(r#"{"name":"home"}"#)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(chosen["active"], "home");
        let saved = std::fs::read_to_string(h.home.path().join("settings.json")).unwrap();
        assert!(saved.contains("\"home\""));
        h.running.close().await;
    }

    #[tokio::test]
    async fn a_session_is_bound_to_the_cluster_chosen_when_it_starts() {
        let h = harness(&[], None).await;
        let default: Value = h
            .api(ReqMethod::POST, "/api/sessions")
            .body("{}")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(default["cluster"], "lab");
        let other: Value = h
            .api(ReqMethod::POST, "/api/sessions")
            .body(r#"{"cluster":"home"}"#)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(other["cluster"], "home");
        let bad = h
            .api(ReqMethod::POST, "/api/sessions")
            .body(r#"{"cluster":"nowhere"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);

        let listed: Value = h
            .api(ReqMethod::GET, "/api/sessions")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let clusters: Vec<&str> = listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["cluster"].as_str())
            .collect();
        assert_eq!(clusters.len(), 2);
        assert!(clusters.contains(&"lab") && clusters.contains(&"home"));
        h.running.close().await;
    }

    #[tokio::test]
    async fn the_summary_follows_the_cluster_asked_for_and_reports_one_that_cannot_connect() {
        let h = harness(&[], None).await;
        let lab: Value = h
            .api(ReqMethod::GET, "/api/cluster/summary?cluster=lab")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(lab["controller"], "127.0.0.1:1");
        let home: Value = h
            .api(ReqMethod::GET, "/api/cluster/summary?cluster=home")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(home["error"], "no certificates for home");
        h.running.close().await;
    }

    #[tokio::test]
    async fn a_restored_session_keeps_its_cluster() {
        let logs = tempfile::tempdir().unwrap();
        let h = harness(&[], Some(logs.path().to_path_buf())).await;
        let made: Value = h
            .api(ReqMethod::POST, "/api/sessions")
            .body(r#"{"cluster":"home"}"#)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(made["cluster"], "home");
        let session = h.session(made["id"].as_str().unwrap());
        let scripted = Scripted::new(|_| vec![ModelEvent::Text("hi".into())]);
        session
            .send("hello", &Provider::Scripted(scripted), "m")
            .await
            .unwrap();
        h.running.close().await;

        let again = harness(&[], Some(logs.path().to_path_buf())).await;
        let listed: Value = again
            .api(ReqMethod::GET, "/api/sessions")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["sessions"][0]["cluster"], "home");
        again.running.close().await;
    }
}
