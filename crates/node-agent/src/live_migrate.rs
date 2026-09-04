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

/// Written by `modules/ch-vm/vm-service.nix` so the node agent and the systemd
/// units it hands VMs off to always resolve the *same* Cloud Hypervisor build
/// and the *same* API socket directory.
pub const CH_BIN_PATH_FILE: &str = "/etc/kcore/cloud-hypervisor";
pub const VM_SOCKET_DIR_FILE: &str = "/etc/kcore/vm-socket-dir";

/// Migration listeners are allocated from a fixed range that sits *below* the
/// kernel's default local port range (32768-60999), so the kernel never hands
/// one of these out to an unrelated outbound connection while Cloud Hypervisor
/// is starting up.
pub const MIGRATE_PORT_BASE: u16 = 18000;
pub const MIGRATE_PORT_COUNT: u16 = 128;

#[derive(Debug)]
pub struct ReceiveSession {
    pub port: u16,
    pub ch_pid: u32,
    pub receive_task: tokio::task::JoinHandle<Result<(), String>>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, ReceiveSession>,
    /// Ports handed out by `reserve_port`, each holding a live listener until
    /// the moment Cloud Hypervisor is told to bind it. Holding the socket is
    /// what stops another process from stealing the port in between.
    reserved: HashMap<u16, Option<std::net::TcpListener>>,
}

#[derive(Clone, Default)]
pub struct LiveMigrateState {
    inner: Arc<Mutex<Inner>>,
}

impl LiveMigrateState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, vm: &str, session: ReceiveSession) {
        self.inner
            .lock()
            .await
            .sessions
            .insert(vm.to_string(), session);
    }

    pub async fn take(&self, vm: &str) -> Option<ReceiveSession> {
        self.inner.lock().await.sessions.remove(vm)
    }

    pub async fn get_port(&self, vm: &str) -> Option<u16> {
        self.inner.lock().await.sessions.get(vm).map(|s| s.port)
    }

    /// Claim a migration port from the reserved range and hold a listener on
    /// it so nothing else can bind it before Cloud Hypervisor does.
    pub async fn reserve_port(&self) -> Result<u16, String> {
        let mut inner = self.inner.lock().await;
        for offset in 0..MIGRATE_PORT_COUNT {
            let port = MIGRATE_PORT_BASE + offset;
            if inner.reserved.contains_key(&port) {
                continue;
            }
            if let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) {
                inner.reserved.insert(port, Some(listener));
                return Ok(port);
            }
        }
        Err(format!(
            "no free live-migration port in range {}-{}",
            MIGRATE_PORT_BASE,
            MIGRATE_PORT_BASE + MIGRATE_PORT_COUNT - 1
        ))
    }

    /// Claim a caller-chosen port. Fails cleanly (rather than letting Cloud
    /// Hypervisor fail later) when the port is already taken.
    pub async fn reserve_explicit_port(&self, port: u16) -> Result<u16, String> {
        let mut inner = self.inner.lock().await;
        if inner.reserved.contains_key(&port) {
            return Err(format!("port {port} is already reserved on this node"));
        }
        match std::net::TcpListener::bind(("0.0.0.0", port)) {
            Ok(listener) => {
                inner.reserved.insert(port, Some(listener));
                Ok(port)
            }
            Err(e) => Err(format!("port {port} is not available: {e}")),
        }
    }

    /// Give up the listening socket so Cloud Hypervisor can take the port,
    /// while keeping the port itself reserved against other callers here.
    pub async fn release_listener(&self, port: u16) {
        if let Some(slot) = self.inner.lock().await.reserved.get_mut(&port) {
            *slot = None;
        }
    }

    pub async fn release_port(&self, port: u16) {
        self.inner.lock().await.reserved.remove(&port);
    }
}

/// Ask the kernel for a free ephemeral port and immediately give it back.
///
/// Inherently racy — the port can be taken between this call and the moment
/// Cloud Hypervisor binds it. Prefer [`LiveMigrateState::reserve_port`], which
/// holds the socket until the handoff.
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

/// True when some process holds a listening TCP socket on `port`.
///
/// Read from `/proc/net/tcp{,6}` rather than probed with `bind`/`connect`:
/// binding would race the very process we are waiting for, and connecting
/// would consume Cloud Hypervisor's single migration connection.
pub fn port_is_listening(port: u16) -> bool {
    const TCP_LISTEN: &str = "0A";
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in contents.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let Some(local) = fields.nth(1) else {
                continue;
            };
            // Fields are: sl local_address rem_address st. `local` consumed
            // index 1, so the state column is two further on, not one.
            let Some(state) = fields.nth(1) else {
                continue;
            };
            if state != TCP_LISTEN {
                continue;
            }
            let Some((_, hex_port)) = local.rsplit_once(':') else {
                continue;
            };
            if u16::from_str_radix(hex_port, 16) == Ok(port) {
                return true;
            }
        }
    }
    false
}

/// Wait until Cloud Hypervisor owns the migration port. Returns a plain error
/// (never hangs) so the caller can abort and retry the migration.
pub async fn wait_for_port_listening(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if port_is_listening(port) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "cloud-hypervisor did not start listening on migration port {port} within {}s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn rbd_device(pool: &str, image: &str) -> PathBuf {
    PathBuf::from(format!("/dev/rbd/{pool}/{image}"))
}

pub fn rbd_is_mapped(pool: &str, image: &str) -> bool {
    rbd_device(pool, image).exists()
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
    if dev.exists() {
        return Err(format!(
            "rbd device {} still present after unmap",
            dev.display()
        ));
    }
    Ok(())
}

pub fn handoff_marker_path(socket_dir: &Path, vm_name: &str) -> PathBuf {
    socket_dir.join(format!("{vm_name}.live-migrated"))
}

pub fn migrate_pid_path(socket_dir: &Path, vm_name: &str) -> PathBuf {
    socket_dir.join(format!("{vm_name}.migrate.pid"))
}

/// PID of a receive-mode Cloud Hypervisor recorded by `spawn_receive_vmm`.
pub fn read_migrate_pid(socket_dir: &Path, vm_name: &str) -> Option<u32> {
    std::fs::read_to_string(migrate_pid_path(socket_dir, vm_name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn kill_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
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
        .map_err(|e| format!("spawn cloud-hypervisor ({ch_bin}): {e}"))?;
    let pid = child
        .id()
        .ok_or_else(|| "cloud-hypervisor produced no pid".to_string())?;
    // Detach: don't await; leave process running for the migrated guest.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let pid_path = migrate_pid_path(client.socket_dir(), vm_name);
    if let Err(e) = std::fs::write(&pid_path, pid.to_string()) {
        kill_pid(pid);
        return Err(format!("write migrate pid: {e}"));
    }
    if let Err(e) = client
        .wait_api_ready(vm_name, Duration::from_secs(10))
        .await
    {
        // Otherwise the half-started VMM keeps the API socket and the RBD
        // mapping alive with nothing tracking it.
        kill_pid(pid);
        let _ = std::fs::remove_file(&pid_path);
        let _ = std::fs::remove_file(&socket);
        return Err(e);
    }
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
        return Err(format!(
            "systemctl set-property Restart=no failed for {unit}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
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

/// `systemctl is-active` output that means the unit is not running. Any state
/// other than `active`/`activating`/`reloading` counts as stopped.
pub fn unit_state_is_stopped(is_active_stdout: &str) -> bool {
    !matches!(
        is_active_stdout.trim(),
        "active" | "activating" | "reloading"
    )
}

pub async fn unit_is_active(unit: &str) -> bool {
    let Ok(out) = Command::new("systemctl")
        .args(["is-active", unit])
        .output()
        .await
    else {
        return false;
    };
    !unit_state_is_stopped(&String::from_utf8_lossy(&out.stdout))
}

pub async fn unit_is_stopped(unit: &str) -> Result<bool, String> {
    let out = Command::new("systemctl")
        .args(["is-active", unit])
        .output()
        .await
        .map_err(|e| format!("systemctl is-active {unit}: {e}"))?;
    // A non-zero exit is the normal answer for inactive/failed/missing units.
    Ok(unit_state_is_stopped(&String::from_utf8_lossy(&out.stdout)))
}

/// `systemctl show -p LoadState` output that means systemd knows the unit.
pub fn unit_load_state_is_loaded(load_state_stdout: &str) -> bool {
    load_state_stdout
        .trim()
        .rsplit('=')
        .next()
        .map(|v| v.trim() == "loaded")
        .unwrap_or(false)
}

/// Block until systemd has the unit loaded, i.e. the destination rebuild has
/// activated the generated configuration. Returns an error on timeout instead
/// of letting the caller `systemctl start` a unit that does not exist yet.
pub async fn wait_for_unit_loaded(unit: &str, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let out = Command::new("systemctl")
            .args(["show", "-p", "LoadState", "--value", unit])
            .output()
            .await
            .map_err(|e| format!("systemctl show {unit}: {e}"))?;
        let value = String::from_utf8_lossy(&out.stdout);
        if value.trim() == "loaded" || unit_load_state_is_loaded(&value) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "unit {unit} was still not loaded after {}s; the destination \
                 nixos-rebuild has not activated the migrated VM's configuration",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Cloud Hypervisor binary, resolved from the same place the systemd unit uses.
///
/// Mixing builds across a handoff would have systemd supervising a process it
/// cannot restart with matching arguments, so an explicit path always wins over
/// whatever `cloud-hypervisor` happens to be on `PATH`.
pub fn resolve_ch_bin() -> String {
    if let Some(from_env) = std::env::var("KCORE_CLOUD_HYPERVISOR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return from_env;
    }
    if let Some(from_nix) = std::fs::read_to_string(CH_BIN_PATH_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return from_nix;
    }
    "cloud-hypervisor".to_string()
}

/// The socket directory the generated VM units use, when the Nix module
/// published one. `None` means the module predates this file and the node
/// agent's own configuration is the only source available.
pub fn nix_vm_socket_dir() -> Option<String> {
    std::fs::read_to_string(VM_SOCKET_DIR_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The handoff only works if the agent writes markers and PID files where the
/// generated unit reads them, so a mismatch has to fail loudly up front.
pub fn check_socket_dir_matches_nix(configured: &Path) -> Result<(), String> {
    let Some(expected) = nix_vm_socket_dir() else {
        return Ok(());
    };
    if Path::new(&expected) == configured {
        return Ok(());
    }
    Err(format!(
        "node agent vmSocketDir '{}' does not match ch-vm.vms.socketDir '{}' from {}; \
         live migration handoff would write markers the VM unit never reads",
        configured.display(),
        expected,
        VM_SOCKET_DIR_FILE
    ))
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

    #[tokio::test]
    async fn reserve_port_returns_port_in_reserved_range() {
        let state = LiveMigrateState::new();
        let port = state.reserve_port().await.expect("reserve");
        assert!(port >= MIGRATE_PORT_BASE);
        assert!(port < MIGRATE_PORT_BASE + MIGRATE_PORT_COUNT);
    }

    #[tokio::test]
    async fn reserve_port_does_not_hand_out_the_same_port_twice() {
        let state = LiveMigrateState::new();
        let first = state.reserve_port().await.expect("first");
        let second = state.reserve_port().await.expect("second");
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn reserved_port_stays_bound_until_the_listener_is_released() {
        let state = LiveMigrateState::new();
        let port = state.reserve_port().await.expect("reserve");
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", port)).is_err(),
            "a held reservation must keep the port unbindable"
        );
        state.release_listener(port).await;
        std::net::TcpListener::bind(("0.0.0.0", port))
            .expect("port must be free once the listener is released");
    }

    #[tokio::test]
    async fn reserve_explicit_port_rejects_an_already_reserved_port() {
        let state = LiveMigrateState::new();
        let port = state.reserve_port().await.expect("reserve");
        let err = state
            .reserve_explicit_port(port)
            .await
            .expect_err("double reservation must fail");
        assert!(err.contains("already reserved"));
    }

    #[tokio::test]
    async fn released_port_can_be_reserved_again() {
        let state = LiveMigrateState::new();
        let port = state.reserve_port().await.expect("reserve");
        state.release_port(port).await;
        assert_eq!(
            state.reserve_explicit_port(port).await.expect("re-reserve"),
            port
        );
    }

    #[test]
    fn port_is_listening_detects_a_live_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert!(port_is_listening(port));
        drop(listener);
    }

    #[test]
    fn unit_state_is_stopped_only_for_non_running_states() {
        assert!(!unit_state_is_stopped("active\n"));
        assert!(!unit_state_is_stopped("activating"));
        assert!(!unit_state_is_stopped("reloading"));
        assert!(unit_state_is_stopped("inactive\n"));
        assert!(unit_state_is_stopped("failed"));
        assert!(unit_state_is_stopped(""));
    }

    #[test]
    fn unit_load_state_parses_both_show_formats() {
        assert!(unit_load_state_is_loaded("loaded\n"));
        assert!(unit_load_state_is_loaded("LoadState=loaded"));
        assert!(!unit_load_state_is_loaded("LoadState=not-found"));
        assert!(!unit_load_state_is_loaded(""));
    }

    #[test]
    fn check_socket_dir_matches_nix_passes_without_published_dir() {
        if nix_vm_socket_dir().is_none() {
            assert!(check_socket_dir_matches_nix(Path::new("/run/kcore")).is_ok());
        }
    }
}
