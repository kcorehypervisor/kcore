use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::client::legacy::Client as HyperClient;
use hyperlocal::{UnixClientExt, Uri};
use serde::Serialize;
use tracing::warn;

use super::types::VmInfo;

/// Client that discovers Cloud Hypervisor API sockets in a directory
/// and queries / mutates them for VM status and live migration.
#[derive(Clone)]
pub struct Client {
    socket_dir: PathBuf,
}

impl Client {
    pub fn new(socket_dir: &str) -> Self {
        Self {
            socket_dir: PathBuf::from(socket_dir),
        }
    }

    pub fn socket_dir(&self) -> &Path {
        &self.socket_dir
    }

    pub fn socket_path(&self, name: &str) -> PathBuf {
        self.socket_dir.join(format!("{name}.sock"))
    }

    /// List all VM names by scanning for `*.sock` files in the socket directory.
    pub fn list_vm_names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.socket_dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) == Some("sock") {
                    path.file_stem().and_then(|s| s.to_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Query a single VM's status via its Cloud Hypervisor API socket.
    pub async fn get_vm_info(&self, name: &str) -> Option<VmInfo> {
        let socket_path = self.socket_path(name);
        query_vm_info(&socket_path).await
    }

    /// Query all VMs and return (name, info) pairs.
    pub async fn list_vms(&self) -> Vec<(String, VmInfo)> {
        let names = self.list_vm_names();
        let mut results = Vec::with_capacity(names.len());
        for name in names {
            if let Some(info) = self.get_vm_info(&name).await {
                results.push((name, info));
            }
        }
        results
    }

    pub async fn receive_migration(&self, name: &str, receiver_url: &str) -> Result<(), String> {
        let body = ReceiveMigrationBody {
            receiver_url: receiver_url.to_string(),
        };
        put_json(
            &self.socket_path(name),
            "/api/v1/vm.receive-migration",
            &body,
        )
        .await
    }

    pub async fn send_migration(&self, name: &str, destination_url: &str) -> Result<(), String> {
        let body = SendMigrationBody {
            destination_url: destination_url.to_string(),
            local: None,
        };
        put_json(&self.socket_path(name), "/api/v1/vm.send-migration", &body).await
    }

    /// Wait until the API socket answers `vm.info` (or timeout).
    pub async fn wait_api_ready(&self, name: &str, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.get_vm_info(name).await.is_some() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("timed out waiting for CH API socket for {name}"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[derive(Serialize)]
struct ReceiveMigrationBody {
    receiver_url: String,
}

#[derive(Serialize)]
struct SendMigrationBody {
    destination_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    local: Option<bool>,
}

async fn query_vm_info(socket_path: &Path) -> Option<VmInfo> {
    let client = HyperClient::unix();
    let uri = Uri::new(socket_path, "/api/v1/vm.info");

    let req = Request::get(uri).body(Empty::<Bytes>::new()).ok()?;

    let resp = match client.request(req).await {
        Ok(r) => r,
        Err(e) => {
            warn!(socket = %socket_path.display(), error = %e, "failed to query CH socket");
            return None;
        }
    };

    let body = resp.into_body().collect().await.ok()?.to_bytes();
    serde_json::from_slice(&body).ok()
}

async fn put_json<T: Serialize>(socket_path: &Path, path: &str, body: &T) -> Result<(), String> {
    let client = HyperClient::unix();
    let uri = Uri::new(socket_path, path);
    let payload = serde_json::to_vec(body).map_err(|e| format!("encode json: {e}"))?;
    let req = Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(payload)))
        .map_err(|e| format!("build request: {e}"))?;
    let resp = client
        .request(req)
        .await
        .map_err(|e| format!("CH {path} request failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp
        .into_body()
        .collect()
        .await
        .map(|b| String::from_utf8_lossy(&b.to_bytes()).to_string())
        .unwrap_or_default();
    Err(format!("CH {path} returned {status}: {}", body.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_bodies_serialize_expected_fields() {
        let recv = serde_json::to_value(ReceiveMigrationBody {
            receiver_url: "tcp:0.0.0.0:9000".into(),
        })
        .unwrap();
        assert_eq!(recv["receiver_url"], "tcp:0.0.0.0:9000");
        let send = serde_json::to_value(SendMigrationBody {
            destination_url: "tcp:10.0.0.2:9000".into(),
            local: None,
        })
        .unwrap();
        assert_eq!(send["destination_url"], "tcp:10.0.0.2:9000");
        assert!(send.get("local").is_none());
    }
}
