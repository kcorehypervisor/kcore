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

    /// `(port, ch_pid)` of the tracked session, for reporting without taking it.
    pub async fn session_facts(&self, vm: &str) -> Option<(u16, u32)> {
        self.inner
            .lock()
            .await
            .sessions
            .get(vm)
            .map(|s| (s.port, s.ch_pid))
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

/// True when `pid` names a live process.
///
/// Signal 0 performs the permission and existence checks without delivering
/// anything. The node agent runs as root, so `EPERM` is not a case we have to
/// disambiguate: a failure means the process is gone.
pub fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// True when `pid`'s command line mentions `needle`.
///
/// Guards against PID reuse. A `.migrate.pid` file can outlive the process it
/// names, and by the time an operator clears the session the kernel may have
/// handed that number to something unrelated — killing it would be a far worse
/// outcome than leaving a stale file behind. The receive VMM is always spawned
/// as `<ch-bin> --api-socket=<socket>`, so the VM's own socket path identifies
/// it unambiguously.
pub fn pid_cmdline_contains(pid: u32, needle: &str) -> bool {
    if pid == 0 || needle.is_empty() {
        return false;
    }
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    // cmdline is NUL-separated; a plain lossy conversion is enough to look for
    // a substring inside one of the arguments.
    String::from_utf8_lossy(&raw).contains(needle)
}

/// Everything the node can observe about a receive session without disturbing
/// it. Produced by [`observe_receive`] and reported verbatim to operators.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReceiveObservation {
    pub has_session: bool,
    pub port: u16,
    pub session_pid: u32,
    pub pid_file_pid: u32,
    pub pid_file_present: bool,
    pub vmm_alive: bool,
    pub vmm_pid_matches_vm: bool,
    pub port_listening: bool,
    pub marker_present: bool,
    pub api_socket_present: bool,
}

impl ReceiveObservation {
    /// The pid a caller should act on: the in-memory session's if we have one,
    /// otherwise whatever the pid file recorded.
    pub fn effective_pid(&self) -> u32 {
        if self.session_pid != 0 {
            self.session_pid
        } else {
            self.pid_file_pid
        }
    }

    /// Nothing of this session is left on the node.
    pub fn is_empty(&self) -> bool {
        !self.has_session
            && !self.pid_file_present
            && !self.marker_present
            && !self.api_socket_present
    }

    /// Whether a receive is plausibly still running here.
    ///
    /// Deliberately *not* used to reap anything automatically: it is the line
    /// shown to an operator so they can make the call themselves. A recycled
    /// pid does not count as live.
    pub fn receive_looks_live(&self) -> bool {
        (self.vmm_alive && self.vmm_pid_matches_vm) || self.port_listening
    }

    /// One-line reading of the fields, written for someone deciding whether it
    /// is safe to clear this session.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "no receive session, pid file, marker or API socket on this node".to_string();
        }
        let mut parts = Vec::new();
        if self.has_session {
            parts.push(format!(
                "session tracked in memory on port {} (pid {})",
                self.port, self.session_pid
            ));
        } else {
            parts.push("no session tracked in memory".to_string());
        }
        if self.pid_file_present {
            parts.push(format!("pid file records {}", self.pid_file_pid));
        }
        let pid = self.effective_pid();
        if pid != 0 {
            if !self.vmm_alive {
                parts.push(format!("pid {pid} is not running"));
            } else if self.vmm_pid_matches_vm {
                parts.push(format!("pid {pid} is alive and is this VM's receive VMM"));
            } else {
                parts.push(format!(
                    "pid {pid} is alive but is NOT this VM's VMM (recycled pid)"
                ));
            }
        }
        if self.port != 0 {
            parts.push(format!(
                "port {} is {}",
                self.port,
                if self.port_listening {
                    "listening"
                } else {
                    "not listening"
                }
            ));
        }
        if self.marker_present {
            parts.push("handoff marker present (receive completed)".to_string());
        }
        if self.api_socket_present {
            parts.push("API socket present".to_string());
        }
        parts.push(
            if self.receive_looks_live() {
                "VERDICT: a receive may still be in flight; clearing would kill it"
            } else {
                "VERDICT: nothing is receiving; the session looks stranded"
            }
            .to_string(),
        );
        parts.join("; ")
    }
}

/// Collect the observable state of `vm_name`'s receive session on this node.
pub async fn observe_receive(
    state: &LiveMigrateState,
    socket_dir: &Path,
    vm_name: &str,
) -> ReceiveObservation {
    let session = state.session_facts(vm_name).await;
    let pid_file = migrate_pid_path(socket_dir, vm_name);
    let pid_file_pid = read_migrate_pid(socket_dir, vm_name).unwrap_or(0);
    let api_socket = socket_dir.join(format!("{vm_name}.sock"));

    let mut obs = ReceiveObservation {
        has_session: session.is_some(),
        port: session.map(|(port, _)| port).unwrap_or(0),
        session_pid: session.map(|(_, pid)| pid).unwrap_or(0),
        pid_file_pid,
        pid_file_present: pid_file.exists(),
        marker_present: handoff_marker_path(socket_dir, vm_name).exists(),
        api_socket_present: api_socket.exists(),
        ..Default::default()
    };
    let pid = obs.effective_pid();
    obs.vmm_alive = pid_is_alive(pid);
    obs.vmm_pid_matches_vm =
        obs.vmm_alive && pid_cmdline_contains(pid, &api_socket.display().to_string());
    obs.port_listening = obs.port != 0 && port_is_listening(obs.port);
    obs
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

    #[tokio::test]
    async fn wait_for_port_listening_returns_at_once_for_a_live_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        wait_for_port_listening(port, Duration::from_secs(5))
            .await
            .expect("an already-bound port must be seen immediately");
    }

    /// The prepare RPC must not claim to be listening when Cloud Hypervisor
    /// never bound the port; the source would dial a closed socket.
    #[tokio::test]
    async fn wait_for_port_listening_reports_a_timeout_rather_than_hanging() {
        // Bind and drop, so the port is almost certainly free and unused.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = probe.local_addr().expect("addr").port();
        drop(probe);
        let err = wait_for_port_listening(port, Duration::from_millis(300))
            .await
            .expect_err("an unbound port must time out");
        assert!(err.contains(&port.to_string()), "unexpected error: {err}");
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

    /// A pid that has been waited on is gone; our own pid is not.
    #[test]
    fn pid_is_alive_separates_our_own_pid_from_a_reaped_child() {
        assert!(pid_is_alive(std::process::id()));
        assert!(!pid_is_alive(0), "pid 0 is never a process we can act on");

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let dead = child.id();
        child.wait().expect("reap");
        assert!(
            !pid_is_alive(dead),
            "a reaped child must not look alive; pid {dead}"
        );
    }

    /// A long-lived process whose argv contains `path`, standing in for the
    /// receive-mode `cloud-hypervisor --api-socket=<path>`.
    ///
    /// `tail -f` is used rather than a shell one-liner because `sh -c` execs
    /// away a single simple command, replacing the argv we are looking for.
    fn spawn_holding(path: &Path) -> std::process::Child {
        std::fs::write(path, b"").expect("create file to hold open");
        let child = std::process::Command::new("tail")
            .arg("-f")
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tail");
        // Wait for the kernel to have the argv readable before asserting on it.
        for _ in 0..100 {
            if pid_cmdline_contains(child.id(), &path.display().to_string()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    #[test]
    fn pid_cmdline_contains_matches_only_the_real_argv() {
        let dir = tempfile::tempdir().expect("tempdir");
        let needle = dir.path().join("kcore-cmdline-needle.sock");
        let mut child = spawn_holding(&needle);
        let pid = child.id();

        assert!(pid_cmdline_contains(pid, &needle.display().to_string()));
        assert!(!pid_cmdline_contains(pid, "/tmp/some-other-vm.sock"));
        assert!(
            !pid_cmdline_contains(pid, ""),
            "an empty needle must never match"
        );
        assert!(
            !pid_cmdline_contains(0, &needle.display().to_string()),
            "pid 0 is never a process we can inspect"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"x").expect("write");
    }

    #[tokio::test]
    async fn observe_receive_reports_nothing_on_a_clean_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = LiveMigrateState::new();
        let obs = observe_receive(&state, dir.path(), "vm-clean").await;
        assert!(obs.is_empty());
        assert!(!obs.receive_looks_live());
        assert_eq!(obs.effective_pid(), 0);
        assert!(
            obs.summary().contains("no receive session"),
            "unexpected summary: {}",
            obs.summary()
        );
    }

    /// The decisive distinction for an operator: a pid file naming a process
    /// that has exited is a corpse, not an in-flight receive.
    #[tokio::test]
    async fn observe_receive_reports_a_dead_receive_vmm_as_stranded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let dead = child.id();
        child.wait().expect("reap");

        std::fs::write(migrate_pid_path(dir.path(), "vm-dead"), dead.to_string()).expect("pid");
        touch(&handoff_marker_path(dir.path(), "vm-dead"));

        let state = LiveMigrateState::new();
        let obs = observe_receive(&state, dir.path(), "vm-dead").await;
        assert!(obs.pid_file_present);
        assert_eq!(obs.pid_file_pid, dead);
        assert!(!obs.vmm_alive, "a reaped pid must not read as alive");
        assert!(!obs.receive_looks_live());
        assert!(
            !obs.is_empty(),
            "marker and pid file are leftovers to clear"
        );
        assert!(
            obs.summary().contains("looks stranded"),
            "unexpected summary: {}",
            obs.summary()
        );
    }

    /// The other half: a receive VMM that really is running must be reported
    /// as live, so an operator does not clear an in-flight migration.
    #[tokio::test]
    async fn observe_receive_reports_a_live_receive_vmm_as_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vm = "vm-live";
        // The observation identifies the VMM by the API socket in its argv,
        // which is how `spawn_receive_vmm` invokes cloud-hypervisor.
        let socket = dir.path().join(format!("{vm}.sock"));
        let mut child = spawn_holding(&socket);
        let pid = child.id();
        std::fs::write(migrate_pid_path(dir.path(), vm), pid.to_string()).expect("pid");

        let state = LiveMigrateState::new();
        let obs = observe_receive(&state, dir.path(), vm).await;
        assert!(obs.vmm_alive);
        assert!(obs.vmm_pid_matches_vm);
        assert!(obs.receive_looks_live());
        assert!(
            obs.summary().contains("may still be in flight"),
            "unexpected summary: {}",
            obs.summary()
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// PID reuse must not be mistaken for a live receive, or clearing would
    /// signal an unrelated process.
    #[tokio::test]
    async fn observe_receive_flags_a_recycled_pid_instead_of_calling_it_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Our own pid is certainly alive and certainly not this VM's VMM.
        std::fs::write(
            migrate_pid_path(dir.path(), "vm-recycled"),
            std::process::id().to_string(),
        )
        .expect("pid");

        let state = LiveMigrateState::new();
        let obs = observe_receive(&state, dir.path(), "vm-recycled").await;
        assert!(obs.vmm_alive, "the pid really is running");
        assert!(
            !obs.vmm_pid_matches_vm,
            "but it is not this VM's receive VMM"
        );
        assert!(!obs.receive_looks_live());
        assert!(
            obs.summary().contains("recycled pid"),
            "the operator must be told why: {}",
            obs.summary()
        );
    }

    #[tokio::test]
    async fn observe_receive_reports_the_in_memory_session_and_its_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = LiveMigrateState::new();
        let port = state.reserve_port().await.expect("reserve");
        state
            .insert(
                "vm-session",
                ReceiveSession {
                    port,
                    ch_pid: 4242,
                    receive_task: tokio::spawn(async { Ok(()) }),
                },
            )
            .await;

        let obs = observe_receive(&state, dir.path(), "vm-session").await;
        assert!(obs.has_session);
        assert_eq!(obs.port, port);
        assert_eq!(obs.session_pid, 4242);
        assert_eq!(
            obs.effective_pid(),
            4242,
            "the in-memory pid wins over the pid file"
        );
        // The reservation still holds the socket, so the port reads as taken.
        assert!(obs.port_listening);
    }
}

#[cfg(test)]
mod prop_tests {
    use super::ReceiveObservation;
    use proptest::prelude::*;

    prop_compose! {
        fn any_observation()(
            has_session in any::<bool>(),
            port in any::<u16>(),
            session_pid in any::<u32>(),
            pid_file_pid in any::<u32>(),
            pid_file_present in any::<bool>(),
            vmm_alive in any::<bool>(),
            vmm_pid_matches_vm in any::<bool>(),
            port_listening in any::<bool>(),
            marker_present in any::<bool>(),
            api_socket_present in any::<bool>(),
        ) -> ReceiveObservation {
            ReceiveObservation {
                has_session,
                port,
                session_pid,
                pid_file_pid,
                pid_file_present,
                vmm_alive,
                vmm_pid_matches_vm,
                port_listening,
                marker_present,
                api_socket_present,
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, .. ProptestConfig::default() })]

        /// The summary is what an operator reads mid-incident before deciding
        /// to destroy something, so it must always render.
        #[test]
        fn summary_always_renders(obs in any_observation()) {
            prop_assert!(!obs.summary().is_empty());
        }

        /// The verdict in the summary must agree with `receive_looks_live`:
        /// the operator's reading and the code's own predicate cannot diverge.
        #[test]
        fn summary_verdict_matches_the_liveness_predicate(obs in any_observation()) {
            let live = (obs.vmm_alive && obs.vmm_pid_matches_vm) || obs.port_listening;
            prop_assert_eq!(obs.receive_looks_live(), live);
            prop_assume!(!obs.is_empty());
            prop_assert_eq!(obs.summary().contains("may still be in flight"), live);
        }

        /// A pid that is alive but is not this VM's VMM is a recycled number.
        /// It must never on its own make a session look like a live receive,
        /// because clearing would then signal an unrelated process. Built
        /// rather than filtered so every case exercises the rule.
        #[test]
        fn a_recycled_pid_alone_never_reads_as_live(obs in any_observation()) {
            let recycled = ReceiveObservation {
                vmm_alive: true,
                vmm_pid_matches_vm: false,
                port_listening: false,
                ..obs
            };
            prop_assert!(!recycled.receive_looks_live());
            prop_assert!(
                !recycled.summary().contains("may still be in flight"),
                "summary: {}", recycled.summary()
            );
        }

        /// `effective_pid` prefers the session's pid and only falls back to
        /// the file, so a stale file can never redirect a kill.
        #[test]
        fn effective_pid_prefers_the_session(obs in any_observation()) {
            let expected = if obs.session_pid != 0 { obs.session_pid } else { obs.pid_file_pid };
            prop_assert_eq!(obs.effective_pid(), expected);
        }
    }
}
