//! Call the Kubernetes API with the admin kubeconfig on this node.
//!
//! The controller asks a control-plane node to do this. It does not run kubectl.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const ADMIN_KUBECONFIG: &str = "/etc/kubernetes/admin.conf";

struct AdminCreds {
    addr: String,
    server_name: String,
    ca_pem: String,
    cert_pem: String,
    key_pem: String,
}

pub async fn call_admin_conf(
    method: &str,
    path: &str,
    body: &str,
    content_type: &str,
) -> Result<(i32, String)> {
    let method = method.trim().to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "POST" | "PATCH" | "PUT" | "DELETE") {
        anyhow::bail!("kubernetes method {method} is not allowed");
    }
    validate_api_path(path)?;
    let text = tokio::fs::read_to_string(ADMIN_KUBECONFIG)
        .await
        .with_context(|| format!("read {ADMIN_KUBECONFIG}"))?;
    let creds = parse_admin_kubeconfig(&text)?;
    let content_type = if content_type.is_empty() {
        "application/json"
    } else {
        content_type
    };
    https_request(&creds, &method, path, body, content_type).await
}

fn validate_api_path(path: &str) -> Result<()> {
    if path.contains('\n') || path.contains('\r') || path.contains(' ') {
        anyhow::bail!("kubernetes path is not a single request target");
    }
    let bare = path.split('?').next().unwrap_or(path);
    if !(bare.starts_with("/api/") || bare.starts_with("/apis/")) || bare.contains("..") {
        anyhow::bail!("kubernetes path must stay under /api or /apis");
    }
    Ok(())
}

fn parse_admin_kubeconfig(text: &str) -> Result<AdminCreds> {
    let value: serde_yaml::Value = serde_yaml::from_str(text).context("admin kubeconfig")?;
    let cluster = value
        .get("clusters")
        .and_then(serde_yaml::Value::as_sequence)
        .and_then(|items| items.first())
        .and_then(|item| item.get("cluster"))
        .context("admin kubeconfig has no cluster")?;
    let user = value
        .get("users")
        .and_then(serde_yaml::Value::as_sequence)
        .and_then(|items| items.first())
        .and_then(|item| item.get("user"))
        .context("admin kubeconfig has no user")?;
    let server = cluster
        .get("server")
        .and_then(serde_yaml::Value::as_str)
        .context("admin kubeconfig has no server")?
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    let server_name = server.split(':').next().unwrap_or("").to_string();
    if server_name.is_empty() {
        anyhow::bail!("admin kubeconfig server has no host");
    }
    let ca_pem = decode_field(cluster, "certificate-authority-data")?;
    let cert_pem = decode_field(user, "client-certificate-data")?;
    let key_pem = decode_field(user, "client-key-data")?;
    Ok(AdminCreds {
        addr: server,
        server_name,
        ca_pem,
        cert_pem,
        key_pem,
    })
}

fn decode_field(value: &serde_yaml::Value, key: &str) -> Result<String> {
    let raw = value
        .get(key)
        .and_then(serde_yaml::Value::as_str)
        .with_context(|| format!("admin kubeconfig missing {key}"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .with_context(|| format!("decode {key}"))?;
    String::from_utf8(bytes).with_context(|| format!("{key} is not utf-8 pem"))
}

async fn https_request(
    creds: &AdminCreds,
    method: &str,
    path: &str,
    body: &str,
    content_type: &str,
) -> Result<(i32, String)> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in pem_certs(&creds.ca_pem)? {
        roots.add(cert).context("kubernetes CA")?;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(pem_certs(&creds.cert_pem)?, pem_key(&creds.key_pem)?)
        .context("admin client certificate")?;
    let name = ServerName::try_from(creds.server_name.clone()).context("api server name")?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&creds.addr))
        .await
        .context("connect timed out")?
        .with_context(|| format!("connect {}", creds.addr))?;
    let mut tls = connector
        .connect(name, stream)
        .await
        .context("tls handshake")?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n",
        creds.server_name
    );
    if !body.is_empty() || matches!(method, "POST" | "PATCH" | "PUT" | "DELETE") {
        request.push_str(&format!(
            "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("\r\n");
    request.push_str(body);
    tls.write_all(request.as_bytes())
        .await
        .context("write api request")?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), tls.read_to_end(&mut buf))
        .await
        .context("read timed out")?
        .context("read api response")?;
    parse_http_response(&buf)
}

fn parse_http_response(bytes: &[u8]) -> Result<(i32, String)> {
    let text = String::from_utf8_lossy(bytes);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        decode_chunked(body)?
    } else {
        body.to_string()
    };
    Ok((status, body))
}

fn decode_chunked(body: &str) -> Result<String> {
    let mut rest = body;
    let mut out = String::new();
    loop {
        let Some((size_line, after)) = rest.split_once("\r\n") else {
            break;
        };
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| anyhow!("bad chunk size"))?;
        if size == 0 {
            break;
        }
        if after.len() < size {
            anyhow::bail!("short chunk");
        }
        out.push_str(&after[..size]);
        rest = after.get(size + 2..).unwrap_or("");
    }
    Ok(out)
}

fn pem_certs(pem_str: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut certs = Vec::new();
    for block in pem::parse_many(pem_str).context("parsing certificate pem")? {
        if block.tag() == "CERTIFICATE" {
            certs.push(CertificateDer::from(block.contents().to_vec()));
        }
    }
    if certs.is_empty() {
        anyhow::bail!("no certificate in pem");
    }
    Ok(certs)
}

fn pem_key(pem_str: &str) -> Result<PrivateKeyDer<'static>> {
    use rustls::pki_types::{PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer};
    let block = pem::parse(pem_str).context("parsing private key pem")?;
    let bytes = block.contents().to_vec();
    match block.tag() {
        "PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bytes))),
        "EC PRIVATE KEY" => Ok(PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(bytes))),
        "RSA PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(bytes))),
        other => anyhow::bail!("unsupported private key type {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubeconfig_yields_the_api_server_and_pem() {
        let ca = base64::engine::general_purpose::STANDARD.encode("CA-PEM");
        let cert = base64::engine::general_purpose::STANDARD.encode("CERT-PEM");
        let key = base64::engine::general_purpose::STANDARD.encode("KEY-PEM");
        let text = format!(
            "clusters:\n- cluster:\n    certificate-authority-data: {ca}\n    server: https://192.168.40.200:6443\nusers:\n- user:\n    client-certificate-data: {cert}\n    client-key-data: {key}\n"
        );
        let creds = parse_admin_kubeconfig(&text).unwrap();
        assert_eq!(creds.addr, "192.168.40.200:6443");
        assert_eq!(creds.ca_pem, "CA-PEM");
        assert_eq!(creds.cert_pem, "CERT-PEM");
    }

    #[test]
    fn chunked_body_is_joined_and_paths_stay_on_the_api() {
        let (status, body) = parse_http_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "hello");
        assert!(validate_api_path("/api/v1/nodes").is_ok());
        assert!(validate_api_path("/apis/policy/v1/namespaces/default/pods/web/eviction").is_ok());
        assert!(validate_api_path("/healthz").is_err());
        assert!(validate_api_path("/api/../etc/passwd").is_err());
    }
}
