//! Ingress gateway support: loading Caddy configuration through its admin
//! socket, reporting certificate and upstream state, writing secrets, and
//! shipping Coraza audit events to the controller.
//!
//! Nix owns the `kcore-ingress` service (see `modules/kcore-ingress.nix`); the
//! controller owns the routing table. This module is the hinge between them:
//! it never starts Caddy, it only loads configuration into a running one and
//! remembers the last configuration that Caddy accepted, which is what the
//! unit boots from.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aws_lc_rs::digest;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::client::legacy::Client as HyperClient;
use hyperlocal::{UnixClientExt, Uri};
use serde_json::Value;
use tracing::{debug, warn};

use crate::config::Config;
use crate::controller_proto;
use crate::proto;

const WAF_TAIL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_EVENTS_PER_BATCH: usize = 500;
/// A single audit line larger than this is skipped rather than buffered.
const MAX_LINE_BYTES: usize = 1 << 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct IngressPaths {
    pub admin_socket: PathBuf,
    pub state_dir: PathBuf,
    pub secrets_dir: PathBuf,
    pub waf_log: PathBuf,
}

impl Default for IngressPaths {
    fn default() -> Self {
        Self {
            admin_socket: "/run/kcore-ingress/admin.sock".into(),
            state_dir: "/var/lib/kcore/ingress".into(),
            secrets_dir: "/etc/kcore/ingress".into(),
            waf_log: "/var/log/kcore/ingress/waf.jsonl".into(),
        }
    }
}

impl IngressPaths {
    fn config_file(&self) -> PathBuf {
        self.state_dir.join("config.json")
    }
    fn hash_file(&self) -> PathBuf {
        self.state_dir.join("applied.hash")
    }
    fn cert_root(&self) -> PathBuf {
        self.state_dir.join("certmagic").join("certificates")
    }
    fn waf_offset_file(&self) -> PathBuf {
        self.state_dir.join("waf.offset")
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    digest::digest(&digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------- admin API

async fn admin_request(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<(u16, Vec<u8>), String> {
    let client = HyperClient::unix();
    let uri = Uri::new(socket, path);
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        // Caddy rejects an Origin it does not know; the agent sends none.
        .header(hyper::header::HOST, "localhost");
    let fut = async {
        let resp = match body {
            Some(b) => {
                let req = builder
                    .header(hyper::header::CONTENT_TYPE, "application/json")
                    .body(Full::new(Bytes::from(b)))
                    .map_err(|e| format!("build request: {e}"))?;
                client.request(req).await
            }
            None => {
                let req = builder
                    .body(Full::new(Bytes::new()))
                    .map_err(|e| format!("build request: {e}"))?;
                client.request(req).await
            }
        }
        .map_err(|e| format!("admin request {path}: {e}"))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("read admin response: {e}"))?
            .to_bytes();
        Ok::<_, String>((status, bytes.to_vec()))
    };
    tokio::time::timeout(REQUEST_TIMEOUT, fut)
        .await
        .map_err(|_| format!("admin request {path} timed out"))?
}

/// Loads `config_json` into the running Caddy and records it as the config to
/// boot from. Returns the hash now applied.
pub async fn apply_config(
    paths: &IngressPaths,
    config_json: &str,
    expected_hash: &str,
) -> Result<String, String> {
    let actual = sha256_hex(config_json.as_bytes());
    if !expected_hash.is_empty() && actual != expected_hash {
        return Err(format!(
            "hash mismatch: controller sent {expected_hash}, payload is {actual}"
        ));
    }
    serde_json::from_str::<Value>(config_json).map_err(|e| format!("config is not JSON: {e}"))?;
    let (status, body) = admin_request(
        &paths.admin_socket,
        Method::POST,
        "/load",
        Some(config_json.as_bytes().to_vec()),
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "caddy refused the configuration ({status}): {}",
            String::from_utf8_lossy(&body).trim()
        ));
    }
    // Only a configuration Caddy accepted is remembered as the boot config.
    write_atomic(&paths.config_file(), config_json.as_bytes(), 0o644)?;
    write_atomic(&paths.hash_file(), actual.as_bytes(), 0o644)?;
    Ok(actual)
}

fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().ok_or("path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| format!("open {}: {e}", tmp.display()))?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))
}

// ------------------------------------------------------------------- status

#[derive(Debug, Default)]
pub struct IngressStatus {
    pub running: bool,
    pub applied_hash: String,
    pub vip_holder: String,
    pub certs: Vec<proto::IngressCertInfo>,
    pub upstreams: Vec<proto::IngressUpstreamInfo>,
    pub message: String,
}

pub async fn status(paths: &IngressPaths) -> IngressStatus {
    let mut out = IngressStatus::default();
    let config = match admin_request(&paths.admin_socket, Method::GET, "/config/", None).await {
        Ok((200, body)) => body,
        Ok((code, _)) => {
            out.message = format!("caddy admin answered {code}");
            return out;
        }
        Err(e) => {
            out.message = format!("ingress service is not reachable: {e}");
            return out;
        }
    };
    out.running = true;
    // The hash file records what Caddy accepted; trust it only while the live
    // config still looks like ours (a hand-edited or restarted Caddy would
    // report the bootstrap config, which has no routes).
    out.applied_hash = std::fs::read_to_string(paths.hash_file())
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    if !live_config_has_servers(&config) {
        out.applied_hash.clear();
    }
    out.vip_holder = first_listen_ip(&config)
        .filter(|ip| is_local_address(ip))
        .unwrap_or_default();
    out.certs = scan_certs(&paths.cert_root());
    if let Ok((200, body)) = admin_request(
        &paths.admin_socket,
        Method::GET,
        "/reverse_proxy/upstreams",
        None,
    )
    .await
    {
        out.upstreams = parse_upstreams(&body);
    }
    out
}

fn live_config_has_servers(config: &[u8]) -> bool {
    serde_json::from_slice::<Value>(config)
        .ok()
        .map(|v| v["apps"]["http"]["servers"]["kcore"].is_object())
        .unwrap_or(false)
}

fn first_listen_ip(config: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(config).ok()?;
    let listen = v["apps"]["http"]["servers"]["kcore"]["listen"].as_array()?;
    let first = listen.first()?.as_str()?;
    let host = first.rsplit_once(':')?.0;
    Some(host.trim_matches(|c| c == '[' || c == ']').to_string())
}

/// True when `ip` is configured on a local interface (IPv4 via the kernel FIB,
/// IPv6 via `if_inet6`). Used to tell which node currently holds the VIP.
fn is_local_address(ip: &str) -> bool {
    if ip.contains(':') {
        let want = ip
            .parse::<std::net::Ipv6Addr>()
            .map(|a| {
                a.octets()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            })
            .unwrap_or_default();
        return std::fs::read_to_string("/proc/net/if_inet6")
            .map(|t| {
                t.lines()
                    .any(|l| l.split_whitespace().next() == Some(want.as_str()))
            })
            .unwrap_or(false);
    }
    let Ok(table) = std::fs::read_to_string("/proc/net/fib_trie") else {
        return false;
    };
    fib_trie_has_local(&table, ip)
}

fn fib_trie_has_local(table: &str, ip: &str) -> bool {
    let mut lines = table.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim_end().ends_with(&format!("|-- {ip}")) {
            if let Some(next) = lines.peek() {
                if next.contains("/32 host LOCAL") {
                    return true;
                }
            }
        }
    }
    false
}

fn parse_upstreams(body: &[u8]) -> Vec<proto::IngressUpstreamInfo> {
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|u| {
            let address = u["address"].as_str()?.to_string();
            // Caddy counts failed health checks and passive failures in `fails`.
            let healthy = u["fails"].as_i64().unwrap_or(0) == 0;
            Some(proto::IngressUpstreamInfo { address, healthy })
        })
        .collect()
}

fn scan_certs(root: &Path) -> Vec<proto::IngressCertInfo> {
    let mut out = Vec::new();
    let Ok(issuers) = std::fs::read_dir(root) else {
        return out;
    };
    for issuer in issuers.filter_map(Result::ok) {
        let issuer_name = issuer.file_name().to_string_lossy().to_string();
        let Ok(hosts) = std::fs::read_dir(issuer.path()) else {
            continue;
        };
        for host in hosts.filter_map(Result::ok) {
            let host_name = host.file_name().to_string_lossy().to_string();
            let crt = host.path().join(format!("{host_name}.crt"));
            let Ok(pem_text) = std::fs::read(&crt) else {
                continue;
            };
            if let Some(not_after) = cert_not_after(&pem_text) {
                out.push(proto::IngressCertInfo {
                    host: host_name,
                    issuer: issuer_name.clone(),
                    not_after,
                });
            }
        }
    }
    out.sort_by(|a, b| a.host.cmp(&b.host).then(a.issuer.cmp(&b.issuer)));
    out
}

fn cert_not_after(pem_bytes: &[u8]) -> Option<String> {
    let block = pem::parse(pem_bytes).ok()?;
    let (_, cert) = x509_parser::parse_x509_certificate(block.contents()).ok()?;
    let t = cert.validity().not_after.to_datetime();
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    ))
}

// --------------------------------------------------------- certificate store

const MAX_STORE_BYTES: usize = 8 << 20;
const MAX_STORE_FILES: usize = 2048;
/// Top-level directories of the CertMagic store that are replicated. Locks are
/// node-local and never copied.
const STORE_DIRS: [&str; 3] = ["certificates", "acme", "ocsp"];

#[derive(serde::Serialize, serde::Deserialize)]
struct StoreFile {
    path: String,
    mode: u32,
    data: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoreArchive {
    files: Vec<StoreFile>,
}

fn store_root(paths: &IngressPaths) -> PathBuf {
    paths.state_dir.join("certmagic")
}

/// Packs the replicated parts of the certificate store.
pub fn export_store(paths: &IngressPaths) -> Result<(Vec<u8>, String, usize), String> {
    use base64::Engine;
    use std::os::unix::fs::PermissionsExt;
    let root = store_root(paths);
    let mut files = Vec::new();
    let mut total = 0usize;
    for top in STORE_DIRS {
        let mut stack = vec![root.join(top)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_file() {
                    let data = std::fs::read(&path)
                        .map_err(|e| format!("read {}: {e}", path.display()))?;
                    total += data.len();
                    if total > MAX_STORE_BYTES || files.len() >= MAX_STORE_FILES {
                        return Err("certificate store is too large to replicate".into());
                    }
                    let rel = path
                        .strip_prefix(&root)
                        .map_err(|_| "path outside the store".to_string())?
                        .to_string_lossy()
                        .to_string();
                    files.push(StoreFile {
                        path: rel,
                        mode: meta.permissions().mode() & 0o777,
                        data: base64::engine::general_purpose::STANDARD.encode(data),
                    });
                }
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let n = files.len();
    let archive = serde_json::to_vec(&StoreArchive { files }).map_err(|e| e.to_string())?;
    let digest = sha256_hex(&archive);
    Ok((archive, digest, n))
}

/// A relative path made only of normal components under a replicated directory.
fn safe_store_path(rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    let p = Path::new(rel);
    let mut comps = p.components();
    let first = match comps.next()? {
        Component::Normal(c) => c.to_str()?,
        _ => return None,
    };
    if !STORE_DIRS.contains(&first) {
        return None;
    }
    if p.components().any(|c| !matches!(c, Component::Normal(_))) || p.components().count() < 2 {
        return None;
    }
    // `components` drops `.` and doubled separators; require the path to be
    // exactly what it normalises to.
    let normal: PathBuf = p.components().collect();
    if normal.to_str() != Some(rel) {
        return None;
    }
    Some(p.to_path_buf())
}

/// Hands `path` to the owner of the store directory (the service's dynamic
/// user), so Caddy can read keys the agent wrote as root.
fn chown_like(path: &Path, owner_of: &Path) {
    use std::os::unix::fs::MetadataExt;
    if let Ok(meta) = std::fs::metadata(owner_of) {
        if let Ok(c) = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()) {
            // SAFETY: `c` is a valid NUL-terminated path for the call.
            unsafe { libc::chown(c.as_ptr(), meta.uid(), meta.gid()) };
        }
    }
}

/// Writes an exported store into this node's store. Existing files with other
/// names are left alone.
pub fn import_store(paths: &IngressPaths, archive: &[u8], digest: &str) -> Result<usize, String> {
    use base64::Engine;
    if archive.len() > MAX_STORE_BYTES * 2 {
        return Err("archive is too large".into());
    }
    if sha256_hex(archive) != digest {
        return Err("archive digest mismatch".into());
    }
    let parsed: StoreArchive =
        serde_json::from_slice(archive).map_err(|e| format!("archive is not valid: {e}"))?;
    if parsed.files.len() > MAX_STORE_FILES {
        return Err("archive has too many files".into());
    }
    let root = store_root(paths);
    std::fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    chown_like(&root, &paths.state_dir);
    let mut written = 0;
    for f in &parsed.files {
        let rel = safe_store_path(&f.path)
            .ok_or_else(|| format!("unsafe path {:?} in archive", f.path))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(&f.data)
            .map_err(|e| format!("{}: {e}", f.path))?;
        let dest = root.join(&rel);
        // Create and hand over every missing directory on the way.
        let mut dir = root.clone();
        for comp in rel.parent().into_iter().flat_map(|p| p.components()) {
            dir.push(comp);
            if !dir.exists() {
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("create {}: {e}", dir.display()))?;
                chown_like(&dir, &root);
            }
        }
        // Keys stay private; everything else is world-readable like Caddy writes it.
        let mode = if f.mode & 0o077 == 0 { 0o600 } else { 0o644 };
        write_atomic(&dest, &data, mode)?;
        chown_like(&dest, &root);
        written += 1;
    }
    Ok(written)
}

/// Loads the stored configuration again so Caddy re-reads certificates that
/// were just written to its storage. A node with no stored config has nothing
/// to reload.
pub async fn reload_stored_config(paths: &IngressPaths) -> Result<(), String> {
    let Ok(config) = std::fs::read(paths.config_file()) else {
        return Ok(());
    };
    let (status, body) =
        admin_request(&paths.admin_socket, Method::POST, "/load", Some(config)).await?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!(
            "reload answered {status}: {}",
            String::from_utf8_lossy(&body).trim()
        ))
    }
}

// ------------------------------------------------------------------ secrets

/// Writes `name` under the secrets directory with mode 0600. The name must be
/// a plain file name: no separators, no leading dot.
pub fn write_secret(paths: &IngressPaths, name: &str, content: &[u8]) -> Result<PathBuf, String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(format!("{name:?} is not a plain file name"));
    }
    let path = paths.secrets_dir.join(name);
    write_atomic(&path, content, 0o600)?;
    Ok(path)
}

// --------------------------------------------------------------- WAF events

/// Extracts one event per matched rule from a Coraza JSON audit line.
pub fn parse_waf_line(line: &str) -> Vec<controller_proto::WafEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let tx = &v["transaction"];
    let host = tx["request"]["headers"]
        .as_object()
        .and_then(|h| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("host"))
                .map(|(_, v)| v)
        })
        .and_then(|v| v.as_array().and_then(|a| a.first()).or(Some(v)))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let blocked = tx["is_interrupted"].as_bool().unwrap_or(false);
    let created_at = tx["timestamp"].as_str().and_then(parse_rfc3339_unix);
    let messages = v["messages"].as_array().cloned().unwrap_or_default();
    messages
        .iter()
        .filter_map(|m| {
            let rule_id = m["data"]["id"].as_i64().filter(|id| *id > 0)? as i32;
            Some(controller_proto::WafEvent {
                id: 0,
                gateway: String::new(),
                route: String::new(),
                node_id: String::new(),
                rule_id,
                severity: m["data"]["severity"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| m["data"]["severity"].as_i64().map(|n| n.to_string()))
                    .unwrap_or_default(),
                client_ip: tx["client_ip"].as_str().unwrap_or_default().to_string(),
                host: host.clone(),
                uri: tx["request"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                action: if blocked {
                    "blocked".into()
                } else {
                    "detected".into()
                },
                tx_id: tx["id"].as_str().unwrap_or_default().to_string(),
                raw_json: m.to_string(),
                created_at: created_at.map(|seconds| prost_types::Timestamp { seconds, nanos: 0 }),
            })
        })
        .collect()
}

/// Seconds since the epoch for `YYYY-MM-DDTHH:MM:SS[.frac](Z|±hh:mm)`.
fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    let format = time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(s, &format)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// Complete lines in `data` and the number of bytes they cover. A trailing
/// partial line is left for the next read.
fn complete_lines(data: &[u8]) -> (Vec<&str>, usize) {
    let mut lines = Vec::new();
    let mut consumed = 0;
    for chunk in data.split_inclusive(|b| *b == b'\n') {
        if chunk.last() != Some(&b'\n') {
            break;
        }
        consumed += chunk.len();
        if chunk.len() > MAX_LINE_BYTES {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(chunk) {
            let s = s.trim();
            if !s.is_empty() {
                lines.push(s);
            }
        }
    }
    (lines, consumed)
}

/// Reads new events past the stored offset. Returns the events and the offset
/// to store once they have been delivered.
pub fn read_new_waf_events(
    paths: &IngressPaths,
) -> Result<(Vec<controller_proto::WafEvent>, WafCursor), String> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::MetadataExt;
    let stored = WafCursor::load(&paths.waf_offset_file());
    let mut file = match std::fs::File::open(&paths.waf_log) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), WafCursor::default()))
        }
        Err(e) => return Err(format!("open {}: {e}", paths.waf_log.display())),
    };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    // A different file (rotated) or a shorter one (truncated): start over.
    let start = if meta.ino() != stored.inode || meta.len() < stored.offset {
        0
    } else {
        stored.offset
    };
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.take(8 << 20)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    let (lines, consumed) = complete_lines(&buf);
    let events: Vec<_> = lines.into_iter().flat_map(parse_waf_line).collect();
    Ok((
        events,
        WafCursor {
            inode: meta.ino(),
            offset: start + consumed as u64,
        },
    ))
}

/// Where the tailer stopped: the file identity and the byte offset in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WafCursor {
    inode: u64,
    offset: u64,
}

impl WafCursor {
    fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut it = text.split_whitespace().map(|n| n.parse::<u64>().ok());
        match (it.next().flatten(), it.next().flatten()) {
            (Some(inode), Some(offset)) => Self { inode, offset },
            _ => Self::default(),
        }
    }

    fn store(&self, path: &Path) -> Result<(), String> {
        write_atomic(
            path,
            format!("{} {}", self.inode, self.offset).as_bytes(),
            0o644,
        )
    }
}

pub fn spawn_waf_tailer(cfg: Config) {
    tokio::spawn(async move {
        let paths = IngressPaths::default();
        let node_id = cfg.node_id.clone();
        loop {
            tokio::time::sleep(WAF_TAIL_INTERVAL).await;
            if !paths.waf_log.exists() {
                continue;
            }
            let (events, new_offset) = match read_new_waf_events(&paths) {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "reading the WAF audit log failed");
                    continue;
                }
            };
            if events.is_empty() {
                // Nothing to ship, but still move past blank or unparsable lines.
                if let Err(e) = new_offset.store(&paths.waf_offset_file()) {
                    debug!(error = %e, "storing WAF offset failed");
                }
                continue;
            }
            let mut delivered = true;
            for batch in events.chunks(MAX_EVENTS_PER_BATCH) {
                if let Err(e) = report(&cfg, &node_id, batch.to_vec()).await {
                    warn!(error = %e, "reporting WAF events failed; will retry");
                    delivered = false;
                    break;
                }
            }
            if delivered {
                if let Err(e) = new_offset.store(&paths.waf_offset_file()) {
                    warn!(error = %e, "storing WAF offset failed");
                }
            }
        }
    });
}

async fn report(
    cfg: &Config,
    node_id: &str,
    events: Vec<controller_proto::WafEvent>,
) -> Result<(), String> {
    let mut last = String::from("no controller endpoint configured");
    for endpoint in crate::registration::controller_endpoints(cfg) {
        let channel = match crate::registration::connect_channel(cfg, &endpoint).await {
            Ok(c) => c,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        let mut client = controller_proto::controller_client::ControllerClient::new(channel);
        match client
            .report_waf_events(controller_proto::ReportWafEventsRequest {
                node_id: node_id.to_string(),
                gateway: String::new(),
                events: events.clone(),
            })
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => last = e.message().to_string(),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"transaction":{"timestamp":"2026-10-26T12:00:01Z","id":"abc123","client_ip":"198.51.100.7","is_interrupted":true,"request":{"uri":"/?q=<script>","headers":{"Host":["Shop.Example.com:443"]}}},"messages":[{"message":"XSS","data":{"id":941100,"severity":"critical"}},{"message":"Anomaly","data":{"id":949110,"severity":2}},{"message":"no id","data":{}}]}"#;

    #[test]
    fn parses_one_event_per_rule() {
        let ev = parse_waf_line(LINE);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].rule_id, 941100);
        assert_eq!(ev[0].severity, "critical");
        assert_eq!(ev[1].severity, "2");
        assert_eq!(ev[0].host, "shop.example.com");
        assert_eq!(ev[0].client_ip, "198.51.100.7");
        assert_eq!(ev[0].action, "blocked");
        assert_eq!(ev[0].tx_id, "abc123");
        assert_eq!(ev[0].created_at.as_ref().unwrap().seconds, 1_793_016_001);
    }

    #[test]
    fn unparsable_or_empty_lines_yield_nothing() {
        assert!(parse_waf_line("not json").is_empty());
        assert!(parse_waf_line(r#"{"transaction":{}}"#).is_empty());
    }

    #[test]
    fn detection_only_is_reported_as_detected() {
        let ev =
            parse_waf_line(&LINE.replace("\"is_interrupted\":true", "\"is_interrupted\":false"));
        assert_eq!(ev[0].action, "detected");
    }

    #[test]
    fn partial_trailing_line_waits_for_the_next_read() {
        let data = b"{\"a\":1}\n{\"b\":2";
        let (lines, consumed) = complete_lines(data);
        assert_eq!(lines, vec!["{\"a\":1}"]);
        assert_eq!(consumed, 8);
    }

    #[test]
    fn tailer_resumes_and_survives_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = IngressPaths {
            state_dir: dir.path().join("state"),
            waf_log: dir.path().join("waf.jsonl"),
            ..Default::default()
        };
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(&paths.waf_log, format!("{LINE}\n")).unwrap();
        let (events, offset) = read_new_waf_events(&paths).unwrap();
        assert_eq!(events.len(), 2);
        offset.store(&paths.waf_offset_file()).unwrap();
        // Nothing new.
        assert!(read_new_waf_events(&paths).unwrap().0.is_empty());
        // Appended line is picked up from the stored offset.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&paths.waf_log)
            .unwrap();
        writeln!(f, "{LINE}").unwrap();
        assert_eq!(read_new_waf_events(&paths).unwrap().0.len(), 2);
        let (_, cursor) = read_new_waf_events(&paths).unwrap();
        cursor.store(&paths.waf_offset_file()).unwrap();
        // Rotated to a new file of any size: start over instead of skipping it.
        std::fs::rename(&paths.waf_log, dir.path().join("waf.jsonl.1")).unwrap();
        std::fs::write(&paths.waf_log, format!("{LINE}\n")).unwrap();
        assert_eq!(read_new_waf_events(&paths).unwrap().0.len(), 2);
        // Truncated in place: also start over.
        let (_, cursor) = read_new_waf_events(&paths).unwrap();
        cursor.store(&paths.waf_offset_file()).unwrap();
        std::fs::write(&paths.waf_log, "").unwrap();
        std::fs::write(&paths.waf_log, format!("{}\n", LINE.replace("abc123", "x"))).unwrap();
        assert_eq!(read_new_waf_events(&paths).unwrap().0.len(), 2);
    }

    #[test]
    fn secret_names_are_plain_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = IngressPaths {
            secrets_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let p = write_secret(&paths, "dns.env", b"TOKEN=x\n").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"TOKEN=x\n");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for bad in ["", "../x", "a/b", ".hidden", "sp ace"] {
            assert!(write_secret(&paths, bad, b"x").is_err(), "{bad}");
        }
    }

    #[test]
    fn config_hash_is_verified_before_anything_is_loaded() {
        let paths = IngressPaths {
            admin_socket: "/nonexistent/admin.sock".into(),
            ..Default::default()
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(apply_config(&paths, "{}", "deadbeef"))
            .unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        let err = rt
            .block_on(apply_config(&paths, "{", &sha256_hex(b"{")))
            .unwrap_err();
        assert!(err.contains("not JSON"), "{err}");
    }

    #[test]
    fn parses_caddy_upstreams_and_listen_ip() {
        let up = parse_upstreams(br#"[{"address":"10.0.0.5:8080","num_requests":0,"fails":0},{"address":"10.0.0.6:8080","fails":3}]"#);
        assert_eq!(up.len(), 2);
        assert!(up[0].healthy && !up[1].healthy);
        let cfg = br#"{"apps":{"http":{"servers":{"kcore":{"listen":["[2001:db8::1]:443","203.0.113.10:80"]}}}}}"#;
        assert_eq!(first_listen_ip(cfg).as_deref(), Some("2001:db8::1"));
        assert!(live_config_has_servers(cfg));
        assert!(!live_config_has_servers(br#"{"admin":{}}"#));
    }

    #[test]
    fn fib_trie_local_addresses_are_found() {
        let table = "Local:\n  +-- 10.0.0.0/24 2 0 2\n     |-- 10.0.0.1\n        /32 host LOCAL\n     |-- 10.0.0.255\n        /32 link BROADCAST\n";
        assert!(fib_trie_has_local(table, "10.0.0.1"));
        assert!(!fib_trie_has_local(table, "10.0.0.255"));
        assert!(!fib_trie_has_local(table, "10.0.0.9"));
    }

    fn store_paths(dir: &Path) -> IngressPaths {
        IngressPaths {
            state_dir: dir.join("state"),
            ..Default::default()
        }
    }

    #[test]
    fn store_round_trips_and_keeps_keys_private() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let from = store_paths(src.path());
        let host = from
            .state_dir
            .join("certmagic/certificates/acme-v02-directory/shop.example.com");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("shop.example.com.crt"), b"CERT").unwrap();
        std::fs::write(host.join("shop.example.com.key"), b"KEY").unwrap();
        std::fs::set_permissions(
            host.join("shop.example.com.key"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        // Locks stay local.
        let locks = from.state_dir.join("certmagic/locks");
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::write(locks.join("issue_cert_x.lock"), b"x").unwrap();

        let (archive, digest, n) = export_store(&from).unwrap();
        assert_eq!(n, 2);
        let to = store_paths(dst.path());
        std::fs::create_dir_all(&to.state_dir).unwrap();
        assert_eq!(import_store(&to, &archive, &digest).unwrap(), 2);
        let out = to
            .state_dir
            .join("certmagic/certificates/acme-v02-directory/shop.example.com");
        assert_eq!(
            std::fs::read(out.join("shop.example.com.crt")).unwrap(),
            b"CERT"
        );
        let key_mode = std::fs::metadata(out.join("shop.example.com.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(key_mode, 0o600);
        assert!(!to.state_dir.join("certmagic/locks").exists());
    }

    #[test]
    fn import_rejects_tampering_and_unsafe_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = store_paths(dir.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let archive = br#"{"files":[]}"#;
        assert!(import_store(&paths, archive, "bad")
            .unwrap_err()
            .contains("digest"));
        for evil in [
            "../escape",
            "/etc/passwd",
            "certificates",
            "locks/x",
            "certificates/../../x",
            "acme/./x",
        ] {
            let a = format!(r#"{{"files":[{{"path":"{evil}","mode":420,"data":""}}]}}"#);
            let d = sha256_hex(a.as_bytes());
            assert!(
                import_store(&paths, a.as_bytes(), &d)
                    .unwrap_err()
                    .contains("unsafe path"),
                "{evil}"
            );
        }
        assert!(safe_store_path("certificates/a/b.crt").is_some());
        assert!(safe_store_path("acme/x/users/u/key").is_some());
    }
}
