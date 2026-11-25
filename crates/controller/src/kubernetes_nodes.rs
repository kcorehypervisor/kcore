//! Node-agent and Kubernetes/etcd I/O for the Kubernetes orchestrator.
//!
//! Production uses [`GrpcNodes`]. Tests pass a [`Fake`].

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use tonic::Request;

use crate::kubernetes_lifecycle::{
    match_registered_node, nodes_from_list, pods_from_list, pods_to_evict, ListedNode,
};
use crate::kubernetes_upgrade::ApplyProgress;
use crate::node_client::NodeClients;
use crate::node_proto;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A secret file written outside `/nix/store` before a Kubernetes apply.
#[derive(Debug, Clone, Default)]
pub struct SecretFile {
    pub path: String,
    pub mode: u32,
    pub content: Vec<u8>,
}

/// Calls into one node agent (by address `host:9091`) and the cluster APIs.
pub trait KubernetesNodes: Send + Sync {
    fn apply_config<'a>(
        &'a self,
        address: &'a str,
        nix: &'a str,
        kubeconfig: &'a str,
        secrets: &'a [SecretFile],
    ) -> BoxFuture<'a, Result<String, String>>;

    fn apply_status<'a>(
        &'a self,
        address: &'a str,
        apply_id: &'a str,
    ) -> BoxFuture<'a, (ApplyProgress, String)>;

    fn kube<'a>(
        &'a self,
        address: &'a str,
        method: &'a str,
        path: &'a str,
        body: &'a str,
        content_type: &'a str,
    ) -> BoxFuture<'a, Result<(i32, String), String>>;

    fn etcd_health<'a>(
        &'a self,
        ip: &'a str,
        port: u16,
        ca_pem: &'a str,
        client_cert_pem: &'a str,
        client_key_pem: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>>;

    fn etcd_post<'a>(
        &'a self,
        ip: &'a str,
        port: u16,
        path: &'a str,
        body: &'a str,
        ca_pem: &'a str,
        client_cert_pem: &'a str,
        client_key_pem: &'a str,
    ) -> BoxFuture<'a, Result<(u16, String), String>>;

    fn apiserver_ready<'a>(
        &'a self,
        addr: &'a str,
        server_name: &'a str,
        ca_pem: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>>;

    fn etcd_snapshot<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>>;

    fn etcd_restore<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
        snapshot: &'a [u8],
        initial_cluster: &'a str,
        initial_cluster_token: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;

    fn write_secrets<'a>(
        &'a self,
        address: &'a str,
        secrets: &'a [SecretFile],
    ) -> BoxFuture<'a, Result<(), String>>;

    fn restart_units<'a>(
        &'a self,
        address: &'a str,
        units: &'a [String],
    ) -> BoxFuture<'a, Result<(), String>> {
        self.manage_units(address, units, "restart")
    }

    fn manage_units<'a>(
        &'a self,
        address: &'a str,
        units: &'a [String],
        action: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// Remove the etcd data directory for `cluster` so a reinstalled host can rejoin.
    fn wipe_etcd_data<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// True when the local etcd data dir is missing or empty (reinstall signal).
    fn etcd_data_empty<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>>;
}

pub struct GrpcNodes {
    clients: NodeClients,
}

impl GrpcNodes {
    pub fn new(clients: NodeClients) -> Self {
        Self { clients }
    }

    async fn admin(
        &self,
        address: &str,
    ) -> Result<
        node_proto::node_admin_client::NodeAdminClient<crate::node_client::NodeChannel>,
        String,
    > {
        if self.clients.get_admin(address).is_none() {
            self.clients
                .connect(address)
                .await
                .map_err(|e| format!("unreachable: connect {address}: {e}"))?;
        }
        self.clients
            .get_admin(address)
            .ok_or_else(|| format!("unreachable: no admin client for {address}"))
    }
}

fn rpc_err(address: &str, what: &str, e: tonic::Status) -> String {
    if matches!(
        e.code(),
        tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
    ) {
        format!("unreachable: {what} {address}: {}", e.message())
    } else {
        format!("{what} {address}: {}", e.message())
    }
}

impl KubernetesNodes for GrpcNodes {
    fn apply_config<'a>(
        &'a self,
        address: &'a str,
        nix: &'a str,
        kubeconfig: &'a str,
        secrets: &'a [SecretFile],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if !secrets.is_empty() {
                self.write_secrets(address, secrets).await?;
            }
            let mut c = self.admin(address).await?;
            let response = c
                .apply_raw_kubernetes_config(Request::new(
                    node_proto::ApplyRawKubernetesConfigRequest {
                        nix_config_content: nix.to_string(),
                        kubeconfig_data: kubeconfig.to_string(),
                        secrets: secrets
                            .iter()
                            .map(|s| node_proto::KubernetesSecretFile {
                                path: s.path.clone(),
                                mode: s.mode,
                                content: s.content.clone(),
                            })
                            .collect(),
                    },
                ))
                .await
                .map_err(|e| rpc_err(address, "ApplyRawKubernetesConfig", e))?
                .into_inner();
            Ok(response.apply_id)
        })
    }

    fn apply_status<'a>(
        &'a self,
        address: &'a str,
        apply_id: &'a str,
    ) -> BoxFuture<'a, (ApplyProgress, String)> {
        Box::pin(async move {
            let Ok(mut c) = self.admin(address).await else {
                return (
                    ApplyProgress::Running,
                    format!("node agent {address} unreachable"),
                );
            };
            match c
                .get_nix_apply_status(Request::new(node_proto::GetNixApplyStatusRequest {
                    apply_id: apply_id.to_string(),
                }))
                .await
            {
                Ok(response) => {
                    let response = response.into_inner();
                    let progress = match node_proto::NixApplyPhase::try_from(response.phase) {
                        Ok(node_proto::NixApplyPhase::Running) => ApplyProgress::Running,
                        Ok(node_proto::NixApplyPhase::Succeeded) => ApplyProgress::Succeeded,
                        Ok(node_proto::NixApplyPhase::Failed) => ApplyProgress::Failed,
                        _ => ApplyProgress::Unknown,
                    };
                    (progress, response.message)
                }
                Err(err) => (ApplyProgress::Running, err.message().to_string()),
            }
        })
    }

    fn kube<'a>(
        &'a self,
        address: &'a str,
        method: &'a str,
        path: &'a str,
        body: &'a str,
        content_type: &'a str,
    ) -> BoxFuture<'a, Result<(i32, String), String>> {
        Box::pin(async move {
            let mut c = self.admin(address).await?;
            let response = c
                .kubernetes_admin(Request::new(node_proto::KubernetesAdminRequest {
                    method: method.to_string(),
                    path: path.to_string(),
                    body: body.to_string(),
                    content_type: content_type.to_string(),
                    bearer_token: String::new(),
                }))
                .await
                .map_err(|e| rpc_err(address, "KubernetesAdmin", e))?
                .into_inner();
            if !response.message.is_empty() && response.status_code == 0 {
                return Err(response.message);
            }
            Ok((response.status_code, response.body))
        })
    }

    fn etcd_health<'a>(
        &'a self,
        ip: &'a str,
        port: u16,
        ca_pem: &'a str,
        client_cert_pem: &'a str,
        client_key_pem: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let (status, body) = tls_get(
                &format!("{ip}:{port}"),
                ip,
                "/health",
                ca_pem,
                Some((client_cert_pem, client_key_pem)),
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(response_reports_ready("/health", status, &body))
        })
    }

    fn etcd_post<'a>(
        &'a self,
        ip: &'a str,
        port: u16,
        path: &'a str,
        body: &'a str,
        ca_pem: &'a str,
        client_cert_pem: &'a str,
        client_key_pem: &'a str,
    ) -> BoxFuture<'a, Result<(u16, String), String>> {
        Box::pin(async move {
            tls_post(
                &format!("{ip}:{port}"),
                ip,
                path,
                body,
                ca_pem,
                client_cert_pem,
                client_key_pem,
            )
            .await
            .map_err(|e| e.to_string())
        })
    }

    fn apiserver_ready<'a>(
        &'a self,
        addr: &'a str,
        server_name: &'a str,
        ca_pem: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            if server_name.is_empty() {
                return Ok(false);
            }
            let (status, body) = tls_get(addr, server_name, "/readyz", ca_pem, None)
                .await
                .map_err(|e| e.to_string())?;
            Ok(response_reports_ready("/readyz", status, &body))
        })
    }

    fn etcd_snapshot<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let mut c = self.admin(address).await?;
            let response = c
                .etcd_snapshot(Request::new(node_proto::EtcdSnapshotRequest {
                    cluster_name: cluster.to_string(),
                }))
                .await
                .map_err(|e| rpc_err(address, "EtcdSnapshot", e))?
                .into_inner();
            if !response.message.is_empty() && response.snapshot.is_empty() {
                return Err(response.message);
            }
            Ok(response.snapshot)
        })
    }

    fn etcd_restore<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
        snapshot: &'a [u8],
        initial_cluster: &'a str,
        initial_cluster_token: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut c = self.admin(address).await?;
            let response = c
                .etcd_restore(Request::new(node_proto::EtcdRestoreRequest {
                    cluster_name: cluster.to_string(),
                    snapshot: snapshot.to_vec(),
                    initial_cluster: initial_cluster.to_string(),
                    initial_cluster_token: initial_cluster_token.to_string(),
                }))
                .await
                .map_err(|e| rpc_err(address, "EtcdRestore", e))?
                .into_inner();
            if !response.ok {
                return Err(response.message);
            }
            Ok(())
        })
    }

    fn write_secrets<'a>(
        &'a self,
        address: &'a str,
        secrets: &'a [SecretFile],
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut c = self.admin(address).await?;
            let response = c
                .write_kubernetes_secrets(Request::new(node_proto::WriteKubernetesSecretsRequest {
                    secrets: secrets
                        .iter()
                        .map(|s| node_proto::KubernetesSecretFile {
                            path: s.path.clone(),
                            mode: s.mode,
                            content: s.content.clone(),
                        })
                        .collect(),
                }))
                .await
                .map_err(|e| rpc_err(address, "WriteKubernetesSecrets", e))?
                .into_inner();
            if !response.ok {
                return Err(response.message);
            }
            Ok(())
        })
    }

    fn manage_units<'a>(
        &'a self,
        address: &'a str,
        units: &'a [String],
        action: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut c = self.admin(address).await?;
            let response = c
                .restart_systemd_units(Request::new(node_proto::RestartSystemdUnitsRequest {
                    units: units.to_vec(),
                    action: action.to_string(),
                }))
                .await
                .map_err(|e| rpc_err(address, "RestartSystemdUnits", e))?
                .into_inner();
            if !response.ok {
                return Err(response.message);
            }
            Ok(())
        })
    }

    fn wipe_etcd_data<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut c = self.admin(address).await?;
            let response = c
                .wipe_etcd_data(Request::new(node_proto::WipeEtcdDataRequest {
                    cluster_name: cluster.to_string(),
                    check_only: false,
                }))
                .await
                .map_err(|e| rpc_err(address, "WipeEtcdData", e))?
                .into_inner();
            if !response.ok {
                return Err(response.message);
            }
            Ok(())
        })
    }

    fn etcd_data_empty<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let mut c = self.admin(address).await?;
            let response = c
                .wipe_etcd_data(Request::new(node_proto::WipeEtcdDataRequest {
                    cluster_name: cluster.to_string(),
                    check_only: true,
                }))
                .await
                .map_err(|e| rpc_err(address, "WipeEtcdData", e))?
                .into_inner();
            if !response.ok {
                return Err(response.message);
            }
            Ok(response.empty)
        })
    }
}

fn response_reports_ready(path: &str, status: u16, body: &str) -> bool {
    if status != 200 {
        return false;
    }
    if path == "/health" {
        body.replace([' ', '\n'], "")
            .contains("\"health\":\"true\"")
    } else {
        body.contains("ok")
    }
}

async fn tls_get(
    addr: &str,
    server_name: &str,
    path: &str,
    ca_pem: &str,
    client_auth: Option<(&str, &str)>,
) -> Result<(u16, String)> {
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut roots = rustls::RootCertStore::empty();
    for cert in pem_certs(ca_pem)? {
        roots.add(cert).context("etcd or kubernetes CA")?;
    }
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let config = if let Some((cert_pem, key_pem)) = client_auth {
        builder
            .with_client_auth_cert(pem_certs(cert_pem)?, pem_key(key_pem)?)
            .context("etcd client certificate")?
    } else {
        builder.with_no_client_auth()
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from(server_name.to_string()).context("server name")?;
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let mut tls = connector
        .connect(name, stream)
        .await
        .context("tls handshake")?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {server_name}\r\nConnection: close\r\n\r\n");
    tls.write_all(request.as_bytes())
        .await
        .context("write request")?;
    let mut buf = Vec::new();
    tls.read_to_end(&mut buf).await.context("read response")?;
    parse_http_response(&buf)
}

async fn tls_post(
    addr: &str,
    server_name: &str,
    path: &str,
    body: &str,
    ca_pem: &str,
    client_cert_pem: &str,
    client_key_pem: &str,
) -> Result<(u16, String)> {
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut roots = rustls::RootCertStore::empty();
    for cert in pem_certs(ca_pem)? {
        roots.add(cert).context("etcd CA")?;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(pem_certs(client_cert_pem)?, pem_key(client_key_pem)?)
        .context("etcd client certificate")?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from(server_name.to_string()).context("server name")?;
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let mut tls = connector
        .connect(name, stream)
        .await
        .context("tls handshake")?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {server_name}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes())
        .await
        .context("write request")?;
    let mut buf = Vec::new();
    tls.read_to_end(&mut buf).await.context("read response")?;
    parse_http_response(&buf)
}

fn parse_http_response(buf: &[u8]) -> Result<(u16, String)> {
    let text = String::from_utf8_lossy(buf);
    let mut parts = text.splitn(2, "\r\n\r\n");
    let headers = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_string();
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, body))
}

fn pem_certs(pem_str: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::CertificateDer;
    let mut certs = Vec::new();
    for block in pem::parse_many(pem_str).context("parsing certificate pem")? {
        if block.tag() == "CERTIFICATE" {
            certs.push(CertificateDer::from(block.contents().to_vec()));
        }
    }
    if certs.is_empty() {
        return Err(anyhow!("no certificate in pem"));
    }
    Ok(certs)
}

fn pem_key(pem_str: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    use rustls::pki_types::{
        PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
    };
    let block = pem::parse(pem_str).context("parsing private key pem")?;
    let bytes = block.contents().to_vec();
    match block.tag() {
        "PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bytes))),
        "EC PRIVATE KEY" => Ok(PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(bytes))),
        "RSA PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(bytes))),
        other => Err(anyhow!("unsupported private key type {other}")),
    }
}

/// In-memory node agent used by orchestrator tests.
#[derive(Default)]
pub struct Fake {
    pub applies: Mutex<BTreeMap<String, String>>,
    pub apply_status: Mutex<BTreeMap<String, ApplyProgress>>,
    pub apply_fail: Mutex<BTreeMap<String, String>>,
    pub unreachable: Mutex<Vec<String>>,
    pub etcd_members: Mutex<BTreeMap<String, bool>>,
    pub api_nodes: Mutex<Vec<ListedNode>>,
    pub api_ready: Mutex<bool>,
    pub calls: Mutex<Vec<(String, String)>>,
    pub secrets: Mutex<Vec<SecretFile>>,
    pub snapshots: Mutex<BTreeMap<String, Vec<u8>>>,
    pub units_restarted: Mutex<Vec<(String, Vec<String>)>>,
    pub clock_offset_secs: Mutex<i64>,
    /// Addresses whose etcd data dir is empty (reinstall signal).
    pub etcd_empty: Mutex<std::collections::HashSet<String>>,
}

impl Fake {
    pub fn new(node_ips: &[&str]) -> Arc<Self> {
        let fake = Fake::default();
        {
            let mut etcd = fake.etcd_members.lock().expect("fake etcd");
            for ip in node_ips {
                etcd.insert((*ip).to_string(), true);
            }
        }
        *fake.api_ready.lock().expect("fake api") = true;
        Arc::new(fake)
    }

    pub fn with_api_nodes(self: &Arc<Self>, nodes: Vec<ListedNode>) -> Arc<Self> {
        *self.api_nodes.lock().expect("fake api nodes") = nodes;
        Arc::clone(self)
    }

    pub fn record(&self, address: &str, cmd: &str) {
        self.calls
            .lock()
            .expect("fake calls")
            .push((address.to_string(), cmd.to_string()));
    }

    fn check(&self, address: &str) -> Result<(), String> {
        if self
            .unreachable
            .lock()
            .expect("fake unreachable")
            .iter()
            .any(|a| a == address)
        {
            return Err(format!("unreachable: {address}"));
        }
        Ok(())
    }

    pub fn calls_named(&self, cmd: &str) -> Vec<String> {
        self.calls
            .lock()
            .expect("fake calls")
            .iter()
            .filter(|(_, c)| c == cmd || c.starts_with(&format!("{cmd}:")))
            .map(|(a, _)| a.clone())
            .collect()
    }
}

impl KubernetesNodes for Fake {
    fn apply_config<'a>(
        &'a self,
        address: &'a str,
        nix: &'a str,
        _kubeconfig: &'a str,
        secrets: &'a [SecretFile],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.check(address)?;
            if let Some(err) = self
                .apply_fail
                .lock()
                .expect("fake apply fail")
                .get(address)
                .cloned()
            {
                self.record(address, "apply:fail");
                return Err(err);
            }
            self.secrets
                .lock()
                .expect("fake secrets")
                .extend(secrets.iter().cloned());
            let id = format!("apply-{address}-{}", nix.len());
            self.applies
                .lock()
                .expect("fake applies")
                .insert(address.to_string(), id.clone());
            self.apply_status
                .lock()
                .expect("fake apply status")
                .insert(id.clone(), ApplyProgress::Succeeded);
            self.record(address, "apply");
            Ok(id)
        })
    }

    fn apply_status<'a>(
        &'a self,
        address: &'a str,
        apply_id: &'a str,
    ) -> BoxFuture<'a, (ApplyProgress, String)> {
        Box::pin(async move {
            if self.check(address).is_err() {
                return (
                    ApplyProgress::Running,
                    format!("node agent {address} unreachable"),
                );
            }
            let progress = self
                .apply_status
                .lock()
                .expect("fake apply status")
                .get(apply_id)
                .copied()
                .unwrap_or(ApplyProgress::Succeeded);
            (progress, String::new())
        })
    }

    fn kube<'a>(
        &'a self,
        address: &'a str,
        method: &'a str,
        path: &'a str,
        body: &'a str,
        _content_type: &'a str,
    ) -> BoxFuture<'a, Result<(i32, String), String>> {
        Box::pin(async move {
            self.check(address)?;
            self.record(address, &format!("kube:{method}:{path}"));
            if path == "/api/v1/nodes" && method == "GET" {
                let nodes = self.api_nodes.lock().expect("fake api nodes").clone();
                let items: Vec<serde_json::Value> = nodes
                    .iter()
                    .map(|n| {
                        let addresses: Vec<_> = n
                            .internal_ips
                            .iter()
                            .map(|ip| serde_json::json!({"type": "InternalIP", "address": ip}))
                            .collect();
                        serde_json::json!({
                            "metadata": { "name": n.name },
                            "status": {
                                "addresses": addresses,
                                "conditions": [{
                                    "type": "Ready",
                                    "status": if n.ready { "True" } else { "False" }
                                }],
                                "nodeInfo": { "kubeletVersion": n.kubelet_version },
                            }
                        })
                    })
                    .collect();
                return Ok((200, serde_json::json!({ "items": items }).to_string()));
            }
            if path.starts_with("/api/v1/nodes/") && method == "PATCH" {
                return Ok((200, body.to_string()));
            }
            if path.starts_with("/api/v1/nodes/") && method == "DELETE" {
                return Ok((200, String::new()));
            }
            if path.starts_with("/api/v1/pods") && method == "GET" {
                return Ok((200, r#"{"items":[]}"#.to_string()));
            }
            if path.contains("/eviction") {
                return Ok((201, String::new()));
            }
            Ok((200, String::new()))
        })
    }

    fn etcd_health<'a>(
        &'a self,
        ip: &'a str,
        _port: u16,
        _ca_pem: &'a str,
        _client_cert_pem: &'a str,
        _client_key_pem: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            self.record(ip, "etcd_health");
            Ok(*self
                .etcd_members
                .lock()
                .expect("fake etcd")
                .get(ip)
                .unwrap_or(&false))
        })
    }

    fn etcd_post<'a>(
        &'a self,
        ip: &'a str,
        _port: u16,
        path: &'a str,
        body: &'a str,
        _ca_pem: &'a str,
        _client_cert_pem: &'a str,
        _client_key_pem: &'a str,
    ) -> BoxFuture<'a, Result<(u16, String), String>> {
        Box::pin(async move {
            self.record(ip, &format!("etcd_post:{path}"));
            if path.ends_with("/member/list") {
                let members = self.etcd_members.lock().expect("fake etcd");
                let list: Vec<_> = members
                    .keys()
                    .enumerate()
                    .map(|(i, member_ip)| {
                        serde_json::json!({
                            "ID": format!("{:x}", i as u64 + 1),
                            "name": format!("m{i}"),
                            "peerURLs": [format!("https://{member_ip}:2380")],
                            "clientURLs": [format!("https://{member_ip}:2379")],
                        })
                    })
                    .collect();
                return Ok((200, serde_json::json!({ "members": list }).to_string()));
            }
            if path.ends_with("/member/add") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                    if let Some(urls) = v.get("peerURLs").and_then(|u| u.as_array()) {
                        if let Some(url) = urls.first().and_then(|u| u.as_str()) {
                            if let Some(host) = url.trim_start_matches("https://").split(':').next()
                            {
                                self.etcd_members
                                    .lock()
                                    .expect("fake etcd")
                                    .insert(host.to_string(), true);
                            }
                        }
                    }
                }
                return Ok((200, r#"{"members":[]}"#.to_string()));
            }
            if path.ends_with("/member/remove") {
                return Ok((200, r#"{}"#.to_string()));
            }
            Ok((200, r#"{}"#.to_string()))
        })
    }

    fn apiserver_ready<'a>(
        &'a self,
        addr: &'a str,
        _server_name: &'a str,
        _ca_pem: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            self.record(addr, "apiserver_ready");
            Ok(*self.api_ready.lock().expect("fake api"))
        })
    }

    fn etcd_snapshot<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            self.check(address)?;
            self.record(address, "etcd_snapshot");
            let bytes = format!("snapshot:{cluster}").into_bytes();
            self.snapshots
                .lock()
                .expect("fake snapshots")
                .insert(cluster.to_string(), bytes.clone());
            Ok(bytes)
        })
    }

    fn etcd_restore<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
        snapshot: &'a [u8],
        _initial_cluster: &'a str,
        _initial_cluster_token: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.check(address)?;
            self.record(address, "etcd_restore");
            let expected = self
                .snapshots
                .lock()
                .expect("fake snapshots")
                .get(cluster)
                .cloned();
            if let Some(expected) = expected {
                if expected != snapshot {
                    return Err("snapshot checksum mismatch".to_string());
                }
            }
            Ok(())
        })
    }

    fn write_secrets<'a>(
        &'a self,
        address: &'a str,
        secrets: &'a [SecretFile],
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.check(address)?;
            self.record(address, "write_secrets");
            self.secrets
                .lock()
                .expect("fake secrets")
                .extend(secrets.iter().cloned());
            Ok(())
        })
    }

    fn manage_units<'a>(
        &'a self,
        address: &'a str,
        units: &'a [String],
        action: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.check(address)?;
            self.record(address, &format!("manage_units:{action}"));
            self.units_restarted
                .lock()
                .expect("fake units")
                .push((address.to_string(), units.to_vec()));
            Ok(())
        })
    }

    fn wipe_etcd_data<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.check(address)?;
            self.record(address, &format!("wipe_etcd_data:{cluster}"));
            self.etcd_empty
                .lock()
                .expect("fake etcd empty")
                .insert(address.to_string());
            Ok(())
        })
    }

    fn etcd_data_empty<'a>(
        &'a self,
        address: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            self.check(address)?;
            self.record(address, &format!("etcd_data_empty:{cluster}"));
            Ok(self
                .etcd_empty
                .lock()
                .expect("fake etcd empty")
                .contains(address))
        })
    }
}

/// Helpers used by [`crate::kubernetes_admin`] when talking through a trait object.
pub async fn list_nodes(
    nodes: &dyn KubernetesNodes,
    address: &str,
) -> Result<Vec<ListedNode>, String> {
    let (status, body) = nodes.kube(address, "GET", "/api/v1/nodes", "", "").await?;
    if status != 200 {
        return Err(format!("list nodes returned {status}: {body}"));
    }
    Ok(nodes_from_list(&body))
}

pub async fn cordon(
    nodes: &dyn KubernetesNodes,
    address: &str,
    name: &str,
    unschedulable: bool,
) -> Result<(), String> {
    let name = api_name(name)?;
    let body = format!(r#"{{"spec":{{"unschedulable":{unschedulable}}}}}"#);
    let (status, response) = nodes
        .kube(
            address,
            "PATCH",
            &format!("/api/v1/nodes/{name}"),
            &body,
            "application/merge-patch+json",
        )
        .await?;
    if status != 200 {
        return Err(format!("cordon {name} returned {status}: {response}"));
    }
    Ok(())
}

pub async fn label_worker(
    nodes: &dyn KubernetesNodes,
    address: &str,
    name: &str,
) -> Result<(), String> {
    let name = api_name(name)?;
    let (status, response) = nodes
        .kube(
            address,
            "PATCH",
            &format!("/api/v1/nodes/{name}"),
            r#"{"metadata":{"labels":{"node-role.kubernetes.io/worker":""}}}"#,
            "application/merge-patch+json",
        )
        .await?;
    if status != 200 {
        return Err(format!("label {name} returned {status}: {response}"));
    }
    Ok(())
}

pub async fn delete_node(
    nodes: &dyn KubernetesNodes,
    address: &str,
    name: &str,
) -> Result<(), String> {
    let name = api_name(name)?;
    let (status, body) = nodes
        .kube(address, "DELETE", &format!("/api/v1/nodes/{name}"), "", "")
        .await?;
    if status != 200 && status != 202 && status != 404 {
        return Err(format!("delete node {name} returned {status}: {body}"));
    }
    Ok(())
}

pub async fn drain_node(
    nodes: &dyn KubernetesNodes,
    address: &str,
    name: &str,
) -> Result<(), String> {
    use std::time::{Duration, Instant};
    use tokio::time::sleep;

    cordon(nodes, address, name, true).await?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match remaining_pods(nodes, address, name).await {
            Ok(pods) if pods.is_empty() => return Ok(()),
            Ok(pods) => {
                for (namespace, pod) in pods {
                    evict_pod(nodes, address, &namespace, &pod).await?;
                }
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    let _ = cordon(nodes, address, name, false).await;
                    return Err(format!("drain timed out: {err}"));
                }
            }
        }
        if Instant::now() >= deadline {
            let _ = cordon(nodes, address, name, false).await;
            return Err("drain timed out after 300s".to_string());
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn remaining_pods(
    nodes: &dyn KubernetesNodes,
    address: &str,
    name: &str,
) -> Result<Vec<(String, String)>, String> {
    let name = api_name(name)?;
    let path = format!("/api/v1/pods?fieldSelector=spec.nodeName%3D{name}");
    let (status, body) = nodes.kube(address, "GET", &path, "", "").await?;
    if status != 200 {
        return Err(format!("list pods returned {status}: {body}"));
    }
    Ok(pods_to_evict(&pods_from_list(&body)))
}

async fn evict_pod(
    nodes: &dyn KubernetesNodes,
    address: &str,
    namespace: &str,
    pod: &str,
) -> Result<(), String> {
    let path = format!("/api/v1/namespaces/{namespace}/pods/{pod}/eviction");
    let body = format!(
        r#"{{"apiVersion":"policy/v1","kind":"Eviction","metadata":{{"name":"{pod}","namespace":"{namespace}"}}}}"#
    );
    let (status, response) = nodes
        .kube(address, "POST", &path, &body, "application/json")
        .await?;
    if status != 200 && status != 201 && status != 202 && status != 404 && status != 429 {
        return Err(format!("evict {pod} returned {status}: {response}"));
    }
    Ok(())
}

fn api_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty()
        || name.contains('/')
        || name.contains('?')
        || name.contains('#')
        || name.contains(' ')
    {
        return Err(format!("invalid kubernetes node name {name:?}"));
    }
    Ok(name.to_string())
}

pub fn registered_node_name(
    nodes: &[ListedNode],
    stored_name: &str,
    hostname: &str,
    ip: &str,
) -> Result<String, String> {
    match_registered_node(nodes, stored_name, hostname, ip)
        .map(|node| node.name.clone())
        .ok_or_else(|| format!("kubernetes has no node for hostname {hostname} address {ip}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_apply_succeeds_and_records_secrets() {
        let fake = Fake::new(&["10.0.0.1"]);
        let secrets = vec![SecretFile {
            path: "/var/lib/kcore/kubernetes/demo/pki/key.pem".into(),
            mode: 0o600,
            content: b"secret".to_vec(),
        }];
        let id = fake
            .apply_config("10.0.0.1:9091", "{ }", "", &secrets)
            .await
            .unwrap();
        let (progress, _) = fake.apply_status("10.0.0.1:9091", &id).await;
        assert_eq!(progress, ApplyProgress::Succeeded);
        assert_eq!(fake.secrets.lock().unwrap().len(), 1);
        assert_eq!(fake.calls_named("apply"), vec!["10.0.0.1:9091".to_string()]);
    }

    #[tokio::test]
    async fn fake_rejects_unreachable_apply() {
        let fake = Fake::new(&["10.0.0.1"]);
        fake.unreachable
            .lock()
            .unwrap()
            .push("10.0.0.1:9091".into());
        let err = fake
            .apply_config("10.0.0.1:9091", "{ }", "", &[])
            .await
            .unwrap_err();
        assert!(err.starts_with("unreachable:"));
    }

    #[tokio::test]
    async fn fake_snapshot_restore_rejects_mismatch() {
        let fake = Fake::new(&["10.0.0.1"]);
        let snap = fake.etcd_snapshot("10.0.0.1:9091", "demo").await.unwrap();
        let err = fake
            .etcd_restore("10.0.0.1:9091", "demo", b"wrong", "", "")
            .await
            .unwrap_err();
        assert!(err.contains("checksum"));
        fake.etcd_restore("10.0.0.1:9091", "demo", &snap, "", "")
            .await
            .unwrap();
    }
}
