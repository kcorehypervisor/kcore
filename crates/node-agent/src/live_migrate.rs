//! Live migration helpers for Cloud Hypervisor over shared RBD.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::vmm;

#[derive(Debug)]
pub struct ReceiveSession {
    pub port: u16,
    pub ch_pid: u32,
    pub receive_task: tokio::task::JoinHandle<Result<(), String>>,
}

#[derive(Clone, Default)]
pub struct LiveMigrateState {
    inner: Arc<Mutex<HashMap<String, ReceiveSession>>>,
}

impl LiveMigrateState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, vm: &str, session: ReceiveSession) {
        self.inner.lock().await.insert(vm.to_string(), session);
    }

    pub async fn take(&self, vm: &str) -> Option<ReceiveSession> {
        self.inner.lock().await.remove(vm)
    }

    pub async fn get_port(&self, vm: &str) -> Option<u16> {
        self.inner.lock().await.get(vm).map(|s| s.port)
    }
}

pub fn pick_free_tcp_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("0.0.0.0:0")
        .map_err(|e| format!("bind ephemeral port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

pub fn rbd_device(pool: &str, image: &str) -> PathBuf {
    PathBuf::from(format!("/dev/rbd/{pool}/{image}"))
}

pub fn ensure_rbd_mapped(pool: &str, image: &str) -> Result<(), String> {
    let dev = rbd_device(pool, image);
    if dev.exists() {
        return Ok(());
    }
    let handle = format!("{pool}/{image}");
    let out = std::process::Command::new("rbd")
        .args(["map", &handle])
        .output()
        .map_err(|e| format!("rbd map: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rbd map failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    for _ in 0..50 {
        if dev.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "rbd device {} did not appear after map",
        dev.display()
    ))
}

pub fn ensure_rbd_unmapped(pool: &str, image: &str) -> Result<(), String> {
    let dev = rbd_device(pool, image);
    if !dev.exists() {
        return Ok(());
    }
    let out = std::process::Command::new("rbd")
        .args(["unmap", &dev.display().to_string()])
        .output()
        .map_err(|e| format!("rbd unmap: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Already unmapped is fine.
        if err.contains("not mapped") || err.contains("No such file") {
            return Ok(());
        }
        return Err(format!("rbd unmap failed: {}", err.trim()));
    }
    Ok(())
}

pub fn handoff_marker_path(socket_dir: &Path, vm_name: &str) -> PathBuf {
    socket_dir.join(format!("{vm_name}.live-migrated"))
}

pub fn migrate_pid_path(socket_dir: &Path, vm_name: &str) -> PathBuf {
    socket_dir.join(format!("{vm_name}.migrate.pid"))
}

pub async fn spawn_receive_vmm(
    client: &vmm::Client,
    vm_name: &str,
    ch_bin: &str,
) -> Result<u32, String> {
    let socket = client.socket_path(vm_name);
    if socket.exists() {
        let _ = std::fs::remove_file(&socket);
    }
    let mut child = Command::new(ch_bin)
        .arg(format!("--api-socket={}", socket.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| format!("spawn cloud-hypervisor: {e}"))?;
    let pid = child
        .id()
        .ok_or_else(|| "cloud-hypervisor produced no pid".to_string())?;
    // Detach: don't await; leave process running for the migrated guest.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    std::fs::write(
        migrate_pid_path(client.socket_dir(), vm_name),
        pid.to_string(),
    )
    .map_err(|e| format!("write migrate pid: {e}"))?;
    client
        .wait_api_ready(vm_name, Duration::from_secs(10))
        .await?;
    Ok(pid)
}

pub async fn start_receive_task(
    client: vmm::Client,
    vm_name: String,
    port: u16,
) -> tokio::task::JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        let url = format!("tcp:0.0.0.0:{port}");
        info!(%vm_name, %url, "waiting for live migration receive");
        let result = client.receive_migration(&vm_name, &url).await;
        if result.is_ok() {
            let marker = handoff_marker_path(client.socket_dir(), &vm_name);
            if let Err(e) = std::fs::write(&marker, b"1") {
                warn!(error = %e, "failed to write live-migrated marker");
            }
        }
        result
    })
}

pub async fn disable_unit_restart(unit: &str) -> Result<(), String> {
    // Prevent systemd from restarting the source CH after send-migration exits.
    let out = Command::new("systemctl")
        .args(["set-property", unit, "Restart=no"])
        .output()
        .await
        .map_err(|e| format!("systemctl set-property: {e}"))?;
    if !out.status.success() {
        // Non-fatal on hosts without the unit yet.
        warn!(
            unit,
            stderr = %String::from_utf8_lossy(&out.stderr),
            "systemctl set-property Restart=no failed"
        );
    }
    Ok(())
}

pub async fn stop_unit(unit: &str) -> Result<(), String> {
    let out = Command::new("systemctl")
        .args(["stop", unit])
        .output()
        .await
        .map_err(|e| format!("systemctl stop: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemctl stop {unit} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

pub fn resolve_ch_bin() -> String {
    std::env::var("KCORE_CLOUD_HYPERVISOR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "cloud-hypervisor".to_string())
}

pub fn vm_unit_name(vm_name: &str) -> String {
    format!("kcore-vm-{vm_name}.service")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbd_device_path_uses_pool_and_image() {
        assert_eq!(
            rbd_device("kcore-vms", "kcore-abc"),
            PathBuf::from("/dev/rbd/kcore-vms/kcore-abc")
        );
    }

    #[test]
    fn pick_free_tcp_port_returns_nonzero() {
        let p = pick_free_tcp_port().unwrap();
        assert!(p > 0);
    }
}
