//! Write rendered files and ask the running services to reload.

use crate::model::Catalog;
use crate::pdns;
use crate::render::{self, ZonePlan};
use anyhow::{Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct Layout {
    pub state_dir: PathBuf,
}

impl Layout {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    pub fn database(&self) -> PathBuf {
        self.state_dir.join("state.sqlite")
    }
    pub fn kea(&self) -> PathBuf {
        self.state_dir.join("kea-dhcp4.json")
    }
    pub fn forward(&self) -> PathBuf {
        self.state_dir.join("forward.zones")
    }
    pub fn lua(&self) -> PathBuf {
        self.state_dir.join("recursor.lua")
    }
    pub fn recursor(&self) -> PathBuf {
        self.state_dir.join("recursor.yml")
    }
    pub fn pdns_conf(&self) -> PathBuf {
        self.state_dir.join("pdns.conf")
    }
    pub fn pdns_key(&self) -> PathBuf {
        self.state_dir.join("pdns-api.key")
    }
    pub fn token(&self) -> PathBuf {
        self.state_dir.join("api.token")
    }
    pub fn pdns_db(&self) -> PathBuf {
        PathBuf::from("/var/lib/pdns/pdns.sqlite3")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub zones: Vec<String>,
    pub kea_path: PathBuf,
}

pub fn render(layout: &Layout, catalog: &Catalog) -> Result<Rendered> {
    fs::create_dir_all(&layout.state_dir)?;
    fs::create_dir_all("/var/lib/kea").ok();
    fs::create_dir_all("/var/lib/pdns").ok();
    let key = ensure_secret(&layout.pdns_key())?;
    let _ = ensure_secret(&layout.token())?;
    let zones = render::zone_plan(catalog);
    let kea = render::kea_dhcp4(
        catalog,
        "/var/lib/kea/dhcp4.leases",
        "/run/kea/kea-dhcp4.sock",
    );
    write_text(&layout.kea(), &serde_json::to_string_pretty(&kea)?)?;
    write_text(&layout.forward(), &render::forward_zones(catalog))?;
    write_text(&layout.lua(), &render::recursor_lua(catalog))?;
    write_text(
        &layout.recursor(),
        &render::recursor_yml(catalog, &layout.state_dir.display().to_string()),
    )?;
    write_secret(
        &layout.pdns_conf(),
        &render::pdns_conf(&key, &layout.pdns_db().display().to_string()),
    )?;
    Ok(Rendered {
        zones: zones.iter().map(|z| z.name.clone()).collect(),
        kea_path: layout.kea(),
    })
}

pub fn reload_services() -> Vec<String> {
    vec![
        reload_kea(),
        systemctl(&["reload-or-restart", "kcore-pdns-recursor"]),
        rec_control(&["reload-zones"]),
        rec_control(&["reload-lua-script"]),
    ]
}

/// Ask the running Kea process to reread kea-dhcp4.json.
/// SIGHUP only reopens logs, so this uses the control socket.
pub fn reload_kea() -> String {
    const SOCKET: &str = "/run/kea/kea-dhcp4.sock";
    match kea_command(SOCKET, r#"{"command":"config-reload"}"#) {
        Ok(reply) if kea_reply_ok(&reply) => format!("kea config-reload ok: {reply}"),
        Ok(reply) => {
            let restarted = systemctl(&["restart", "kcore-kea-dhcp4"]);
            format!("kea config-reload rejected ({reply}); {restarted}")
        }
        Err(err) if kea_is_active() => {
            let restarted = systemctl(&["restart", "kcore-kea-dhcp4"]);
            format!("kea config-reload failed: {err}; {restarted}")
        }
        Err(err) => format!("kea is not running: {err}"),
    }
}

fn kea_reply_ok(reply: &str) -> bool {
    reply.contains("\"result\":0") || reply.contains("\"result\": 0")
}

fn kea_is_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "kcore-kea-dhcp4"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn kea_command(socket: &str, command: &str) -> Result<String> {
    use std::os::unix::net::UnixStream;
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connecting to {socket}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.write_all(command.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    let reply = reply.trim();
    if reply.is_empty() {
        return Err(anyhow::anyhow!("empty reply from {socket}"));
    }
    Ok(reply.to_string())
}

pub fn sync_dns(layout: &Layout, zones: &[ZonePlan]) -> Result<()> {
    let key = fs::read_to_string(layout.pdns_key()).context("reading the powerdns api key")?;
    pdns::sync("http://127.0.0.1:8081", key.trim(), zones)
}

fn systemctl(args: &[&str]) -> String {
    match Command::new("systemctl").args(args).output() {
        Ok(output) if output.status.success() => format!("systemctl {} ok", args.join(" ")),
        Ok(output) => format!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => format!("systemctl {} failed: {err}", args.join(" ")),
    }
}

fn rec_control(args: &[&str]) -> String {
    let mut command = Command::new("rec_control");
    command.arg("--socket-dir=/run/pdns-recursor").args(args);
    match command.output() {
        Ok(output) if output.status.success() => format!("rec_control {} ok", args.join(" ")),
        Ok(output) => format!(
            "rec_control {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => format!("rec_control {} failed: {err}", args.join(" ")),
    }
}

pub fn ensure_secret(path: &Path) -> Result<String> {
    if let Ok(existing) = fs::read_to_string(path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes);
    let value = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    write_secret(path, &value)?;
    Ok(value)
}

fn fill_random(bytes: &mut [u8]) {
    if let Ok(mut file) = fs::File::open("/dev/urandom") {
        use std::io::Read;
        if file.read_exact(bytes).is_ok() {
            return;
        }
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(17).wrapping_add(91);
    }
}

fn write_text(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o644)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?
        .write_all(body.as_bytes())?;
    Ok(())
}

fn write_secret(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = if body.ends_with('\n') {
        body.to_string()
    } else {
        format!("{body}\n")
    };
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?
        .write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample_catalog;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn render_writes_two_subnets() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let rendered = render(&layout, &sample_catalog()).unwrap();
        assert!(rendered.zones.iter().any(|zone| zone.contains("home.arpa")));
        let kea = fs::read_to_string(layout.kea()).unwrap();
        assert!(kea.contains("192.168.50.0/24"));
        assert!(kea.contains("lan10g"));
        assert!(fs::read_to_string(layout.forward())
            .unwrap()
            .contains("+.=1.1.1.1;8.8.8.8"));
        let mode = fs::metadata(layout.token()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
