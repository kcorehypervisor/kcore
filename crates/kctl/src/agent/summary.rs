//! The one-line description of the cluster the chat header shows.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::ConnectionInfo;

pub fn default_home() -> PathBuf {
    if let Ok(home) = std::env::var("KCORE_AGENT_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home.trim());
        }
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".kcore")
        .join("agent")
}

fn operator_name(info: &ConnectionInfo) -> Option<String> {
    let pem = match (&info.cert_pem, &info.cert) {
        (Some(pem), _) => pem.clone(),
        (None, Some(path)) => std::fs::read_to_string(path).ok()?,
        _ => return None,
    };
    let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).ok()?;
    let cert = parsed.parse_x509().ok()?;
    let name = cert
        .subject()
        .iter_common_name()
        .next()?
        .as_str()
        .ok()?
        .to_string();
    Some(name)
}

pub fn cluster_summary(info: &ConnectionInfo) -> Value {
    json!({
        "controller": info.address,
        "operator": operator_name(info),
        "insecure": info.insecure,
    })
}
