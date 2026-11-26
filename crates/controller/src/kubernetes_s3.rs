//! Minimal S3 PutObject / DeleteObject with SigV4 over HTTPS.
//!
//! Credentials JSON: `{"access_key","secret_key","endpoint"?,"region"?}`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use aws_lc_rs::{digest, hmac};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct S3Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub endpoint: String,
    pub region: String,
}

impl S3Credentials {
    pub fn from_json(raw: &str, endpoint_override: &str) -> Result<Self> {
        let v: serde_json::Value = serde_json::from_str(raw).context("parse s3 credentials")?;
        let access_key = v
            .get("access_key")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("credentials missing access_key"))?
            .to_string();
        let secret_key = v
            .get("secret_key")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("credentials missing secret_key"))?
            .to_string();
        let endpoint = if endpoint_override.is_empty() {
            v.get("endpoint")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            endpoint_override.to_string()
        };
        let region = v
            .get("region")
            .and_then(|x| x.as_str())
            .unwrap_or("us-east-1")
            .to_string();
        Ok(Self {
            access_key,
            secret_key,
            endpoint,
            region,
        })
    }
}

pub fn object_key(prefix: &str, id: &str) -> String {
    if prefix.is_empty() {
        format!("{id}.db")
    } else {
        format!("{}/{id}.db", prefix.trim_matches('/'))
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    hex_encode(digest::digest(&digest::SHA256, data).as_ref())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&k, data).as_ref().to_vec()
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

struct HostPort {
    host: String,
    port: u16,
    https: bool,
    host_header: String,
}

fn parse_endpoint(endpoint: &str, bucket: &str) -> Result<HostPort> {
    if endpoint.is_empty() {
        // Virtual-hosted–style AWS endpoint.
        return Ok(HostPort {
            host: format!("{bucket}.s3.amazonaws.com"),
            port: 443,
            https: true,
            host_header: format!("{bucket}.s3.amazonaws.com"),
        });
    }
    let url = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("https://{endpoint}")
    };
    let parsed = url::Url::parse(&url).context("s3 endpoint url")?;
    let https = parsed.scheme() != "http";
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("endpoint has no host"))?
        .to_string();
    let port = parsed.port().unwrap_or(if https { 443 } else { 80 });
    Ok(HostPort {
        host_header: if parsed.port().is_some() {
            format!("{host}:{port}")
        } else {
            host.clone()
        },
        host,
        port,
        https,
    })
}

fn amz_dates() -> Result<(String, String)> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("{e}"))?
        .as_secs() as i64;
    let odt = time::OffsetDateTime::from_unix_timestamp(secs).map_err(|e| anyhow!("{e}"))?;
    let amz = odt
        .format(&time::format_description::parse(
            "[year][month][day]T[hour][minute][second]Z",
        )?)
        .map_err(|e| anyhow!("{e}"))?;
    let date = odt
        .format(&time::format_description::parse("[year][month][day]")?)
        .map_err(|e| anyhow!("{e}"))?;
    Ok((amz, date))
}

async fn http_exchange(hp: &HostPort, request: &str, body: &[u8]) -> Result<(u16, String)> {
    let addr = format!("{}:{}", hp.host, hp.port);
    let stream = tokio::net::TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let mut buf = Vec::new();
    if hp.https {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let name = ServerName::try_from(hp.host.clone()).context("server name")?;
        let mut tls = connector.connect(name, stream).await.context("tls")?;
        tls.write_all(request.as_bytes()).await?;
        if !body.is_empty() {
            tls.write_all(body).await?;
        }
        tls.read_to_end(&mut buf).await?;
    } else {
        let mut stream = stream;
        stream.write_all(request.as_bytes()).await?;
        if !body.is_empty() {
            stream.write_all(body).await?;
        }
        stream.read_to_end(&mut buf).await?;
    }
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, text.into_owned()))
}

fn authorize(
    creds: &S3Credentials,
    method: &str,
    canonical_uri: &str,
    host_header: &str,
    payload_hash: &str,
    amz: &str,
    date: &str,
) -> String {
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers =
        format!("host:{host_header}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz}\n");
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{date}/{}/s3/aws4_request", creds.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let key = signing_key(&creds.secret_key, date, &creds.region, "s3");
    let signature = hex_encode(&hmac_sha256(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    )
}

/// PUT object body to `s3://bucket/key`.
pub async fn put_object(creds: &S3Credentials, bucket: &str, key: &str, body: &[u8]) -> Result<()> {
    let hp = parse_endpoint(&creds.endpoint, bucket)?;
    let (amz, date) = amz_dates()?;
    let payload_hash = sha256_hex(body);
    // Path-style for custom endpoints; virtual-hosted for empty endpoint.
    let (canonical_uri, path) = if creds.endpoint.is_empty() {
        (format!("/{key}"), format!("/{key}"))
    } else {
        (format!("/{bucket}/{key}"), format!("/{bucket}/{key}"))
    };
    let auth = authorize(
        creds,
        "PUT",
        &canonical_uri,
        &hp.host_header,
        &payload_hash,
        &amz,
        &date,
    );
    let request = format!(
        "PUT {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nx-amz-content-sha256: {payload_hash}\r\nx-amz-date: {amz}\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n",
        hp.host_header,
        body.len(),
    );
    let (status, resp) = http_exchange(&hp, &request, body).await?;
    if !(200..300).contains(&status) {
        anyhow::bail!("s3 put {bucket}/{key} returned {status}: {resp}");
    }
    Ok(())
}

/// DELETE object at `s3://bucket/key`. Missing objects (404) are success.
pub async fn delete_object(creds: &S3Credentials, bucket: &str, key: &str) -> Result<()> {
    let hp = parse_endpoint(&creds.endpoint, bucket)?;
    let (amz, date) = amz_dates()?;
    let payload_hash = sha256_hex(b"");
    let (canonical_uri, path) = if creds.endpoint.is_empty() {
        (format!("/{key}"), format!("/{key}"))
    } else {
        (format!("/{bucket}/{key}"), format!("/{bucket}/{key}"))
    };
    let auth = authorize(
        creds,
        "DELETE",
        &canonical_uri,
        &hp.host_header,
        &payload_hash,
        &amz,
        &date,
    );
    let request = format!(
        "DELETE {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nx-amz-content-sha256: {payload_hash}\r\nx-amz-date: {amz}\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n",
        hp.host_header,
    );
    let (status, resp) = http_exchange(&hp, &request, b"").await?;
    if status == 404 || (200..300).contains(&status) {
        return Ok(());
    }
    anyhow::bail!("s3 delete {bucket}/{key} returned {status}: {resp}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_joins_prefix() {
        assert_eq!(object_key("", "snap"), "snap.db");
        assert_eq!(object_key("etcd/", "snap"), "etcd/snap.db");
    }

    #[test]
    fn credentials_require_keys() {
        assert!(S3Credentials::from_json(r#"{"access_key":"a"}"#, "").is_err());
        let c = S3Credentials::from_json(
            r#"{"access_key":"a","secret_key":"b","region":"eu-west-1"}"#,
            "",
        )
        .unwrap();
        assert_eq!(c.region, "eu-west-1");
    }
}
