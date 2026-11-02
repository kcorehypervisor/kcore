//! PowerDNS authoritative HTTP API.

use crate::render::ZonePlan;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub fn sync(base: &str, api_key: &str, zones: &[ZonePlan]) -> Result<()> {
    for zone in zones {
        match get(api_key, &zone_url(base, &zone.name)) {
            Ok(_) => {}
            Err(err) if err.to_string().contains("404") => {
                let body = json!({
                    "name": zone.name,
                    "kind": "Native",
                    "nameservers": [zone.nameserver],
                });
                request(
                    "POST",
                    &format!("{base}/api/v1/servers/localhost/zones"),
                    api_key,
                    Some(&body.to_string()),
                )?;
            }
            Err(err) => return Err(err),
        }
        let existing = get(api_key, &zone_url(base, &zone.name))?;
        let patch = diff_patch(&existing, zone);
        if patch["rrsets"]
            .as_array()
            .map(|r| !r.is_empty())
            .unwrap_or(false)
        {
            request(
                "PATCH",
                &zone_url(base, &zone.name),
                api_key,
                Some(&patch.to_string()),
            )?;
        }
    }
    Ok(())
}

fn diff_patch(existing: &Value, zone: &ZonePlan) -> Value {
    let mut rrsets = Vec::new();
    let current = existing
        .get("rrsets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for rrset in &zone.rrsets {
        rrsets.push(json!({
            "name": rrset.name,
            "type": rrset.rrtype,
            "ttl": rrset.ttl,
            "changetype": "REPLACE",
            "records": [{"content": rrset.content, "disabled": false}],
        }));
    }
    for have in &current {
        let name = have.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let rrtype = have.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(rrtype, "A" | "PTR" | "CNAME" | "AAAA") {
            continue;
        }
        let wanted = zone
            .rrsets
            .iter()
            .any(|r| r.name == name && r.rrtype == rrtype);
        if !wanted {
            rrsets.push(json!({
                "name": name,
                "type": rrtype,
                "changetype": "DELETE",
            }));
        }
    }
    json!({ "rrsets": rrsets })
}

fn zone_url(base: &str, name: &str) -> String {
    format!("{base}/api/v1/servers/localhost/zones/{name}")
}

fn get(api_key: &str, url: &str) -> Result<Value> {
    let text = request("GET", url, api_key, None)?;
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).with_context(|| format!("pdns json from {url}"))
}

fn request(method: &str, url: &str, api_key: &str, body: Option<&str>) -> Result<String> {
    let (host, port, path) = split_url(url)?;
    let mut stream = TcpStream::connect((host.as_str(), port))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let body = body.unwrap_or("");
    let mut headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nX-API-Key: {api_key}\r\nAccept: application/json\r\nConnection: close\r\n"
    );
    if !body.is_empty() {
        headers.push_str("Content-Type: application/json\r\n");
        headers.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body.as_bytes())?;
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (status, rest) = text
        .split_once("\r\n")
        .ok_or_else(|| anyhow!("short http response from {url}"))?;
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("bad http status {status}"))?;
    if !(200..300).contains(&code) {
        return Err(anyhow!("pdns {method} {path} returned {code}"));
    }
    let body = rest
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    Ok(decode_body(rest, body))
}

fn decode_body(headers_and_body: &str, body: &str) -> String {
    let headers = headers_and_body
        .split_once("\r\n\r\n")
        .map(|(h, _)| h)
        .unwrap_or("");
    let chunked = headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    if !chunked {
        return body.to_string();
    }
    let mut rest = body;
    let mut out = String::new();
    while !rest.is_empty() {
        let Some((size, after)) = rest.split_once("\r\n") else {
            break;
        };
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let take = size.min(after.len());
        out.push_str(&after[..take]);
        rest = after.get(take + 2..).unwrap_or("");
    }
    out
}

fn split_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("pdns url {url} must be http"))?;
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| anyhow!("pdns url {url} has no path"))?;
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        let port = port
            .parse()
            .map_err(|_| anyhow!("pdns port in {url} is invalid"))?;
        (host.to_string(), port)
    } else {
        (authority.to_string(), 80)
    };
    Ok((host, port, format!("/{path}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Rrset;

    #[test]
    fn patch_replaces_and_deletes() {
        let existing = json!({
            "rrsets": [
                {"name": "old.home.arpa.", "type": "A", "records": [{"content": "192.168.40.9"}]},
                {"name": "home.arpa.", "type": "SOA", "records": [{"content": "ns1"}]}
            ]
        });
        let zone = ZonePlan {
            name: "home.arpa.".into(),
            nameserver: "ns1.home.arpa.".into(),
            rrsets: vec![Rrset {
                name: "node.home.arpa.".into(),
                rrtype: "A".into(),
                ttl: 3600,
                content: "192.168.40.105".into(),
            }],
        };
        let patch = diff_patch(&existing, &zone);
        let sets = patch["rrsets"].as_array().unwrap();
        assert!(sets.iter().any(|r| r["changetype"] == "REPLACE"));
        assert!(sets
            .iter()
            .any(|r| r["name"] == "old.home.arpa." && r["changetype"] == "DELETE"));
        assert!(!sets.iter().any(|r| r["type"] == "SOA"));
    }
}
