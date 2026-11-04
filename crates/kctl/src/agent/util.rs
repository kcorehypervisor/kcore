use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

pub fn random_bytes(count: usize) -> Result<Vec<u8>> {
    let mut buffer = vec![0u8; count];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut buffer)
        .context("reading /dev/urandom")?;
    Ok(buffer)
}

pub fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn random_token(bytes: usize) -> Result<String> {
    Ok(base64url(&random_bytes(bytes)?))
}

pub fn sha256_base64url(text: &str) -> String {
    base64url(&Sha256::digest(text.as_bytes()))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Write `contents` to `path` with mode 0600 through a temp file, so a crash never leaves half a secret.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).ok();
    }
    let suffix = random_token(4)?;
    let temp = path.with_extension(format!("{}.{suffix}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("writing {}", temp.display()))?;
    file.write_all(contents)?;
    file.sync_all().ok();
    drop(file);
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
pub mod testutil {
    use std::sync::{Arc, Mutex};

    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::Router;

    #[derive(Debug, Clone)]
    pub struct Seen {
        pub method: String,
        pub path: String,
        pub query: String,
        pub body: String,
        pub headers: HeaderMap,
    }

    pub struct Reply {
        pub status: u16,
        pub content_type: &'static str,
        pub body: String,
    }

    impl Reply {
        pub fn json(value: serde_json::Value) -> Self {
            Self {
                status: 200,
                content_type: "application/json",
                body: value.to_string(),
            }
        }

        pub fn status(status: u16, body: &str) -> Self {
            Self {
                status,
                content_type: "text/plain",
                body: body.to_string(),
            }
        }

        pub fn sse(body: &str) -> Self {
            Self {
                status: 200,
                content_type: "text/event-stream",
                body: body.to_string(),
            }
        }
    }

    type Handler = dyn Fn(&Seen) -> Reply + Send + Sync;

    pub struct Mock {
        pub base: String,
        pub seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Mock {
        pub fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[derive(Clone)]
    struct Shared {
        seen: Arc<Mutex<Vec<Seen>>>,
        handler: Arc<Handler>,
    }

    async fn respond(
        State(shared): State<Shared>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let seen = Seen {
            method: method.to_string(),
            path: uri.path().to_string(),
            query: uri.query().unwrap_or("").to_string(),
            body: String::from_utf8_lossy(&body).into_owned(),
            headers,
        };
        shared.seen.lock().unwrap().push(seen.clone());
        let reply = (shared.handler)(&seen);
        (
            StatusCode::from_u16(reply.status).unwrap(),
            [("content-type", reply.content_type)],
            reply.body,
        )
            .into_response()
    }

    pub async fn mock(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Mock {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shared = Shared {
            seen: seen.clone(),
            handler: Arc::new(handler),
        };
        let app = Router::new().fallback(respond).with_state(shared);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Mock { base, seen }
    }
}
