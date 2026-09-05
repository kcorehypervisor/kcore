#![allow(clippy::result_large_err)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value as JsonValue;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use crate::auth::{self, CN_CONTROLLER_PREFIX, CN_KCTL};
use crate::discovery;
use crate::disk::classifier::{self, Verdict};
use crate::disk::lsblk;
use crate::live_migrate::{self, LiveMigrateState, ReceiveSession};
use crate::proto;
use crate::storage::{self, StorageAdapter};
use crate::vmm;
pub struct AdminService {
    nix_config_path: PathBuf,
    vm_socket_dir: PathBuf,
    storage: Arc<dyn StorageAdapter>,
    apply_lock: Arc<AsyncMutex<()>>,
    live_migrate: LiveMigrateState,
    /// Set by [`AdminService::with_pki`]; `None` leaves `RotateNodeCert`
    /// answering `unimplemented`, which is what the live-ISO/installer builds
    /// want since they have no enrolled identity to rotate.
    pki: Option<PkiRuntime>,
    /// Serialises rotation so two concurrent RotateNodeCert calls cannot
    /// interleave their installs.
    rotate_lock: Arc<AsyncMutex<()>>,
}

/// Everything `RotateNodeCert` needs: the node config (paths, controller
/// endpoints, thresholds) and the handle that rebuilds the TLS listener.
#[derive(Clone)]
struct PkiRuntime {
    cfg: crate::config::Config,
    reload: crate::pki::reload::ReloadHandle,
}

const BOOTSTRAP_CERT_DIR: &str = "/etc/kcore/certs";
const INSTALL_LOG_DIR: &str = "/var/log/kcore";
const NIXOS_CONFIG_PATH: &str = "/etc/nixos/configuration.nix";
const IMAGE_CACHE_DIR: &str = "/var/lib/kcore/images";
const DISK_MANAGEMENT_MODE_PATH: &str = "/etc/kcore/disk-management-mode";
const DISK_MANAGEMENT_MODE_PATH_LEGACY: &str = "/etc/kcore/disko-management-mode";
const DISK_MODE_INSTALLER_ONLY: &str = "installer-only";
const DISK_MODE_CONTROLLER_MANAGED: &str = "controller-managed";
const DISK_LAYOUT_DIR: &str = "/etc/kcore/disk";
const DISK_LAYOUT_CURRENT_PATH: &str = "/etc/kcore/disk/current.nix";
const CEPH_NIX_PATH: &str = "/etc/nixos/kcore-ceph.nix";
/// Rebuild verdicts live on tmpfs so they survive the node-agent restart that
/// `nixos-rebuild switch` performs, but not a reboot (after which there is
/// nothing in flight to report on anyway).
const NIX_APPLY_STATE_DIR: &str = "/run/kcore/nix-apply";
/// How long to wait for a freshly spawned receive-mode Cloud Hypervisor to
/// bind its migration port. Only covers process start plus one API call.
const RECEIVE_LISTEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long the destination waits for its `nixos-rebuild` to make the migrated
/// VM's unit known to systemd before giving up on adopting the guest.
const UNIT_LOADED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const NIX_APPLY_RUNNING: &str = "running";
const NIX_APPLY_SUCCEEDED: &str = "succeeded";
const NIX_APPLY_FAILED: &str = "failed";
/// A newer apply killed this one's rebuild before it reached a verdict. Maps to
/// `NIX_APPLY_PHASE_UNKNOWN`: there is nothing left to wait for.
const NIX_APPLY_SUPERSEDED: &str = "superseded";
const KCORE_VOLUME_ROOTS: &[&str] = &["/var/lib/kcore/volumes", "/var/lib/kcore/images"];

async fn resolve_nixpkgs_path() -> Option<String> {
    for candidate in [
        "/nix/var/nix/profiles/per-user/root/channels/nixos",
        "/run/current-system/sw/share/nixpkgs",
    ] {
        if std::path::Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }

    if let Ok(contents) = tokio::fs::read_to_string(NIXOS_CONFIG_PATH).await {
        for line in contents.lines() {
            if let Some(rest) = line.trim().strip_prefix("nix.nixPath") {
                if let Some(start) = rest.find("nixpkgs=") {
                    let path_start = start + "nixpkgs=".len();
                    let path_end = rest[path_start..]
                        .find(['"', '\'', ']'])
                        .map(|i| path_start + i)
                        .unwrap_or(rest.len());
                    let p = rest[path_start..path_end].trim();
                    if !p.is_empty() && std::path::Path::new(p).exists() {
                        return Some(p.to_string());
                    }
                }
            }
        }
    }

    let out = Command::new("nix")
        .args(["eval", "--raw", "nixpkgs#path"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(p)
    }
}

impl AdminService {
    pub fn new(nix_config_path: String) -> Self {
        Self::new_with_storage(
            nix_config_path,
            "/run/kcore".to_string(),
            storage::default_adapter(),
            LiveMigrateState::new(),
        )
    }

    pub fn new_with_storage(
        nix_config_path: String,
        vm_socket_dir: String,
        storage: Arc<dyn StorageAdapter>,
        live_migrate: LiveMigrateState,
    ) -> Self {
        Self {
            nix_config_path: PathBuf::from(nix_config_path),
            vm_socket_dir: PathBuf::from(vm_socket_dir),
            storage,
            apply_lock: Arc::new(AsyncMutex::new(())),
            live_migrate,
            pki: None,
            rotate_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Enable `RotateNodeCert` on this service.
    pub fn with_pki(
        mut self,
        cfg: crate::config::Config,
        reload: crate::pki::reload::ReloadHandle,
    ) -> Self {
        self.pki = Some(PkiRuntime { cfg, reload });
        self
    }

    /// Map the shared image, start a receive-mode Cloud Hypervisor and wait
    /// until it is really listening on `port`.
    ///
    /// Split out of `prepare_live_migrate_receive` so a failure anywhere in
    /// here releases the port reservation on the way out. Reporting success
    /// before the listener exists would have the source dial a closed port.
    async fn prepare_receive_session(
        &self,
        vm_name: &str,
        pool: &str,
        image: &str,
        port: u16,
    ) -> Result<Response<proto::PrepareLiveMigrateReceiveResponse>, Status> {
        live_migrate::ensure_rbd_mapped(pool, image)
            .map_err(|e| Status::internal(format!("map RBD {pool}/{image} for receive: {e}")))?;

        let client = vmm::Client::new(self.vm_socket_dir.to_str().unwrap_or("/run/kcore"));
        let ch_bin = live_migrate::resolve_ch_bin();
        let ch_pid = live_migrate::spawn_receive_vmm(&client, vm_name, &ch_bin)
            .await
            .map_err(|e| {
                Status::internal(format!(
                    "starting receive-mode cloud-hypervisor for {vm_name}: {e}"
                ))
            })?;
        // Cloud Hypervisor binds the port from inside the receive task, so the
        // listener must be handed over first.
        self.live_migrate.release_listener(port).await;
        let receive_task =
            live_migrate::start_receive_task(client, vm_name.to_string(), port).await;
        if let Err(e) = live_migrate::wait_for_port_listening(port, RECEIVE_LISTEN_TIMEOUT).await {
            receive_task.abort();
            live_migrate::kill_pid(ch_pid);
            if let Err(unmap) = live_migrate::ensure_rbd_unmapped(pool, image) {
                warn!(error = %unmap, "unmap RBD after failed receive prepare");
            }
            return Err(Status::internal(format!(
                "receive-mode cloud-hypervisor for {vm_name}: {e}"
            )));
        }
        self.live_migrate
            .insert(
                vm_name,
                ReceiveSession {
                    port,
                    ch_pid,
                    receive_task,
                },
            )
            .await;

        Ok(Response::new(proto::PrepareLiveMigrateReceiveResponse {
            success: true,
            message: format!("listening for migration on tcp:0.0.0.0:{port}"),
            listen_port: port as i32,
            // Bound on every interface: the controller knows better than this
            // node which of its addresses the source can reach.
            listen_addr: "0.0.0.0".into(),
        }))
    }

    /// Tear down everything a receive session owns and report what was there.
    ///
    /// The single implementation behind both the abort that ends a failed
    /// migration and the operator escape hatch for a stranded session, so the
    /// two can never drift over what "cleaned up" means. Returns the state
    /// observed *before* the teardown, which is the only chance to record
    /// whether a live receive was killed.
    ///
    /// Best-effort throughout: a missing marker or an already-unmapped image
    /// is the normal case when this runs twice, and must not turn a repeated
    /// runbook step into an error.
    async fn clear_receive_session(
        &self,
        vm_name: &str,
        rbd_pool: &str,
        rbd_image: &str,
    ) -> live_migrate::ReceiveObservation {
        // Snapshot before touching anything, so the caller can be told what
        // was destroyed rather than only that something was.
        let observed =
            live_migrate::observe_receive(&self.live_migrate, &self.vm_socket_dir, vm_name).await;

        if let Some(session) = self.live_migrate.take(vm_name).await {
            session.receive_task.abort();
            let _ = nix_kill(session.ch_pid);
            self.live_migrate.release_port(session.port).await;
        } else if observed.pid_file_pid != 0 {
            // No session in memory: this agent restarted after spawning the
            // receive VMM. The pid file is the only remaining handle on it, and
            // without this the orphan keeps the API socket and the RBD mapping
            // alive forever.
            let pid = observed.pid_file_pid;
            if observed.vmm_alive && !observed.vmm_pid_matches_vm {
                // The number was recycled onto an unrelated process. The stale
                // file still goes below, but we do not signal a stranger.
                warn!(
                    %vm_name,
                    pid,
                    "pid file names a live process that is not this VM's receive VMM; \
                     refusing to kill it and discarding the stale pid file"
                );
            } else {
                warn!(
                    %vm_name,
                    pid,
                    "no receive session in memory; killing the receive VMM recorded on disk"
                );
                live_migrate::kill_pid(pid);
            }
        }

        let _ = std::fs::remove_file(live_migrate::handoff_marker_path(
            &self.vm_socket_dir,
            vm_name,
        ));
        let _ = std::fs::remove_file(live_migrate::migrate_pid_path(&self.vm_socket_dir, vm_name));
        let _ = std::fs::remove_file(self.vm_socket_dir.join(format!("{vm_name}.sock")));

        if !rbd_pool.is_empty() && !rbd_image.is_empty() {
            if let Err(e) = live_migrate::ensure_rbd_unmapped(rbd_pool, rbd_image) {
                warn!(error = %e, "abort unmap RBD failed");
            }
        }
        observed
    }
}

fn receive_state_to_proto(
    obs: &live_migrate::ReceiveObservation,
) -> proto::LiveMigrateReceiveState {
    proto::LiveMigrateReceiveState {
        has_session: obs.has_session,
        port: obs.port as i32,
        session_pid: obs.session_pid,
        pid_file_pid: obs.pid_file_pid,
        vmm_alive: obs.vmm_alive,
        vmm_pid_matches_vm: obs.vmm_pid_matches_vm,
        port_listening: obs.port_listening,
        pid_file_present: obs.pid_file_present,
        marker_present: obs.marker_present,
        api_socket_present: obs.api_socket_present,
        summary: obs.summary(),
    }
}

fn validate_disk_path(path: &str, field: &str) -> Result<(), Status> {
    if !path.starts_with("/dev/") {
        return Err(Status::invalid_argument(format!(
            "{field}: must start with /dev/, got {path}"
        )));
    }
    if path.contains("..") {
        return Err(Status::invalid_argument(format!(
            "{field}: path traversal not allowed in {path}"
        )));
    }
    if path.contains(char::is_whitespace) {
        return Err(Status::invalid_argument(format!(
            "{field}: whitespace not allowed in {path}"
        )));
    }
    Ok(())
}

fn validate_apply_ceph_config_args(ceph_nix: &str, fsid: &str) -> Result<(), Status> {
    if ceph_nix.trim().is_empty() || fsid.trim().is_empty() {
        return Err(Status::invalid_argument("ceph_nix and fsid are required"));
    }
    Ok(())
}

fn validate_bootstrap_osd_device(osd_device: &str) -> Result<(), Status> {
    if !Path::new(osd_device.trim()).is_absolute() {
        return Err(Status::invalid_argument("osd_device must be absolute"));
    }
    Ok(())
}

/// SIGTERM one process.
///
/// Pid 0 is refused: `kill(0, ...)` signals *every process in the caller's
/// process group*, which for the node agent means itself and every VMM it
/// shares a group with. A zero pid here only ever means "we never learned the
/// real one", so there is nothing legitimate to signal.
fn nix_kill(pid: u32) -> Result<(), String> {
    if pid == 0 {
        return Err("refusing to signal pid 0 (would target the whole process group)".to_string());
    }
    let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn normalize_disk_management_mode(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        DISK_MODE_CONTROLLER_MANAGED => DISK_MODE_CONTROLLER_MANAGED,
        _ => DISK_MODE_INSTALLER_ONLY,
    }
}

fn read_disk_management_mode() -> &'static str {
    let raw = std::fs::read_to_string(DISK_MANAGEMENT_MODE_PATH)
        .or_else(|_| std::fs::read_to_string(DISK_MANAGEMENT_MODE_PATH_LEGACY))
        .unwrap_or_else(|_| DISK_MODE_INSTALLER_ONLY.to_string());
    normalize_disk_management_mode(&raw)
}

fn validate_disk_timeout_seconds_or_default(timeout_seconds: i32) -> u64 {
    if timeout_seconds <= 0 {
        300
    } else {
        timeout_seconds.min(3600) as u64
    }
}

fn write_bootstrap_pki(req: &proto::InstallToDiskRequest) -> Result<(), Status> {
    write_bootstrap_pki_at(req, &PathBuf::from(BOOTSTRAP_CERT_DIR))
}

fn rebuild_args(mode: &'static str) -> Vec<&'static str> {
    vec![mode]
}

fn rebuild_sequence(test_success: bool) -> Vec<&'static str> {
    if test_success {
        vec!["test", "switch"]
    } else {
        vec!["test"]
    }
}

async fn run_rebuild_mode(mode: &'static str) -> Result<std::process::Output, std::io::Error> {
    let nixpkgs_path = resolve_nixpkgs_path().await;
    if let Some(ref p) = nixpkgs_path {
        info!(nixpkgs_path = %p, "resolved nixpkgs path for rebuild");
    }
    let mut cmd = Command::new("nixos-rebuild");
    cmd.args(rebuild_args(mode));
    if let Some(ref p) = nixpkgs_path {
        cmd.env(
            "NIX_PATH",
            format!("nixos-config={NIXOS_CONFIG_PATH}:nixpkgs={p}"),
        );
    }
    cmd.output().await
}

async fn log_failed_kcore_units(context: &'static str) {
    let out = match Command::new("systemctl")
        .args([
            "--no-pager",
            "--full",
            "--failed",
            "list-units",
            "kcore-vm-*",
            "kcore-tap-*",
            "kcore-bridge-*",
            "kcore-dhcp-*",
        ])
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            error!(error = %e, "failed to inspect failed kcore units");
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stdout.contains("0 loaded units listed.") {
        info!(
            context = context,
            exit_code = out.status.code().unwrap_or(-1),
            "no failed kcore VM units after nix apply"
        );
        return;
    }
    error!(
        context = context,
        exit_code = out.status.code().unwrap_or(-1),
        stdout = %stdout,
        stderr = %stderr,
        "detected failed kcore VM units after nix apply"
    );
}

fn parse_stopped_vms_from_nix(configuration_nix: &str) -> Vec<String> {
    let mut current_vm: Option<String> = None;
    let mut stopped = Vec::new();
    for raw in configuration_nix.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("virtualMachines.\"") {
            if let Some((vm_name, _)) = rest.split_once("\"") {
                current_vm = Some(vm_name.to_string());
            }
            continue;
        }
        if line.starts_with("};") {
            current_vm = None;
            continue;
        }
        if let Some(vm_name) = current_vm.as_ref() {
            if line == "autoStart = false;" {
                stopped.push(vm_name.clone());
            }
        }
    }
    stopped.sort();
    stopped.dedup();
    stopped
}

async fn enforce_stopped_vm_units(stopped_vms: &[String]) {
    for vm_name in stopped_vms {
        let unit = format!("kcore-vm-{vm_name}.service");
        let out = match Command::new("systemctl")
            .args(["stop", &unit])
            .output()
            .await
        {
            Ok(out) => out,
            Err(e) => {
                error!(vm_name = %vm_name, error = %e, "failed to spawn systemctl stop");
                continue;
            }
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            error!(
                vm_name = %vm_name,
                unit = %unit,
                stderr = %stderr,
                "failed to enforce stopped VM unit after switch"
            );
        } else {
            info!(vm_name = %vm_name, unit = %unit, "enforced stopped VM unit");
        }
    }
}

/// Sanitize a caller-supplied apply id into something safe to use as a file
/// name under [`NIX_APPLY_STATE_DIR`].
fn sanitize_apply_id(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Fallback apply id for callers (older controllers) that do not supply one.
fn uuid_like_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}

fn apply_state_path(apply_id: &str) -> PathBuf {
    PathBuf::from(NIX_APPLY_STATE_DIR).join(format!("{apply_id}.state"))
}

fn latest_apply_id() -> Option<String> {
    std::fs::read_to_string(PathBuf::from(NIX_APPLY_STATE_DIR).join("latest"))
        .ok()
        .and_then(|s| sanitize_apply_id(&s))
}

fn record_apply_state(apply_id: &str, phase: &str, message: &str) {
    if let Err(e) = std::fs::write(apply_state_path(apply_id), format!("{phase}\n{message}\n")) {
        error!(error = %e, apply_id, "failed to record nix apply state");
    }
}

/// Retire the previous apply when a new one starts.
///
/// `run_test_then_switch` stops any in-flight `kcore-nix-rebuild` unit before
/// launching its own, so the older apply's rebuild is killed without ever
/// writing a verdict. Left alone its state file would read `running` forever
/// and a controller polling it would block until its own timeout.
fn supersede_running_applies(new_apply_id: &str) {
    let Some(previous) = latest_apply_id() else {
        return;
    };
    if previous == new_apply_id {
        return;
    }
    if let Some((phase, _)) = read_apply_state(&previous) {
        if phase == NIX_APPLY_RUNNING {
            record_apply_state(
                &previous,
                NIX_APPLY_SUPERSEDED,
                &format!("superseded by apply {new_apply_id}"),
            );
        }
    }
}

fn write_apply_state(apply_id: &str, phase: &str, message: &str) {
    if let Err(e) = std::fs::create_dir_all(NIX_APPLY_STATE_DIR) {
        error!(error = %e, dir = NIX_APPLY_STATE_DIR, "failed to create nix apply state dir");
        return;
    }
    if phase == NIX_APPLY_RUNNING {
        supersede_running_applies(apply_id);
    }
    record_apply_state(apply_id, phase, message);
    let _ = std::fs::write(
        PathBuf::from(NIX_APPLY_STATE_DIR).join("latest"),
        apply_id.as_bytes(),
    );
}

/// `(phase, message)` for a recorded apply, or `None` when the node has no
/// record of it.
fn read_apply_state(apply_id: &str) -> Option<(String, String)> {
    let raw = std::fs::read_to_string(apply_state_path(apply_id)).ok()?;
    let mut lines = raw.lines();
    let phase = lines.next()?.trim().to_string();
    let message = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    Some((phase, message))
}

fn apply_phase_to_proto(phase: &str) -> i32 {
    match phase {
        NIX_APPLY_RUNNING => proto::NixApplyPhase::Running as i32,
        NIX_APPLY_SUCCEEDED => proto::NixApplyPhase::Succeeded as i32,
        NIX_APPLY_FAILED => proto::NixApplyPhase::Failed as i32,
        _ => proto::NixApplyPhase::Unknown as i32,
    }
}

async fn run_test_then_switch(path: PathBuf, _desired_stopped_vms: Vec<String>, apply_id: String) {
    let nixpkgs_path = resolve_nixpkgs_path().await;
    let nix_path_val = nixpkgs_path
        .as_deref()
        .map(|p| format!("nixos-config={NIXOS_CONFIG_PATH}:nixpkgs={p}"))
        .unwrap_or_default();

    // `nixos-rebuild switch` restarts kcore-node-agent, so the rebuild must
    // outlive this process and the *script* — not a task in this process — has
    // to record the verdict. Callers poll GetNixApplyStatus for that verdict.
    let state_file = apply_state_path(&apply_id);
    let state_display = state_file.display().to_string();
    let script = format!(
        "export PATH=\"/run/current-system/sw/bin:$PATH\"; \
         export NIX_PATH='{nix_path_val}'; \
         if nixos-rebuild test && nixos-rebuild switch; then \
           printf '{succeeded}\\nnixos-rebuild test+switch completed\\n' > '{state}.tmp'; \
         else \
           printf '{failed}\\nnixos-rebuild test+switch failed (exit %s)\\n' \"$?\" > '{state}.tmp'; \
         fi; \
         mv -f '{state}.tmp' '{state}'",
        succeeded = NIX_APPLY_SUCCEEDED,
        failed = NIX_APPLY_FAILED,
        state = state_display,
    );

    info!(path = %path.display(), %apply_id, "launching nixos-rebuild test+switch via transient systemd unit");

    let _ = Command::new("systemctl")
        .args(["stop", "kcore-nix-rebuild.service"])
        .output()
        .await;
    let _ = Command::new("systemctl")
        .args(["reset-failed", "kcore-nix-rebuild.service"])
        .output()
        .await;

    let out = Command::new("systemd-run")
        .args([
            "--unit=kcore-nix-rebuild",
            "--collect",
            "--property=Type=exec",
            "--",
            "bash",
            "-c",
            &script,
        ])
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            info!("kcore-nix-rebuild transient unit launched successfully");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            error!(stderr = %stderr, "failed to launch kcore-nix-rebuild transient unit");
            write_apply_state(
                &apply_id,
                NIX_APPLY_FAILED,
                &format!("systemd-run failed: {stderr}"),
            );
        }
        Err(e) => {
            error!(error = %e, "failed to spawn systemd-run for nix rebuild");
            write_apply_state(
                &apply_id,
                NIX_APPLY_FAILED,
                &format!("spawning systemd-run failed: {e}"),
            );
        }
    }
}

fn validate_image_sha256(sha: &str) -> Result<String, Status> {
    let normalized = sha.trim().to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Status::invalid_argument(
            "image_sha256 must be exactly 64 hexadecimal characters",
        ));
    }
    Ok(normalized)
}

fn validate_image_url(url: &str) -> Result<String, Status> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument("image_url is required"));
    }
    if !trimmed.starts_with("https://") {
        return Err(Status::invalid_argument(
            "image_url must use https:// scheme",
        ));
    }
    Ok(trimmed.to_string())
}

fn validate_destination_path(path: &str) -> Result<PathBuf, Status> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument("destination_path is required"));
    }
    let p = PathBuf::from(trimmed);
    if !p.is_absolute() {
        return Err(Status::invalid_argument(
            "destination_path must be an absolute path",
        ));
    }
    if !p.starts_with(IMAGE_CACHE_DIR) {
        return Err(Status::invalid_argument(format!(
            "destination_path must be under {IMAGE_CACHE_DIR}"
        )));
    }
    if trimmed.contains("..") {
        return Err(Status::invalid_argument(
            "destination_path must not contain path traversal",
        ));
    }
    Ok(p)
}

fn parse_lease_entry(line: &str) -> Option<(&str, &str, &str)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 {
        return None;
    }
    let mac = fields[1];
    let ip = fields[2];
    let hostname = fields[3];
    Some((mac, ip, hostname))
}

fn find_vm_ip_in_lease_file(path: &Path, vm_name: &str, vm_mac: Option<&str>) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let target_mac = vm_mac.map(|m| m.to_ascii_lowercase());
    let mut matched_ip: Option<String> = None;
    for line in reader.lines().map_while(Result::ok) {
        let Some((mac, ip, hostname)) = parse_lease_entry(&line) else {
            continue;
        };
        let mac_match = target_mac
            .as_ref()
            .map(|target| mac.eq_ignore_ascii_case(target))
            .unwrap_or(false);
        let host_match = hostname == vm_name;
        if mac_match || host_match {
            matched_ip = Some(ip.to_string());
        }
    }
    matched_ip
}

fn vm_primary_mac(info: &crate::vmm::VmInfo) -> Option<String> {
    info.config
        .net
        .iter()
        .find_map(|n| n.mac.as_ref())
        .map(|m| m.to_ascii_lowercase())
}

fn lease_files_for_network(runtime_dir: &Path, network: &str) -> Vec<PathBuf> {
    if !network.trim().is_empty() {
        return vec![runtime_dir.join(format!("dnsmasq-{}.leases", network.trim()))];
    }
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|name| name.starts_with("dnsmasq-") && name.ends_with(".leases"))
        })
        .collect()
}

fn validate_port_or_default(port: i32) -> u16 {
    if port <= 0 {
        22
    } else {
        port.clamp(1, u16::MAX as i32) as u16
    }
}

fn validate_timeout_ms_or_default(timeout_ms: i32) -> u64 {
    if timeout_ms <= 0 {
        1500
    } else {
        timeout_ms as u64
    }
}

fn parse_i64_field(v: &JsonValue, key: &str) -> i64 {
    if let Some(n) = v.get(key).and_then(|x| x.as_i64()) {
        return n;
    }
    v.get(key)
        .and_then(|x| x.as_str())
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

fn parse_string_field(v: &JsonValue, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

async fn lvm_report_json(bin: &str, args: &[&str]) -> Result<JsonValue, String> {
    let resolved = resolve_lvm_bin(bin);
    let out = Command::new(&resolved)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("spawn {resolved} {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "{bin} {} failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }
    serde_json::from_slice::<JsonValue>(&out.stdout)
        .map_err(|e| format!("parse {resolved} {} json: {e}", args.join(" ")))
}

fn resolve_lvm_bin(bin: &str) -> String {
    let candidates = [
        format!("/run/current-system/sw/bin/{bin}"),
        format!("/nix/var/nix/profiles/default/bin/{bin}"),
        format!("/usr/sbin/{bin}"),
        format!("/usr/bin/{bin}"),
        bin.to_string(),
    ];
    for c in candidates {
        if std::path::Path::new(&c).exists() || !c.starts_with('/') {
            return c;
        }
    }
    bin.to_string()
}

async fn collect_lvm_info() -> proto::GetLvmInfoResponse {
    let vg = lvm_report_json(
        "vgs",
        &[
            "--reportformat",
            "json",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "vg_name,vg_size,vg_free,vg_attr",
        ],
    )
    .await;
    let lv = lvm_report_json(
        "lvs",
        &[
            "--reportformat",
            "json",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "lv_name,vg_name,lv_size,lv_attr,lv_path,pool_lv,origin,data_percent,metadata_percent",
        ],
    )
    .await;
    let pv = lvm_report_json(
        "pvs",
        &[
            "--reportformat",
            "json",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "pv_name,vg_name,pv_size,pv_free,pv_attr",
        ],
    )
    .await;

    if vg.is_err() && lv.is_err() && pv.is_err() {
        return proto::GetLvmInfoResponse {
            available: false,
            message: "lvm tooling unavailable or no readable reports".to_string(),
            volume_groups: vec![],
            logical_volumes: vec![],
            physical_volumes: vec![],
        };
    }

    let mut volume_groups = Vec::new();
    if let Ok(vg_json) = vg {
        if let Some(vgs) = vg_json
            .get("report")
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
            .and_then(|x| x.get("vg"))
            .and_then(|x| x.as_array())
        {
            for row in vgs {
                volume_groups.push(proto::LvmVolumeGroupInfo {
                    name: parse_string_field(row, "vg_name"),
                    size_bytes: parse_i64_field(row, "vg_size"),
                    free_bytes: parse_i64_field(row, "vg_free"),
                    attr: parse_string_field(row, "vg_attr"),
                });
            }
        }
    }

    let mut logical_volumes = Vec::new();
    if let Ok(lv_json) = lv {
        if let Some(lvs) = lv_json
            .get("report")
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
            .and_then(|x| x.get("lv"))
            .and_then(|x| x.as_array())
        {
            for row in lvs {
                logical_volumes.push(proto::LvmLogicalVolumeInfo {
                    name: parse_string_field(row, "lv_name"),
                    vg_name: parse_string_field(row, "vg_name"),
                    size_bytes: parse_i64_field(row, "lv_size"),
                    attr: parse_string_field(row, "lv_attr"),
                    path: parse_string_field(row, "lv_path"),
                    pool: parse_string_field(row, "pool_lv"),
                    origin: parse_string_field(row, "origin"),
                    data_percent: parse_string_field(row, "data_percent"),
                    metadata_percent: parse_string_field(row, "metadata_percent"),
                });
            }
        }
    }

    let mut physical_volumes = Vec::new();
    if let Ok(pv_json) = pv {
        if let Some(pvs) = pv_json
            .get("report")
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
            .and_then(|x| x.get("pv"))
            .and_then(|x| x.as_array())
        {
            for row in pvs {
                physical_volumes.push(proto::LvmPhysicalVolumeInfo {
                    name: parse_string_field(row, "pv_name"),
                    vg_name: parse_string_field(row, "vg_name"),
                    size_bytes: parse_i64_field(row, "pv_size"),
                    free_bytes: parse_i64_field(row, "pv_free"),
                    attr: parse_string_field(row, "pv_attr"),
                });
            }
        }
    }

    proto::GetLvmInfoResponse {
        available: true,
        message: "ok".to_string(),
        volume_groups,
        logical_volumes,
        physical_volumes,
    }
}

#[derive(Debug, Default, Clone)]
struct VmUnitState {
    active_state: String,
    sub_state: String,
    result: String,
    n_restarts: u32,
}

fn vm_unit_name(vm_name: &str) -> String {
    format!("kcore-vm-{vm_name}.service")
}

async fn read_vm_unit_state(vm_name: &str) -> Option<VmUnitState> {
    let unit = vm_unit_name(vm_name);
    let out = Command::new("systemctl")
        .args([
            "show",
            "--property=ActiveState,SubState,Result,NRestarts",
            &unit,
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut state = VmUnitState::default();
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            state.active_state = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("SubState=") {
            state.sub_state = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("Result=") {
            state.result = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("NRestarts=") {
            state.n_restarts = v.trim().parse::<u32>().unwrap_or(0);
        }
    }
    Some(state)
}

async fn vm_recent_failure_hint(vm_name: &str) -> Option<String> {
    let unit = vm_unit_name(vm_name);
    let out = Command::new("journalctl")
        .args(["-u", &unit, "-n", "12", "--no-pager"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|s| s.trim().to_string())
}

fn vm_unit_is_fatal(state: &VmUnitState) -> bool {
    if state.active_state == "failed" {
        return true;
    }
    // auto-restart flapping with repeated exit failures is effectively fatal for readiness.
    state.result == "exit-code"
        && (state.active_state == "activating" || state.sub_state == "auto-restart")
        && state.n_restarts >= 3
}

fn parse_neigh_line(line: &str) -> Option<(&str, &str)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 5 {
        return None;
    }
    let ip = fields[0];
    let lladdr_idx = fields.iter().position(|f| *f == "lladdr")?;
    if lladdr_idx + 1 >= fields.len() {
        return None;
    }
    let mac = fields[lladdr_idx + 1];
    Some((ip, mac))
}

async fn find_vm_ip_in_neigh(vm_mac: &str, network: &str) -> Option<String> {
    let mut args = vec!["neigh".to_string(), "show".to_string()];
    let net = network.trim();
    if !net.is_empty() {
        args.push("dev".to_string());
        args.push(format!("kbr-{net}"));
    }
    let out = Command::new("ip").args(args).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut last_match: Option<String> = None;
    for line in stdout.lines() {
        let Some((ip, mac)) = parse_neigh_line(line) else {
            continue;
        };
        if mac.eq_ignore_ascii_case(vm_mac) {
            last_match = Some(ip.to_string());
        }
    }
    last_match
}

fn is_private_key(filename: &str) -> bool {
    filename.ends_with(".key")
}

fn write_bootstrap_pki_at(
    req: &proto::InstallToDiskRequest,
    base_dir: &PathBuf,
) -> Result<(), Status> {
    let certs = [
        ("ca.crt", &req.ca_cert_pem),
        ("node.crt", &req.node_cert_pem),
        ("node.key", &req.node_key_pem),
        ("controller.crt", &req.controller_cert_pem),
        ("controller.key", &req.controller_key_pem),
        ("kctl.crt", &req.kctl_cert_pem),
        ("kctl.key", &req.kctl_key_pem),
        ("sub-ca.crt", &req.sub_ca_cert_pem),
        ("sub-ca.key", &req.sub_ca_key_pem),
    ];

    let has_any = certs.iter().any(|(_, content)| !content.trim().is_empty());
    if !has_any {
        return Ok(());
    }

    std::fs::create_dir_all(base_dir)
        .map_err(|e| Status::internal(format!("creating {}: {e}", base_dir.display())))?;

    for (name, content) in certs {
        if content.trim().is_empty() {
            continue;
        }
        let path = base_dir.join(name);
        std::fs::write(&path, content)
            .map_err(|e| Status::internal(format!("writing {}: {e}", path.display())))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if is_private_key(name) { 0o600 } else { 0o644 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .map_err(|e| Status::internal(format!("chmod {}: {e}", path.display())))?;
        }
    }

    Ok(())
}

fn prepare_install_log() -> Result<(std::fs::File, PathBuf), Status> {
    std::fs::create_dir_all(INSTALL_LOG_DIR)
        .map_err(|e| Status::internal(format!("creating {INSTALL_LOG_DIR}: {e}")))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Status::internal(format!("system clock before UNIX_EPOCH: {e}")))?
        .as_secs();
    let log_path = PathBuf::from(INSTALL_LOG_DIR).join(format!("install-to-disk-{timestamp}.log"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| Status::internal(format!("opening {}: {e}", log_path.display())))?;
    Ok((file, log_path))
}

fn normalize_endpoint(s: &str, default_port: u16) -> String {
    let trimmed = s.trim();
    if trimmed.parse::<std::net::SocketAddr>().is_ok() {
        return trimmed.to_string();
    }
    if trimmed.starts_with('[') {
        return format!("{trimmed}:{default_port}");
    }
    let colon_count = trimmed.chars().filter(|&c| c == ':').count();
    if colon_count > 1 {
        return format!("[{trimmed}]:{default_port}");
    }
    if colon_count == 1 {
        return trimmed.to_string();
    }
    format!("{trimmed}:{default_port}")
}

fn build_install_command_args(req: &proto::InstallToDiskRequest) -> Result<Vec<String>, Status> {
    let mut controllers: Vec<String> = req
        .controllers
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(|v| normalize_endpoint(&v, 9090))
        .collect();
    if controllers.is_empty() && !req.controller.trim().is_empty() {
        let c = req.controller.trim().to_string();
        controllers.push(normalize_endpoint(&c, 9090));
    }
    let has_controller = !controllers.is_empty();
    if !has_controller && !req.run_controller {
        return Err(Status::invalid_argument(
            "provide --controller or --run-controller (or both for a joining controller)",
        ));
    }

    let mut args = vec![
        "--disk".to_string(),
        req.os_disk.clone(),
        "--yes".to_string(),
        "--wipe".to_string(),
        "--non-interactive".to_string(),
        "--reboot".to_string(),
    ];
    for dd in &req.data_disks {
        args.push("--data-disk".to_string());
        args.push(dd.clone());
    }
    if has_controller {
        for controller in &controllers {
            args.push("--controller".to_string());
            args.push(controller.clone());
        }
    } else if !req.controller.trim().is_empty() {
        args.push("--controller".to_string());
        args.push(req.controller.trim().to_string());
    }
    if req.run_controller {
        args.push("--run-controller".to_string());
    }
    let typed_mode = match proto::StorageBackendType::try_from(req.storage_backend)
        .unwrap_or(proto::StorageBackendType::Unspecified)
    {
        proto::StorageBackendType::Filesystem => "filesystem",
        proto::StorageBackendType::Lvm => "lvm",
        proto::StorageBackendType::Zfs => "zfs",
        proto::StorageBackendType::Ceph => "ceph",
        proto::StorageBackendType::Unspecified => "",
    };
    let mode = if typed_mode.is_empty() {
        req.data_disk_mode.trim()
    } else {
        typed_mode
    };
    if !mode.is_empty() {
        args.push("--data-disk-mode".to_string());
        args.push(mode.to_string());
    }
    if mode == "lvm" {
        if !req.lvm_vg_name.trim().is_empty() {
            args.push("--lvm-vg-name".to_string());
            args.push(req.lvm_vg_name.trim().to_string());
        }
        if !req.lvm_lv_prefix.trim().is_empty() {
            args.push("--lvm-lv-prefix".to_string());
            args.push(req.lvm_lv_prefix.trim().to_string());
        }
    }
    if mode == "zfs" {
        if !req.zfs_pool_name.trim().is_empty() {
            args.push("--zfs-pool-name".to_string());
            args.push(req.zfs_pool_name.trim().to_string());
        }
        if !req.zfs_dataset_prefix.trim().is_empty() {
            args.push("--zfs-dataset-prefix".to_string());
            args.push(req.zfs_dataset_prefix.trim().to_string());
        }
    }
    if req.disable_vxlan {
        args.push("--disable-vxlan".to_string());
    }
    if !req.dc_id.trim().is_empty() {
        args.push("--dc-id".to_string());
        args.push(req.dc_id.trim().to_string());
    }
    if !req.hostname.trim().is_empty() {
        args.push("--hostname".to_string());
        args.push(req.hostname.trim().to_string());
    }
    if !req.node_id.trim().is_empty() {
        args.push("--node-id".to_string());
        args.push(req.node_id.trim().to_string());
    }
    Ok(args)
}

async fn apply_disk_layout_impl(
    req: proto::ApplyDiskLayoutRequest,
) -> Result<Response<proto::ApplyDiskLayoutResponse>, Status> {
    let mode = read_disk_management_mode();

    if req.disk_layout_nix.trim().is_empty() {
        return Err(Status::invalid_argument("disk_layout_nix cannot be empty"));
    }
    // The underlying tool is still disko; enforce the disko.devices attribute so
    // we fail fast on configuration that cannot possibly be applied.
    if !req.disk_layout_nix.contains("disko.devices") {
        return Err(Status::invalid_argument(
            "disk_layout_nix must define disko.devices",
        ));
    }
    if req.apply && mode != DISK_MODE_CONTROLLER_MANAGED {
        return Ok(Response::new(proto::ApplyDiskLayoutResponse {
            success: false,
            message: "node is in installer-only disk management mode; enable controller-managed mode first"
                .to_string(),
            mode: mode.to_string(),
            refusal_reason: String::new(),
        }));
    }

    // Safe/dangerous classifier runs only for apply mode. Validate-only calls
    // never touch the disks, so they are always permitted.
    if req.apply {
        let targets = classifier::extract_target_devices(&req.disk_layout_nix);
        match lsblk::snapshot().await {
            Ok(snap) => {
                let verdict = classifier::classify_disk_layout(&targets, &snap, KCORE_VOLUME_ROOTS);
                if let Verdict::Dangerous { code, detail } = verdict {
                    info!(
                        refusal_reason = code,
                        detail = %detail,
                        targets = ?targets,
                        "refusing dangerous disk layout apply"
                    );
                    return Ok(Response::new(proto::ApplyDiskLayoutResponse {
                        success: false,
                        message: detail,
                        mode: mode.to_string(),
                        refusal_reason: code.to_string(),
                    }));
                }
            }
            Err(e) => {
                // Fail closed: if we can't confirm the node is idle, refuse.
                error!(error = %e, "lsblk snapshot failed; refusing disk layout apply");
                return Ok(Response::new(proto::ApplyDiskLayoutResponse {
                    success: false,
                    message: format!("could not inspect block devices to classify request: {e}"),
                    mode: mode.to_string(),
                    refusal_reason: "lsblk_probe_failed".to_string(),
                }));
            }
        }
    }

    let timeout_seconds = validate_disk_timeout_seconds_or_default(req.timeout_seconds);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Status::internal(format!("system clock error: {e}")))?
        .as_secs();

    // Always stage the expression in a temp file co-located with the final
    // persisted path so the apply-mode rename is atomic (same filesystem).
    let staging_dir = if req.apply {
        PathBuf::from(DISK_LAYOUT_DIR)
    } else {
        PathBuf::from("/tmp")
    };
    let temp_path = staging_dir.join(format!(".kcore-disk-layout-{timestamp}.nix.tmp"));

    let staging_dir_clone = staging_dir.clone();
    let write_path = temp_path.clone();
    let layout_nix = req.disk_layout_nix.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&staging_dir_clone)?;
        std::fs::write(&write_path, layout_nix)
    })
    .await
    .map_err(|e| Status::internal(format!("task join: {e}")))?
    .map_err(|e| {
        Status::internal(format!(
            "staging disk layout config {}: {e}",
            temp_path.display()
        ))
    })?;

    let run_path = temp_path.clone();
    let mut cmd = Command::new("timeout");
    cmd.args([format!("{timeout_seconds}s")]);
    if req.apply {
        cmd.arg("disko")
            .arg("--mode")
            .arg("format,mount")
            .arg(run_path.as_os_str());
    } else {
        cmd.arg("nix-instantiate")
            .arg("--parse")
            .arg(run_path.as_os_str());
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| Status::internal(format!("running disko command: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        // On any failure, drop the staged temp file; we never promote a failed
        // apply to the persisted path.
        let _ = tokio::fs::remove_file(&temp_path).await;
        let detail = if stderr.is_empty() {
            format!("disko command failed with status {}", output.status)
        } else {
            format!("disko command failed: {stderr}")
        };
        return Ok(Response::new(proto::ApplyDiskLayoutResponse {
            success: false,
            message: detail,
            mode: mode.to_string(),
            refusal_reason: String::new(),
        }));
    }

    // On successful apply, atomically promote the staged temp file to the
    // canonical persisted path so NixOS (via modules/kcore-disko.nix) and the
    // reconciler both observe the layout that was actually realised.
    let persisted = if req.apply {
        let current_path = PathBuf::from(DISK_LAYOUT_CURRENT_PATH);
        let staged = temp_path.clone();
        let target = current_path.clone();
        tokio::task::spawn_blocking(move || std::fs::rename(&staged, &target))
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))?
            .map_err(|e| {
                Status::internal(format!(
                    "persisting disk layout to {}: {e}",
                    current_path.display()
                ))
            })?;
        Some(current_path)
    } else {
        let _ = tokio::fs::remove_file(&temp_path).await;
        None
    };

    // Chain nixos-rebuild test -> switch so the persisted layout is evaluated
    // by NixOS and activated atomically. We fire-and-forget inside a transient
    // systemd unit (same helper ApplyNixConfig uses) to avoid blocking the
    // RPC on what is often a multi-minute rebuild.
    let mut rebuild_scheduled = false;
    if req.apply && req.rebuild {
        if let Some(persisted_path) = persisted.clone() {
            tokio::spawn(async move {
                let apply_id = format!("disk-layout-{}", uuid_like_suffix());
                write_apply_state(&apply_id, NIX_APPLY_RUNNING, "queued");
                run_test_then_switch(persisted_path, Vec::new(), apply_id).await;
            });
            rebuild_scheduled = true;
        }
    }

    let action = if req.apply { "applied" } else { "validated" };
    let base = if stdout.is_empty() {
        format!("disk layout {action} successfully")
    } else {
        format!("disk layout {action} successfully: {stdout}")
    };
    let mut detail = base;
    if let Some(ref path) = persisted {
        detail = format!("{detail}; persisted at {}", path.display());
    }
    if rebuild_scheduled {
        detail = format!("{detail}; nixos-rebuild test+switch started");
    }

    Ok(Response::new(proto::ApplyDiskLayoutResponse {
        success: true,
        message: detail,
        mode: mode.to_string(),
        refusal_reason: String::new(),
    }))
}

#[tonic::async_trait]
impl proto::node_admin_server::NodeAdmin for AdminService {
    async fn list_disks(
        &self,
        request: Request<proto::ListDisksRequest>,
    ) -> Result<Response<proto::ListDisksResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        let disks = tokio::task::spawn_blocking(discovery::list_disks)
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))?
            .map_err(Status::internal)?;
        Ok(Response::new(proto::ListDisksResponse { disks }))
    }

    async fn list_network_interfaces(
        &self,
        request: Request<proto::ListNetworkInterfacesRequest>,
    ) -> Result<Response<proto::ListNetworkInterfacesResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        let interfaces = tokio::task::spawn_blocking(discovery::list_network_interfaces)
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))?
            .map_err(Status::internal)?;
        Ok(Response::new(proto::ListNetworkInterfacesResponse {
            interfaces,
        }))
    }

    async fn get_lvm_info(
        &self,
        request: Request<proto::GetLvmInfoRequest>,
    ) -> Result<Response<proto::GetLvmInfoResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        Ok(Response::new(collect_lvm_info().await))
    }

    async fn apply_nix_config(
        &self,
        request: Request<proto::ApplyNixConfigRequest>,
    ) -> Result<Response<proto::ApplyNixConfigResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let path = self.nix_config_path.clone();

        let write_path = path.clone();
        let config_nix = req.configuration_nix.clone();
        tokio::task::spawn_blocking(move || std::fs::write(&write_path, &config_nix))
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))?
            .map_err(|e| {
                error!(path = %path.display(), error = %e, "failed to write nix config");
                Status::internal(format!("writing {}: {e}", path.display()))
            })?;

        info!(path = %path.display(), "wrote nix config");

        if !req.rebuild {
            return Ok(Response::new(proto::ApplyNixConfigResponse {
                success: true,
                message: format!("config written to {}", path.display()),
                apply_id: String::new(),
            }));
        }

        let apply_id = sanitize_apply_id(&req.apply_id)
            .unwrap_or_else(|| format!("apply-{}", uuid_like_suffix()));
        write_apply_state(&apply_id, NIX_APPLY_RUNNING, "queued");

        let planned_steps = rebuild_sequence(true).join(" -> ");
        info!(path = %path.display(), steps = %planned_steps, %apply_id, "starting background nix apply flow");
        let rebuild_path = path.clone();
        let desired_stopped_vms = parse_stopped_vms_from_nix(&req.configuration_nix);
        let apply_lock = Arc::clone(&self.apply_lock);
        let spawned_id = apply_id.clone();
        tokio::spawn(async move {
            let _guard = apply_lock.lock().await;
            run_test_then_switch(rebuild_path, desired_stopped_vms, spawned_id).await;
        });

        Ok(Response::new(proto::ApplyNixConfigResponse {
            success: true,
            message: format!(
                "config written to {}; nixos-rebuild test+switch started",
                path.display()
            ),
            apply_id,
        }))
    }

    async fn get_nix_apply_status(
        &self,
        request: Request<proto::GetNixApplyStatusRequest>,
    ) -> Result<Response<proto::GetNixApplyStatusResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let apply_id = match sanitize_apply_id(&req.apply_id).or_else(latest_apply_id) {
            Some(id) => id,
            None => {
                return Ok(Response::new(proto::GetNixApplyStatusResponse {
                    apply_id: String::new(),
                    phase: proto::NixApplyPhase::Unknown as i32,
                    message: "no nix apply has run on this node".into(),
                }))
            }
        };
        let (phase, message) = read_apply_state(&apply_id).unwrap_or_else(|| {
            (
                String::new(),
                format!("no record of nix apply '{apply_id}'"),
            )
        });
        Ok(Response::new(proto::GetNixApplyStatusResponse {
            apply_id,
            phase: apply_phase_to_proto(&phase),
            message,
        }))
    }

    async fn apply_ceph_config(
        &self,
        request: Request<proto::ApplyCephConfigRequest>,
    ) -> Result<Response<proto::ApplyCephConfigResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        validate_apply_ceph_config_args(&req.ceph_nix, &req.fsid)?;
        tokio::fs::write(CEPH_NIX_PATH, req.ceph_nix.as_bytes())
            .await
            .map_err(|e| Status::internal(format!("writing {CEPH_NIX_PATH}: {e}")))?;

        let generated = req.keyring.is_empty();
        let pkg = if generated {
            crate::ceph_bootstrap::generate_bootstrap_package(&req.fsid)
                .map_err(Status::internal)?
        } else {
            crate::ceph_bootstrap::decode_package(&req.keyring).map_err(Status::invalid_argument)?
        };
        if pkg.fsid != req.fsid {
            return Err(Status::invalid_argument(
                "bootstrap package fsid does not match request fsid",
            ));
        }
        crate::ceph_bootstrap::write_keyring_files(&pkg).map_err(Status::internal)?;

        if req.rebuild {
            let out = Command::new("nixos-rebuild")
                .args(["test"])
                .output()
                .await
                .map_err(|e| Status::internal(format!("starting nixos-rebuild: {e}")))?;
            if !out.status.success() {
                return Err(Status::internal(format!(
                    "nixos-rebuild test failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let out = Command::new("nixos-rebuild")
                .args(["switch"])
                .output()
                .await
                .map_err(|e| Status::internal(format!("starting nixos-rebuild: {e}")))?;
            if !out.status.success() {
                return Err(Status::internal(format!(
                    "nixos-rebuild switch failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }

        if req.mon {
            let mons = crate::ceph_bootstrap::parse_mon_map(&req.mon_map)
                .map_err(Status::invalid_argument)?;
            crate::ceph_bootstrap::mkfs_mon(&pkg, &req.daemon_id, &mons)
                .map_err(Status::internal)?;
            let _ = Command::new("systemctl")
                .args(["start", &format!("ceph-mon-{}", req.daemon_id)])
                .output()
                .await;
        }
        if req.mgr {
            let daemon = req.daemon_id.clone();
            tokio::task::spawn_blocking(move || crate::ceph_bootstrap::ensure_mgr_keyring(&daemon))
                .await
                .map_err(|e| Status::internal(format!("mgr keyring task: {e}")))?
                .map_err(Status::internal)?;
            let _ = Command::new("systemctl")
                .args(["start", &format!("ceph-mgr-{}", req.daemon_id)])
                .output()
                .await;
        }

        let keyring = if generated {
            crate::ceph_bootstrap::encode_package(&pkg).map_err(Status::internal)?
        } else {
            Vec::new()
        };
        Ok(Response::new(proto::ApplyCephConfigResponse {
            success: true,
            message: format!("Ceph config written to {CEPH_NIX_PATH}; mon/mgr bootstrapped"),
            keyring,
        }))
    }

    async fn bootstrap_ceph_osd(
        &self,
        request: Request<proto::BootstrapCephOsdRequest>,
    ) -> Result<Response<proto::BootstrapCephOsdResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        validate_bootstrap_osd_device(&req.osd_device)?;
        let listed = Command::new("ceph-volume")
            .args(["lvm", "list", "--format", "json"])
            .output()
            .await
            .map_err(|e| {
                Status::internal(format!("starting ceph-volume (is Ceph installed?): {e}"))
            })?;
        if listed.status.success() {
            let stdout = String::from_utf8_lossy(&listed.stdout);
            if stdout.contains(req.osd_device.trim()) {
                return Ok(Response::new(proto::BootstrapCephOsdResponse {
                    success: true,
                    already_prepared: true,
                    message: "OSD already prepared".into(),
                    osd_id: String::new(),
                }));
            }
        }
        let signatures = Command::new("wipefs")
            .args(["--no-act", req.osd_device.as_str()])
            .output()
            .await
            .map_err(|e| Status::internal(format!("running wipefs: {e}")))?;
        if !req.force_wipe && !signatures.stdout.is_empty() {
            return Err(Status::failed_precondition(
                "OSD device has signatures; set forceWipe only after verifying the device",
            ));
        }
        if req.force_wipe {
            let out = Command::new("wipefs")
                .args(["--all", req.osd_device.as_str()])
                .output()
                .await
                .map_err(|e| Status::internal(format!("running wipefs: {e}")))?;
            if !out.status.success() {
                return Err(Status::internal(
                    String::from_utf8_lossy(&out.stderr).to_string(),
                ));
            }
        }
        // Let ceph-volume install/activate systemd units (do not pass --no-systemd).
        let out = Command::new("ceph-volume")
            .args(["lvm", "create", "--data", req.osd_device.as_str()])
            .output()
            .await
            .map_err(|e| {
                Status::internal(format!("starting ceph-volume (is Ceph installed?): {e}"))
            })?;
        if !out.status.success() {
            return Err(Status::internal(format!(
                "ceph-volume failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(Response::new(proto::BootstrapCephOsdResponse {
            success: true,
            already_prepared: false,
            message: "OSD prepared and activated".into(),
            osd_id: String::new(),
        }))
    }

    async fn get_ceph_health(
        &self,
        request: Request<proto::GetCephHealthRequest>,
    ) -> Result<Response<proto::GetCephHealthResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let _ = request.into_inner();
        let out = Command::new("ceph")
            .args(["-s", "--format", "json"])
            .output()
            .await
            .map_err(|e| Status::internal(format!("starting ceph (is Ceph installed?): {e}")))?;
        if !out.status.success() {
            return Err(Status::internal(format!(
                "ceph status failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let raw_status = String::from_utf8_lossy(&out.stdout).to_string();
        let json: JsonValue = serde_json::from_str(&raw_status)
            .map_err(|e| Status::internal(format!("invalid ceph status JSON: {e}")))?;
        let health_status = json
            .pointer("/health/status")
            .and_then(JsonValue::as_str)
            .unwrap_or("HEALTH_ERR")
            .to_string();
        let (osd_up, osd_in) = crate::ceph_bootstrap::parse_osd_counters(&json);
        Ok(Response::new(proto::GetCephHealthResponse {
            health_status,
            raw_status,
            osd_up,
            osd_in,
        }))
    }

    async fn ensure_ceph_pool(
        &self,
        request: Request<proto::EnsureCephPoolRequest>,
    ) -> Result<Response<proto::EnsureCephPoolResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let pool = req.pool.trim();
        if pool.is_empty() {
            return Err(Status::invalid_argument("pool is required"));
        }
        let listed = Command::new("ceph")
            .args(["osd", "pool", "ls"])
            .output()
            .await
            .map_err(|e| Status::internal(format!("ceph osd pool ls: {e}")))?;
        if !listed.status.success() {
            return Err(Status::internal(format!(
                "ceph osd pool ls failed: {}",
                String::from_utf8_lossy(&listed.stderr).trim()
            )));
        }
        let exists = String::from_utf8_lossy(&listed.stdout)
            .lines()
            .any(|l| l.trim() == pool);
        let mut created = false;
        if !exists {
            let out = Command::new("ceph")
                .args(["osd", "pool", "create", pool, "32"])
                .output()
                .await
                .map_err(|e| Status::internal(format!("ceph osd pool create: {e}")))?;
            if !out.status.success() {
                return Err(Status::internal(format!(
                    "ceph osd pool create failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            created = true;
        }
        if req.size > 0 {
            let _ = Command::new("ceph")
                .args(["osd", "pool", "set", pool, "size", &req.size.to_string()])
                .output()
                .await;
        }
        if req.min_size > 0 {
            let _ = Command::new("ceph")
                .args([
                    "osd",
                    "pool",
                    "set",
                    pool,
                    "min_size",
                    &req.min_size.to_string(),
                ])
                .output()
                .await;
        }
        let _ = Command::new("ceph")
            .args(["osd", "pool", "application", "enable", pool, "rbd"])
            .output()
            .await;
        let init = Command::new("rbd")
            .args(["pool", "init", pool])
            .output()
            .await
            .map_err(|e| Status::internal(format!("rbd pool init: {e}")))?;
        if !init.status.success() {
            let err = String::from_utf8_lossy(&init.stderr);
            if !err.contains("already initialized") && !err.is_empty() {
                return Err(Status::internal(format!(
                    "rbd pool init failed: {}",
                    err.trim()
                )));
            }
        }
        Ok(Response::new(proto::EnsureCephPoolResponse {
            success: true,
            created,
            message: if created {
                format!("created and initialized pool {pool}")
            } else {
                format!("pool {pool} already present")
            },
        }))
    }

    async fn prepare_live_migrate_receive(
        &self,
        request: Request<proto::PrepareLiveMigrateReceiveRequest>,
    ) -> Result<Response<proto::PrepareLiveMigrateReceiveResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let vm_name = req.vm_name.trim();
        let pool = req.rbd_pool.trim();
        let image = req.rbd_image.trim();
        if vm_name.is_empty() || pool.is_empty() || image.is_empty() {
            return Err(Status::invalid_argument(
                "vm_name, rbd_pool, and rbd_image are required",
            ));
        }
        if self.live_migrate.get_port(vm_name).await.is_some() {
            return Err(Status::already_exists(format!(
                "live migrate receive already prepared for {vm_name}"
            )));
        }
        // The handoff writes a marker and a pid file that the generated VM unit
        // reads. If the two disagree about where those live, the destination
        // unit cold-starts a second Cloud Hypervisor instead of adopting the
        // migrated one, so refuse the migration rather than corrupt the guest.
        live_migrate::check_socket_dir_matches_nix(&self.vm_socket_dir)
            .map_err(Status::failed_precondition)?;

        if req.listen_port > u16::MAX as i32 || req.listen_port < 0 {
            return Err(Status::invalid_argument(format!(
                "listen_port {} is not a TCP port",
                req.listen_port
            )));
        }
        // Hold a listener on the port until Cloud Hypervisor is about to bind
        // it, so nothing can win the race in between.
        let port = if req.listen_port > 0 {
            self.live_migrate
                .reserve_explicit_port(req.listen_port as u16)
                .await
                .map_err(Status::failed_precondition)?
        } else {
            self.live_migrate
                .reserve_port()
                .await
                .map_err(Status::resource_exhausted)?
        };

        let prepared = self
            .prepare_receive_session(vm_name, pool, image, port)
            .await;
        if prepared.is_err() {
            self.live_migrate.release_port(port).await;
        }
        prepared
    }

    async fn wait_live_migrate_receive(
        &self,
        request: Request<proto::WaitLiveMigrateReceiveRequest>,
    ) -> Result<Response<proto::WaitLiveMigrateReceiveResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let vm_name = req.vm_name.trim();
        if vm_name.is_empty() {
            return Err(Status::invalid_argument("vm_name is required"));
        }
        let timeout = if req.timeout_seconds > 0 {
            std::time::Duration::from_secs(req.timeout_seconds as u64)
        } else {
            std::time::Duration::from_secs(600)
        };
        let Some(session) = self.live_migrate.take(vm_name).await else {
            // Receive may have already completed and been consumed; treat marker as success.
            let marker = live_migrate::handoff_marker_path(&self.vm_socket_dir, vm_name);
            if marker.exists() {
                return Ok(Response::new(proto::WaitLiveMigrateReceiveResponse {
                    success: true,
                    message: "receive already completed".into(),
                }));
            }
            return Err(Status::not_found(format!(
                "no live migrate receive session for {vm_name}"
            )));
        };
        let port = session.port;
        let outcome = tokio::time::timeout(timeout, session.receive_task).await;
        // Either way the port is no longer ours to hold: Cloud Hypervisor owns
        // it on success, and on failure it must go back to the pool.
        self.live_migrate.release_port(port).await;
        outcome
            .map_err(|_| {
                Status::deadline_exceeded(format!(
                    "timed out after {}s waiting for {vm_name} to finish arriving",
                    timeout.as_secs()
                ))
            })?
            .map_err(|e| Status::internal(format!("receive task for {vm_name} panicked: {e}")))?
            .map_err(|e| {
                Status::internal(format!("receiving live migration for {vm_name}: {e}"))
            })?;
        Ok(Response::new(proto::WaitLiveMigrateReceiveResponse {
            success: true,
            message: "live migration receive completed".into(),
        }))
    }

    async fn abort_live_migrate_receive(
        &self,
        request: Request<proto::AbortLiveMigrateReceiveRequest>,
    ) -> Result<Response<proto::AbortLiveMigrateReceiveResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let vm_name = req.vm_name.trim();
        if vm_name.is_empty() {
            return Err(Status::invalid_argument("vm_name is required"));
        }
        let observed = self
            .clear_receive_session(vm_name, req.rbd_pool.trim(), req.rbd_image.trim())
            .await;
        Ok(Response::new(proto::AbortLiveMigrateReceiveResponse {
            success: true,
            message: "aborted live migrate receive".into(),
            observed: Some(receive_state_to_proto(&observed)),
        }))
    }

    async fn get_live_migrate_receive_status(
        &self,
        request: Request<proto::GetLiveMigrateReceiveStatusRequest>,
    ) -> Result<Response<proto::GetLiveMigrateReceiveStatusResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let vm_name = req.vm_name.trim();
        if vm_name.is_empty() {
            return Err(Status::invalid_argument("vm_name is required"));
        }
        let observed =
            live_migrate::observe_receive(&self.live_migrate, &self.vm_socket_dir, vm_name).await;
        Ok(Response::new(proto::GetLiveMigrateReceiveStatusResponse {
            success: true,
            message: observed.summary(),
            state: Some(receive_state_to_proto(&observed)),
        }))
    }

    async fn send_live_migrate(
        &self,
        request: Request<proto::SendLiveMigrateRequest>,
    ) -> Result<Response<proto::SendLiveMigrateResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let vm_name = req.vm_name.trim();
        let dest = req.destination_url.trim();
        if vm_name.is_empty() || dest.is_empty() {
            return Err(Status::invalid_argument(
                "vm_name and destination_url are required",
            ));
        }
        if !dest.starts_with("tcp:") {
            return Err(Status::invalid_argument(
                "destination_url must start with tcp:",
            ));
        }
        let unit = live_migrate::vm_unit_name(vm_name);
        live_migrate::disable_unit_restart(&unit)
            .await
            .map_err(|e| {
                Status::internal(format!(
                    "disabling systemd restart for {unit} before sending: {e}"
                ))
            })?;
        let client = vmm::Client::new(self.vm_socket_dir.to_str().unwrap_or("/run/kcore"));
        let timeout = if req.timeout_seconds > 0 {
            std::time::Duration::from_secs(req.timeout_seconds as u64)
        } else {
            std::time::Duration::from_secs(600)
        };
        let send_fut = client.send_migration(vm_name, dest);
        tokio::time::timeout(timeout, send_fut)
            .await
            .map_err(|_| {
                Status::deadline_exceeded(format!(
                    "timed out after {}s sending {vm_name} to {dest}",
                    timeout.as_secs()
                ))
            })?
            .map_err(|e| {
                Status::internal(format!(
                    "cloud-hypervisor refused to send {vm_name} to {dest}: {e}"
                ))
            })?;
        Ok(Response::new(proto::SendLiveMigrateResponse {
            success: true,
            message: format!("sent migration for {vm_name} to {dest}"),
        }))
    }

    async fn finalize_live_migrate_source(
        &self,
        request: Request<proto::FinalizeLiveMigrateSourceRequest>,
    ) -> Result<Response<proto::FinalizeLiveMigrateSourceResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let vm_name = req.vm_name.trim();
        if vm_name.is_empty() {
            return Err(Status::invalid_argument("vm_name is required"));
        }
        let unit = live_migrate::vm_unit_name(vm_name);
        let _ = live_migrate::disable_unit_restart(&unit).await;
        if let Err(e) = live_migrate::stop_unit(&unit).await {
            // Unit may already be inactive after CH exited on send.
            warn!(error = %e, unit = %unit, "finalize source stop");
        }
        let pool = req.rbd_pool.trim();
        let image = req.rbd_image.trim();
        let has_rbd = !pool.is_empty() && !image.is_empty();
        if has_rbd {
            live_migrate::ensure_rbd_unmapped(pool, image)
                .map_err(|e| Status::internal(format!("unmap RBD on source: {e}")))?;
        }
        // Report observed state, not the fact that we asked: the controller
        // uses these to decide whether it is safe to start the VM elsewhere.
        let vmm_stopped = live_migrate::unit_is_stopped(&unit)
            .await
            .map_err(Status::internal)?;
        let rbd_unmapped = !has_rbd || !live_migrate::rbd_is_mapped(pool, image);
        Ok(Response::new(proto::FinalizeLiveMigrateSourceResponse {
            success: vmm_stopped && rbd_unmapped,
            message: if vmm_stopped && rbd_unmapped {
                format!("finalized source after migrate of {vm_name}")
            } else {
                format!(
                    "source not fully released for {vm_name} (vmm_stopped={vmm_stopped}, rbd_unmapped={rbd_unmapped})"
                )
            },
            vmm_stopped,
            rbd_unmapped,
        }))
    }

    async fn finalize_live_migrate_dest(
        &self,
        request: Request<proto::FinalizeLiveMigrateDestRequest>,
    ) -> Result<Response<proto::FinalizeLiveMigrateDestResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let vm_name = req.vm_name.trim();
        if vm_name.is_empty() {
            return Err(Status::invalid_argument("vm_name is required"));
        }
        let unit = live_migrate::vm_unit_name(vm_name);
        // The unit only exists once the destination's nixos-rebuild activated,
        // and that runs asynchronously. Starting a unit systemd has never heard
        // of would fail the migration for a reason that fixes itself.
        live_migrate::wait_for_unit_loaded(&unit, UNIT_LOADED_TIMEOUT)
            .await
            .map_err(Status::failed_precondition)?;
        let out = Command::new("systemctl")
            .args(["start", &unit])
            .output()
            .await
            .map_err(|e| Status::internal(format!("systemctl start {unit}: {e}")))?;
        if !out.status.success() {
            return Err(Status::internal(format!(
                "systemctl start {unit} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(Response::new(proto::FinalizeLiveMigrateDestResponse {
            success: true,
            message: format!("adopted migrated VM via {unit}"),
        }))
    }

    async fn apply_disk_layout(
        &self,
        request: Request<proto::ApplyDiskLayoutRequest>,
    ) -> Result<Response<proto::ApplyDiskLayoutResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        apply_disk_layout_impl(req).await
    }

    async fn apply_disko_layout(
        &self,
        request: Request<proto::ApplyDiskoLayoutRequest>,
    ) -> Result<Response<proto::ApplyDiskoLayoutResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        let legacy = request.into_inner();
        let translated = proto::ApplyDiskLayoutRequest {
            disk_layout_nix: legacy.disko_nix,
            apply: legacy.apply,
            timeout_seconds: legacy.timeout_seconds,
            rebuild: false,
        };
        let resp = apply_disk_layout_impl(translated).await?.into_inner();
        Ok(Response::new(proto::ApplyDiskoLayoutResponse {
            success: resp.success,
            message: resp.message,
            mode: resp.mode,
        }))
    }

    async fn ensure_image(
        &self,
        request: Request<proto::EnsureImageRequest>,
    ) -> Result<Response<proto::EnsureImageResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let storage = Arc::clone(&self.storage);
        let resp = tokio::task::spawn_blocking(move || {
            storage
                .ensure_image(storage::EnsureImageRequest {
                    image_url: req.image_url,
                    image_sha256: req.image_sha256,
                    destination_path: req.destination_path,
                })
                .map(storage::ensure_image_response)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(resp))
    }

    async fn upload_image(
        &self,
        request: Request<proto::UploadImageRequest>,
    ) -> Result<Response<proto::UploadImageResponse>, Status> {
        auth::require_peer(&request, &[CN_KCTL])?;
        let req = request.into_inner();
        let storage = Arc::clone(&self.storage);
        let resp = tokio::task::spawn_blocking(move || {
            storage
                .upload_image(storage::UploadImageRequest {
                    image_bytes: req.image_bytes,
                    source_name: req.source_name,
                    destination_name: req.destination_name,
                    image_format: req.image_format,
                    image_sha256: req.image_sha256,
                })
                .map(storage::upload_image_response)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(resp))
    }

    async fn upload_image_stream(
        &self,
        request: Request<tonic::Streaming<proto::UploadImageChunk>>,
    ) -> Result<Response<proto::UploadImageResponse>, Status> {
        auth::require_peer(&request, &[CN_KCTL])?;
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("upload stream is empty"))?;

        let source_name = first.source_name.clone();
        let destination_name = first.destination_name.clone();
        let image_format = first.image_format.clone();
        let image_sha256 = first.image_sha256.clone();

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Status::internal(format!("system clock error: {e}")))?
            .as_millis();
        let tmp_path = PathBuf::from(format!("/tmp/kcore-upload-{timestamp}.part"));
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| Status::internal(format!("creating {}: {e}", tmp_path.display())))?;

        if !first.chunk_data.is_empty() {
            tokio::io::AsyncWriteExt::write_all(&mut file, &first.chunk_data)
                .await
                .map_err(|e| Status::internal(format!("writing {}: {e}", tmp_path.display())))?;
        }

        while let Some(chunk) = stream.message().await? {
            if !chunk.image_format.trim().is_empty()
                && chunk.image_format.trim() != image_format.trim()
            {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(Status::invalid_argument(
                    "image_format must be consistent across stream chunks",
                ));
            }
            if !chunk.image_sha256.trim().is_empty()
                && chunk.image_sha256.trim() != image_sha256.trim()
            {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(Status::invalid_argument(
                    "image_sha256 must be consistent across stream chunks",
                ));
            }
            if !chunk.source_name.trim().is_empty()
                && chunk.source_name.trim() != source_name.trim()
            {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(Status::invalid_argument(
                    "source_name must be consistent across stream chunks",
                ));
            }
            if !chunk.destination_name.trim().is_empty()
                && chunk.destination_name.trim() != destination_name.trim()
            {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(Status::invalid_argument(
                    "destination_name must be consistent across stream chunks",
                ));
            }
            if !chunk.chunk_data.is_empty() {
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk.chunk_data)
                    .await
                    .map_err(|e| {
                        Status::internal(format!("writing {}: {e}", tmp_path.display()))
                    })?;
            }
        }

        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|e| Status::internal(format!("flushing {}: {e}", tmp_path.display())))?;
        drop(file);

        let storage = Arc::clone(&self.storage);
        let tmp_path_for_upload = tmp_path.clone();
        let resp = tokio::task::spawn_blocking(move || {
            storage
                .upload_image_from_path(storage::UploadImageFromPathRequest {
                    source_file_path: tmp_path_for_upload.display().to_string(),
                    source_name,
                    destination_name,
                    image_format,
                    image_sha256,
                })
                .map(storage::upload_image_response)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;

        let _ = tokio::fs::remove_file(&tmp_path).await;
        Ok(Response::new(resp))
    }

    async fn check_vm_ssh_ready(
        &self,
        request: Request<proto::CheckVmSshReadyRequest>,
    ) -> Result<Response<proto::CheckVmSshReadyResponse>, Status> {
        auth::require_peer(&request, &[CN_KCTL])?;
        let req = request.into_inner();
        let vm_name = req.vm_name.trim();
        if vm_name.is_empty() {
            return Err(Status::invalid_argument("vm_name is required"));
        }
        let port = validate_port_or_default(req.port);
        let timeout_ms = validate_timeout_ms_or_default(req.timeout_ms);
        if let Some(unit_state) = read_vm_unit_state(vm_name).await {
            if vm_unit_is_fatal(&unit_state) {
                let hint = vm_recent_failure_hint(vm_name)
                    .await
                    .unwrap_or_else(|| "see journalctl for VM unit details".to_string());
                return Ok(Response::new(proto::CheckVmSshReadyResponse {
                    ready: false,
                    ip: String::new(),
                    port: port as i32,
                    reason: format!(
                        "VM unit {} is failing (active={}, sub={}, result={}, restarts={}): {}",
                        vm_unit_name(vm_name),
                        unit_state.active_state,
                        unit_state.sub_state,
                        unit_state.result,
                        unit_state.n_restarts,
                        hint
                    ),
                    fatal: true,
                }));
            }
        }
        let vmm = crate::vmm::Client::new(&self.vm_socket_dir.display().to_string());
        let vm_info = vmm.get_vm_info(vm_name).await;
        let vm_mac = vm_info.as_ref().and_then(vm_primary_mac);
        let lease_files = lease_files_for_network(&self.vm_socket_dir, &req.network);
        if lease_files.is_empty() {
            return Ok(Response::new(proto::CheckVmSshReadyResponse {
                ready: false,
                ip: String::new(),
                port: port as i32,
                reason: format!(
                    "no dnsmasq lease files found in {}",
                    self.vm_socket_dir.display()
                ),
                fatal: false,
            }));
        }

        let mut vm_ip = None;
        for lease in &lease_files {
            if let Some(ip) = find_vm_ip_in_lease_file(lease, vm_name, vm_mac.as_deref()) {
                vm_ip = Some(ip);
                break;
            }
        }
        let Some(ip) = vm_ip else {
            if let Some(mac) = vm_mac.as_deref() {
                if let Some(ip) = find_vm_ip_in_neigh(mac, &req.network).await {
                    let connect = tokio::time::timeout(
                        std::time::Duration::from_millis(timeout_ms),
                        tokio::net::TcpStream::connect((ip.as_str(), port)),
                    )
                    .await;
                    return match connect {
                        Ok(Ok(_stream)) => Ok(Response::new(proto::CheckVmSshReadyResponse {
                            ready: true,
                            ip,
                            port: port as i32,
                            reason: "ssh port reachable (ip discovered via arp/neigh)".to_string(),
                            fatal: false,
                        })),
                        Ok(Err(e)) => Ok(Response::new(proto::CheckVmSshReadyResponse {
                            ready: false,
                            ip,
                            port: port as i32,
                            reason: format!("arp/neigh found IP but tcp connect failed: {e}"),
                            fatal: false,
                        })),
                        Err(_) => Ok(Response::new(proto::CheckVmSshReadyResponse {
                            ready: false,
                            ip,
                            port: port as i32,
                            reason: format!(
                                "arp/neigh found IP but tcp connect timed out after {timeout_ms}ms"
                            ),
                            fatal: false,
                        })),
                    };
                }
            }
            return Ok(Response::new(proto::CheckVmSshReadyResponse {
                ready: false,
                ip: String::new(),
                port: port as i32,
                reason: if vm_mac.is_some() {
                    "no DHCP lease found for VM yet (and no arp/neigh match for VM MAC)".to_string()
                } else {
                    "no DHCP lease found for VM yet (VM MAC unavailable)".to_string()
                },
                fatal: false,
            }));
        };

        let connect = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            tokio::net::TcpStream::connect((ip.as_str(), port)),
        )
        .await;
        match connect {
            Ok(Ok(_stream)) => Ok(Response::new(proto::CheckVmSshReadyResponse {
                ready: true,
                ip,
                port: port as i32,
                reason: "ssh port reachable".to_string(),
                fatal: false,
            })),
            Ok(Err(e)) => Ok(Response::new(proto::CheckVmSshReadyResponse {
                ready: false,
                ip,
                port: port as i32,
                reason: format!("tcp connect failed: {e}"),
                fatal: false,
            })),
            Err(_) => Ok(Response::new(proto::CheckVmSshReadyResponse {
                ready: false,
                ip,
                port: port as i32,
                reason: format!("tcp connect timed out after {timeout_ms}ms"),
                fatal: false,
            })),
        }
    }

    type AttachVmConsoleStream = crate::console::ConsoleOutboundStream;

    async fn attach_vm_console(
        &self,
        request: Request<tonic::Streaming<proto::ConsoleMessage>>,
    ) -> Result<Response<Self::AttachVmConsoleStream>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("console stream is empty"))?;
        if first.vm_name.trim().is_empty() {
            return Err(Status::invalid_argument(
                "vm_name is required on the first console message",
            ));
        }
        let outbound =
            crate::console::bridge_console_session(&self.vm_socket_dir, first, inbound).await?;
        Ok(Response::new(outbound))
    }

    async fn install_to_disk(
        &self,
        request: Request<proto::InstallToDiskRequest>,
    ) -> Result<Response<proto::InstallToDiskResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL])?;
        let req = request.into_inner();
        if req.os_disk.is_empty() {
            return Err(Status::invalid_argument("os_disk is required"));
        }
        validate_disk_path(&req.os_disk, "os_disk")?;
        for (i, dd) in req.data_disks.iter().enumerate() {
            validate_disk_path(dd, &format!("data_disks[{i}]"))?;
        }

        let resp =
            tokio::task::spawn_blocking(move || -> Result<proto::InstallToDiskResponse, Status> {
                write_bootstrap_pki(&req)?;

                let mut args = build_install_command_args(&req)?;

                let tpm_present = std::path::Path::new("/sys/class/tpm/tpm0").exists();
                let luks_method = if tpm_present { "tpm2" } else { "key-file" };
                args.push("--luks-method".to_string());
                args.push(luks_method.to_string());

                let cmd_str = format!("install-to-disk {}", args.join(" "));
                let (mut log_file, log_path) = prepare_install_log()?;
                use std::io::Write as _;
                writeln!(log_file, "Starting install command: {cmd_str}").map_err(|e| {
                    Status::internal(format!("writing {}: {e}", log_path.display()))
                })?;
                let stderr_log = log_file.try_clone().map_err(|e| {
                    Status::internal(format!("cloning {}: {e}", log_path.display()))
                })?;

                let spawn_result = std::process::Command::new("install-to-disk")
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::from(log_file))
                    .stderr(std::process::Stdio::from(stderr_log))
                    .spawn();

                match spawn_result {
                    Ok(child) => {
                        let pid = child.id();
                        info!(pid, log_path = %log_path.display(), "started install-to-disk");
                        Ok(proto::InstallToDiskResponse {
                            accepted: true,
                            message: format!(
                                "install started (pid {pid}): {cmd_str}; logs: {}",
                                log_path.display()
                            ),
                            luks_method: luks_method.to_string(),
                        })
                    }
                    Err(e) => Err(Status::internal(format!("failed to start install: {e}"))),
                }
            })
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))?;

        resp.map(Response::new)
    }

    async fn prepare_system_update(
        &self,
        request: Request<proto::PrepareSystemUpdateRequest>,
    ) -> Result<Response<proto::PrepareSystemUpdateResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let update_name = sanitize_update_name(&req.update_name)?;
        let flake_ref = req.flake_ref.trim();
        if flake_ref.is_empty() {
            return Err(Status::invalid_argument("flake_ref is required"));
        }
        let flake_rev = req.flake_rev.trim();
        let timeout_sec = if req.timeout_seconds > 0 {
            req.timeout_seconds as u64
        } else {
            3600u64
        };
        let host_system = if req.host_system.trim().is_empty() {
            "x86_64-linux".to_string()
        } else {
            req.host_system.trim().to_string()
        };

        let flake_url = flake_url_with_rev(flake_ref, flake_rev);
        let manifest_path = PathBuf::from("/var/lib/kcore/updates")
            .join(&update_name)
            .join("manifest.json");

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_sec),
            tokio::task::spawn_blocking(move || {
                prepare_system_update_blocking(flake_url, host_system, manifest_path)
            }),
        )
        .await;

        match result {
            Err(_) => Ok(Response::new(proto::PrepareSystemUpdateResponse {
                success: false,
                message: "nix prepare timed out".into(),
                prepared_closure: String::new(),
                manifest_path: String::new(),
                current_generation: String::new(),
                target_generation: String::new(),
                requires_reboot: false,
            })),
            Ok(join_res) => match join_res {
                Ok(Ok(manifest)) => Ok(Response::new(proto::PrepareSystemUpdateResponse {
                    success: true,
                    message: "manifest written".into(),
                    prepared_closure: manifest.manifest_path.clone(),
                    manifest_path: manifest.manifest_path,
                    current_generation: manifest.current_generation,
                    target_generation: manifest.target_generation,
                    requires_reboot: manifest.requires_reboot,
                })),
                Ok(Err(e)) => Ok(Response::new(proto::PrepareSystemUpdateResponse {
                    success: false,
                    message: e,
                    prepared_closure: String::new(),
                    manifest_path: String::new(),
                    current_generation: String::new(),
                    target_generation: String::new(),
                    requires_reboot: false,
                })),
                Err(join_e) => Err(Status::internal(format!("prepare join: {join_e}"))),
            },
        }
    }

    async fn activate_system_update(
        &self,
        request: Request<proto::ActivateSystemUpdateRequest>,
    ) -> Result<Response<proto::ActivateSystemUpdateResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let update_name = sanitize_update_name(&req.update_name)?;
        let manifest_path = if req.prepared_closure.trim().ends_with(".json") {
            PathBuf::from(req.prepared_closure.trim())
        } else {
            PathBuf::from("/var/lib/kcore/updates")
                .join(&update_name)
                .join("manifest.json")
        };
        let mode = req.activation_mode.trim();
        if mode != "test"
            && mode != "switch"
            && mode != "boot"
            && mode != "auto"
            && !mode.is_empty()
        {
            return Err(Status::invalid_argument(
                "activation_mode must be test|switch|boot|auto",
            ));
        }
        let effective_mode: String = if mode.is_empty() || mode == "auto" {
            "switch".into()
        } else {
            mode.to_string()
        };
        if effective_mode == "test" {
            return Ok(Response::new(proto::ActivateSystemUpdateResponse {
                success: true,
                message: "test mode: manifest prepared only; no activation performed".into(),
            }));
        }

        let manifest_path_clone = manifest_path.clone();
        let res = tokio::task::spawn_blocking(move || {
            activate_system_update_blocking(&manifest_path_clone, effective_mode)
        })
        .await
        .map_err(|e| Status::internal(format!("activate join: {e}")))?;

        match res {
            Ok(msg) => Ok(Response::new(proto::ActivateSystemUpdateResponse {
                success: true,
                message: msg,
            })),
            Err(e) => Ok(Response::new(proto::ActivateSystemUpdateResponse {
                success: false,
                message: e,
            })),
        }
    }

    async fn get_system_update_status(
        &self,
        request: Request<proto::GetSystemUpdateStatusRequest>,
    ) -> Result<Response<proto::GetSystemUpdateStatusResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let update_name = sanitize_update_name(&req.update_name)?;
        let manifest_path = PathBuf::from("/var/lib/kcore/updates")
            .join(&update_name)
            .join("manifest.json");

        let summary = tokio::task::spawn_blocking(systemctl_failed_summary)
            .await
            .map_err(|e| Status::internal(format!("join: {e}")))?;

        let phase = if manifest_path.exists() {
            "prepared"
        } else {
            "unknown"
        };
        let closure = manifest_path.display().to_string();

        Ok(Response::new(proto::GetSystemUpdateStatusResponse {
            success: true,
            message: String::new(),
            phase: phase.into(),
            prepared_closure: closure,
            systemctl_failed_summary: summary,
            booted_generation: String::new(),
            current_generation: String::new(),
        }))
    }

    async fn rollback_system_update(
        &self,
        request: Request<proto::RollbackSystemUpdateRequest>,
    ) -> Result<Response<proto::RollbackSystemUpdateResponse>, Status> {
        auth::require_peer_insecure_ok(&request, &[CN_KCTL, CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let update_name = sanitize_update_name(&req.update_name)?;
        let rollback_dir = PathBuf::from("/var/lib/kcore/updates")
            .join(&update_name)
            .join("rollback_bin");

        let res = tokio::task::spawn_blocking(move || rollback_blocking(&rollback_dir))
            .await
            .map_err(|e| Status::internal(format!("rollback join: {e}")))?;

        match res {
            Ok(msg) => Ok(Response::new(proto::RollbackSystemUpdateResponse {
                success: true,
                message: msg,
            })),
            Err(e) => Ok(Response::new(proto::RollbackSystemUpdateResponse {
                success: false,
                message: e,
            })),
        }
    }

    async fn rotate_node_cert(
        &self,
        request: Request<proto::RotateNodeCertRequest>,
    ) -> Result<Response<proto::RotateNodeCertResponse>, Status> {
        // Only the controller may drive rotation; kctl goes through the
        // controller's RotateNodeCerts RPC so the inventory stays authoritative.
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();

        let pki = self.pki.as_ref().ok_or_else(|| {
            Status::unimplemented(
                "this node-agent has no enrolled TLS identity; certificate rotation is unavailable",
            )
        })?;

        let _guard = self.rotate_lock.lock().await;
        match crate::pki::rotate::rotate_once(
            &pki.cfg,
            &pki.cfg.cert_rotation,
            req.force,
            &req.reason,
            &pki.reload,
        )
        .await
        {
            Ok(outcome) => Ok(Response::new(proto::RotateNodeCertResponse {
                success: true,
                message: outcome.message,
                serial_hex: outcome.serial_hex,
                skipped: outcome.skipped,
                days_until_expiry: outcome.days_until_expiry,
            })),
            // A failed rotation is reported in-band rather than as a Status:
            // the node is still healthy on its previous certificate, and the
            // controller's reconciler logs the message and retries next tick.
            Err(error) => {
                warn!(%error, node_id = %pki.cfg.node_id, "certificate rotation failed; keeping the existing certificate");
                Ok(Response::new(proto::RotateNodeCertResponse {
                    success: false,
                    message: error,
                    serial_hex: String::new(),
                    skipped: false,
                    days_until_expiry: 0,
                }))
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct UpdateManifest {
    flake_url: String,
    host_system: String,
    bins: std::collections::HashMap<String, String>,
}

struct PrepareOk {
    manifest_path: String,
    current_generation: String,
    target_generation: String,
    requires_reboot: bool,
}

fn sanitize_update_name(name: &str) -> Result<String, Status> {
    let t = name.trim();
    if t.is_empty() {
        return Err(Status::invalid_argument("update_name is required"));
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(Status::invalid_argument(
            "update_name must be alphanumeric with - _ . only",
        ));
    }
    Ok(t.to_string())
}

fn flake_url_with_rev(flake_ref: &str, flake_rev: &str) -> String {
    if flake_rev.is_empty() || flake_ref.contains("?rev=") || flake_ref.contains("&rev=") {
        flake_ref.to_string()
    } else if flake_ref.contains('?') {
        format!("{flake_ref}&rev={flake_rev}")
    } else {
        format!("{flake_ref}?rev={flake_rev}")
    }
}

fn prepare_system_update_blocking(
    flake_url: String,
    host_system: String,
    manifest_path: PathBuf,
) -> Result<PrepareOk, String> {
    let pkgs = [
        "kcore-node-agent",
        "kcore-controller",
        "kctl",
        "kcore-dashboard",
        "kcore-console",
    ];
    let mut bins = std::collections::HashMap::new();
    for pkg in pkgs {
        let attr = format!("{flake_url}#packages.{host_system}.{pkg}");
        let out = std::process::Command::new("nix")
            .args([
                "build",
                &attr,
                "--no-link",
                "--accept-flake-config",
                "--print-out-paths",
            ])
            .output()
            .map_err(|e| format!("nix build {pkg}: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "nix build failed for {pkg}: {}",
                stderr.trim().chars().take(500).collect::<String>()
            ));
        }
        let store_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if store_path.is_empty() {
            return Err(format!("nix build returned no path for {pkg}"));
        }
        let bin_path = Path::new(&store_path).join("bin").join(pkg);
        if !bin_path.exists() {
            return Err(format!(
                "built package has no bin/{pkg} at {}",
                bin_path.display()
            ));
        }
        bins.insert(pkg.to_string(), bin_path.to_string_lossy().to_string());
    }

    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir updates dir: {e}"))?;
    }
    let mf = UpdateManifest {
        flake_url: flake_url.clone(),
        host_system: host_system.clone(),
        bins,
    };
    let json = serde_json::to_string_pretty(&mf).map_err(|e| format!("serialize manifest: {e}"))?;
    std::fs::write(&manifest_path, json).map_err(|e| format!("write manifest: {e}"))?;

    Ok(PrepareOk {
        manifest_path: manifest_path.to_string_lossy().to_string(),
        current_generation: String::new(),
        target_generation: String::new(),
        requires_reboot: false,
    })
}

fn activate_system_update_blocking(manifest_path: &Path, _mode: String) -> Result<String, String> {
    let raw = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("read manifest {}: {e}", manifest_path.display()))?;
    let mf: UpdateManifest =
        serde_json::from_str(&raw).map_err(|e| format!("parse manifest: {e}"))?;

    let opt_bin = Path::new("/opt/kcore/bin");
    std::fs::create_dir_all(opt_bin).map_err(|e| format!("mkdir /opt/kcore/bin: {e}"))?;

    let rollback = manifest_path
        .parent()
        .ok_or_else(|| "manifest has no parent".to_string())?
        .join("rollback_bin");
    std::fs::create_dir_all(&rollback).map_err(|e| format!("rollback dir: {e}"))?;

    // Replace each binary atomically. We copy the store output into a sibling
    // tmpfile (same directory as the destination, so `rename(2)` is atomic on
    // the same filesystem) and then rename it over the destination. This is
    // critical because `kcore-node-agent` may currently be executing from the
    // destination path: a naive `std::fs::copy` opens the dest with O_TRUNC,
    // which would overwrite a running ELF and crash this very process. Using
    // rename keeps the old inode alive for the running kernel mapping while
    // the new file takes the path.
    for (name, store_bin) in &mf.bins {
        let dest = opt_bin.join(name);
        if dest.exists() {
            let bak = rollback.join(name);
            std::fs::copy(&dest, &bak).map_err(|e| format!("backup {}: {e}", dest.display()))?;
        }
        let tmp = opt_bin.join(format!(".{name}.kcore-update-tmp"));
        if tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }
        std::fs::copy(store_bin, &tmp)
            .map_err(|e| format!("copy {} -> {}: {e}", store_bin, tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&tmp)
                .map_err(|e| format!("meta {}: {e}", tmp.display()))?
                .permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&tmp, perm)
                .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &dest)
            .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), dest.display()))?;
    }

    let status_path = manifest_path
        .parent()
        .expect("parent")
        .join("last_activate.json");
    let _ = std::fs::write(
        &status_path,
        serde_json::json!({ "ok": true, "ts": "now" }).to_string(),
    );

    // Restart sibling services synchronously (they don't kill us).
    for unit in ["kcore-controller.service", "kcore-dashboard.service"] {
        let out = std::process::Command::new("systemctl")
            .args(["try-restart", unit])
            .output();
        if let Ok(o) = out {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!(unit, stderr = %stderr.trim(), "try-restart failed");
            }
        }
    }

    // Restart the node-agent itself out-of-band, so we can return this RPC
    // reply BEFORE the kernel sends SIGTERM. `systemd-run --on-active=…` schedules
    // a transient timer that runs the restart command after a delay.
    let _ = std::process::Command::new("systemd-run")
        .args([
            "--collect",
            "--unit=kcore-node-agent-restart.service",
            "--on-active=3sec",
            "--",
            "systemctl",
            "try-restart",
            "kcore-node-agent.service",
        ])
        .output();

    Ok("installed kcore binaries to /opt/kcore/bin and scheduled restarts".into())
}

fn systemctl_failed_summary() -> String {
    let out = std::process::Command::new("systemctl")
        .args(["--failed", "--no-pager", "--plain"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(e) => format!("(systemctl unavailable: {e})"),
    }
}

fn rollback_blocking(rollback_dir: &Path) -> Result<String, String> {
    if !rollback_dir.exists() {
        return Err("no rollback snapshot (rollback_bin missing)".into());
    }
    let opt_bin = Path::new("/opt/kcore/bin");
    for entry in std::fs::read_dir(rollback_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let src = entry.path();
        let name_str = name.to_string_lossy().to_string();
        let dest = opt_bin.join(&name);
        let tmp = opt_bin.join(format!(".{name_str}.kcore-rollback-tmp"));
        if tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }
        std::fs::copy(&src, &tmp).map_err(|e| format!("stage {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&tmp) {
                let mut perm = meta.permissions();
                perm.set_mode(0o755);
                let _ = std::fs::set_permissions(&tmp, perm);
            }
        }
        std::fs::rename(&tmp, &dest)
            .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), dest.display()))?;
    }
    for unit in ["kcore-controller.service", "kcore-dashboard.service"] {
        let _ = std::process::Command::new("systemctl")
            .args(["try-restart", unit])
            .output();
    }
    let _ = std::process::Command::new("systemd-run")
        .args([
            "--collect",
            "--unit=kcore-node-agent-rollback-restart.service",
            "--on-active=3sec",
            "--",
            "systemctl",
            "try-restart",
            "kcore-node-agent.service",
        ])
        .output();
    Ok("restored prior binaries from rollback_bin and scheduled restarts".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Request;

    #[test]
    fn validate_apply_ceph_config_args_requires_nix_and_fsid() {
        assert!(validate_apply_ceph_config_args("{ }", "fsid").is_ok());
        assert!(validate_apply_ceph_config_args("", "fsid").is_err());
        assert!(validate_apply_ceph_config_args("{ }", "  ").is_err());
        assert!(validate_apply_ceph_config_args("  ", "").is_err());
    }

    #[test]
    fn validate_bootstrap_osd_device_requires_absolute_path() {
        assert!(validate_bootstrap_osd_device("/dev/nvme0n1").is_ok());
        assert!(validate_bootstrap_osd_device("nvme0n1").is_err());
        assert!(validate_bootstrap_osd_device("  ").is_err());
    }

    #[test]
    fn bootstrap_pki_writes_supplied_materials_with_correct_permissions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cert_dir = temp.path().join("certs");
        let req = proto::InstallToDiskRequest {
            os_disk: "/dev/sda".to_string(),
            data_disks: vec![],
            controller: String::new(),
            run_controller: false,
            ca_cert_pem: "ca".to_string(),
            node_cert_pem: "node-cert".to_string(),
            node_key_pem: "node-key".to_string(),
            controller_cert_pem: String::new(),
            controller_key_pem: String::new(),
            kctl_cert_pem: String::new(),
            kctl_key_pem: String::new(),
            data_disk_mode: String::new(),
            storage_backend: proto::StorageBackendType::Unspecified as i32,
            lvm_vg_name: String::new(),
            lvm_lv_prefix: String::new(),
            zfs_pool_name: String::new(),
            zfs_dataset_prefix: String::new(),
            disable_vxlan: false,
            sub_ca_cert_pem: String::new(),
            sub_ca_key_pem: String::new(),
            controllers: Vec::new(),
            dc_id: String::new(),
            hostname: String::new(),
            node_id: String::new(),
        };

        write_bootstrap_pki_at(&req, &cert_dir).expect("write certs");

        assert_eq!(
            std::fs::read_to_string(cert_dir.join("ca.crt")).expect("ca"),
            "ca"
        );
        assert_eq!(
            std::fs::read_to_string(cert_dir.join("node.crt")).expect("node cert"),
            "node-cert"
        );
        assert_eq!(
            std::fs::read_to_string(cert_dir.join("node.key")).expect("node key"),
            "node-key"
        );
        assert!(!cert_dir.join("controller.crt").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ca_mode = std::fs::metadata(cert_dir.join("ca.crt"))
                .expect("ca meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(ca_mode, 0o644, "certs should be world-readable");

            let key_mode = std::fs::metadata(cert_dir.join("node.key"))
                .expect("key meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(key_mode, 0o600, "private keys should be owner-only");

            let cert_mode = std::fs::metadata(cert_dir.join("node.crt"))
                .expect("cert meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(cert_mode, 0o644, "certs should be world-readable");
        }
    }

    #[test]
    fn bootstrap_pki_no_materials_is_noop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cert_dir = temp.path().join("certs");
        let req = proto::InstallToDiskRequest {
            os_disk: "/dev/sda".to_string(),
            data_disks: vec![],
            controller: String::new(),
            run_controller: false,
            ca_cert_pem: String::new(),
            node_cert_pem: String::new(),
            node_key_pem: String::new(),
            controller_cert_pem: String::new(),
            controller_key_pem: String::new(),
            kctl_cert_pem: String::new(),
            kctl_key_pem: String::new(),
            data_disk_mode: String::new(),
            storage_backend: proto::StorageBackendType::Unspecified as i32,
            lvm_vg_name: String::new(),
            lvm_lv_prefix: String::new(),
            zfs_pool_name: String::new(),
            zfs_dataset_prefix: String::new(),
            disable_vxlan: false,
            sub_ca_cert_pem: String::new(),
            sub_ca_key_pem: String::new(),
            controllers: Vec::new(),
            dc_id: String::new(),
            hostname: String::new(),
            node_id: String::new(),
        };
        write_bootstrap_pki_at(&req, &cert_dir).expect("noop cert write");
        assert!(
            !cert_dir.exists(),
            "no certificate directory should be created when payload is empty"
        );
    }

    #[test]
    fn validate_disk_path_accepts_valid_devices() {
        validate_disk_path("/dev/sda", "os_disk").expect("sda");
        validate_disk_path("/dev/nvme0n1", "os_disk").expect("nvme");
        validate_disk_path("/dev/disk/by-id/scsi-0", "d").expect("by-id");
    }

    #[test]
    fn validate_disk_path_rejects_invalid() {
        validate_disk_path("/tmp/sda", "d").expect_err("not /dev/");
        validate_disk_path("/dev/../etc/passwd", "d").expect_err("traversal");
        validate_disk_path("/dev/sd a", "d").expect_err("whitespace");
    }

    #[test]
    fn validate_image_inputs_reject_invalid_values() {
        validate_image_url("http://example.com/a.raw").expect_err("must be https");
        validate_image_sha256("1234").expect_err("sha must be 64 hex");
        validate_destination_path("/tmp/evil.raw").expect_err("must be under image cache dir");
    }

    #[test]
    fn rebuild_args_uses_requested_mode() {
        assert_eq!(rebuild_args("test"), vec!["test"]);
        assert_eq!(rebuild_args("switch"), vec!["switch"]);
    }

    #[test]
    fn rebuild_sequence_skips_switch_on_test_failure() {
        assert_eq!(rebuild_sequence(false), vec!["test"]);
        assert_eq!(rebuild_sequence(true), vec!["test", "switch"]);
    }

    #[test]
    fn flake_url_with_rev_appends_rev_when_missing() {
        assert_eq!(
            flake_url_with_rev("github:org/repo", "abc"),
            "github:org/repo?rev=abc"
        );
    }

    #[test]
    fn flake_url_with_rev_uses_amp_when_query_string_present() {
        assert_eq!(
            flake_url_with_rev("github:org/repo?dir=foo", "abc"),
            "github:org/repo?dir=foo&rev=abc"
        );
    }

    #[test]
    fn flake_url_with_rev_is_noop_when_rev_empty_or_already_pinned() {
        assert_eq!(flake_url_with_rev("github:org/repo", ""), "github:org/repo");
        assert_eq!(
            flake_url_with_rev("github:org/repo?rev=abc", "def"),
            "github:org/repo?rev=abc"
        );
        assert_eq!(
            flake_url_with_rev("github:org/repo?dir=foo&rev=abc", "def"),
            "github:org/repo?dir=foo&rev=abc"
        );
    }

    #[test]
    fn sanitize_update_name_accepts_safe_inputs() {
        assert_eq!(
            sanitize_update_name("release-0.3.0_rc1").unwrap(),
            "release-0.3.0_rc1"
        );
        assert_eq!(sanitize_update_name("  abc  ").unwrap(), "abc");
    }

    #[test]
    fn sanitize_update_name_rejects_dangerous_inputs() {
        assert!(sanitize_update_name("").is_err(), "empty must fail");
        assert!(
            sanitize_update_name("../etc").is_err(),
            "path traversal must fail"
        );
        assert!(sanitize_update_name("foo bar").is_err(), "spaces must fail");
        assert!(sanitize_update_name("foo/bar").is_err(), "slash must fail");
        assert!(
            sanitize_update_name("foo;reboot").is_err(),
            "shell metacharacters must fail"
        );
    }

    #[tokio::test]
    async fn prepare_system_update_rejects_empty_update_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nix_path = temp.path().join("kcore-vms.nix");
        let svc = AdminService::new(nix_path.display().to_string());
        let err = <AdminService as proto::node_admin_server::NodeAdmin>::prepare_system_update(
            &svc,
            Request::new(proto::PrepareSystemUpdateRequest {
                update_name: String::new(),
                flake_ref: "github:foo/bar".into(),
                flake_rev: "abc".into(),
                system_profile: String::new(),
                host_system: String::new(),
                timeout_seconds: 60,
            }),
        )
        .await
        .expect_err("empty update_name must fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn prepare_system_update_rejects_empty_flake_ref() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nix_path = temp.path().join("kcore-vms.nix");
        let svc = AdminService::new(nix_path.display().to_string());
        let err = <AdminService as proto::node_admin_server::NodeAdmin>::prepare_system_update(
            &svc,
            Request::new(proto::PrepareSystemUpdateRequest {
                update_name: "release-0-3-0".into(),
                flake_ref: String::new(),
                flake_rev: "abc".into(),
                system_profile: String::new(),
                host_system: String::new(),
                timeout_seconds: 60,
            }),
        )
        .await
        .expect_err("empty flake_ref must fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn activate_system_update_rejects_invalid_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nix_path = temp.path().join("kcore-vms.nix");
        let svc = AdminService::new(nix_path.display().to_string());
        let err = <AdminService as proto::node_admin_server::NodeAdmin>::activate_system_update(
            &svc,
            Request::new(proto::ActivateSystemUpdateRequest {
                update_name: "release".into(),
                activation_mode: "rm-rf".into(),
                prepared_closure: String::new(),
            }),
        )
        .await
        .expect_err("bad mode must fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn activate_system_update_test_mode_does_not_touch_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nix_path = temp.path().join("kcore-vms.nix");
        let svc = AdminService::new(nix_path.display().to_string());
        let resp = <AdminService as proto::node_admin_server::NodeAdmin>::activate_system_update(
            &svc,
            Request::new(proto::ActivateSystemUpdateRequest {
                update_name: "release".into(),
                activation_mode: "test".into(),
                prepared_closure: String::new(),
            }),
        )
        .await
        .expect("test mode")
        .into_inner();
        assert!(resp.success);
        assert!(resp.message.contains("test mode"));
    }

    /// Build a service whose socket dir is a tempdir, so the receive-session
    /// tests can create and assert on marker / pid / socket files.
    fn receive_svc(socket_dir: &std::path::Path, state: LiveMigrateState) -> AdminService {
        AdminService::new_with_storage(
            socket_dir.join("kcore-vms.nix").display().to_string(),
            socket_dir.display().to_string(),
            storage::default_adapter(),
            state,
        )
    }

    /// The cleanup behind both `AbortLiveMigrateReceive` and the operator
    /// reset. Called directly because the handlers require a real mTLS peer
    /// certificate, which a unit test has no way to present.
    async fn clear(svc: &AdminService, vm: &str) -> live_migrate::ReceiveObservation {
        svc.clear_receive_session(vm, "", "").await
    }

    /// A pid that is certainly not running: spawned and reaped.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        child.wait().expect("reap");
        pid
    }

    /// `kill(0, SIGTERM)` signals the caller's whole process group, so a
    /// session that never learned its VMM's pid must not reach `kill` at all.
    /// Without this guard the node agent would SIGTERM itself.
    #[test]
    fn nix_kill_refuses_pid_zero() {
        let err = nix_kill(0).expect_err("pid 0 must be refused");
        assert!(err.contains("process group"), "unexpected message: {err}");
    }

    /// The operator escape hatch: a stranded session must be cleared along
    /// with everything it owns — the port reservation, the marker and the pid
    /// file — so a retried migration can prepare a fresh receive.
    #[tokio::test]
    async fn clearing_a_stranded_session_releases_the_port_marker_and_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = LiveMigrateState::new();
        let port = state.reserve_port().await.expect("reserve");
        let vm = "vm-stranded";
        let ch_pid = dead_pid();
        state
            .insert(
                vm,
                ReceiveSession {
                    port,
                    ch_pid,
                    receive_task: tokio::spawn(async { Ok(()) }),
                },
            )
            .await;

        let marker = live_migrate::handoff_marker_path(temp.path(), vm);
        let pid_file = live_migrate::migrate_pid_path(temp.path(), vm);
        let socket = temp.path().join(format!("{vm}.sock"));
        std::fs::write(&marker, b"1").expect("marker");
        std::fs::write(&pid_file, ch_pid.to_string()).expect("pid");
        std::fs::write(&socket, b"").expect("socket");

        let svc = receive_svc(temp.path(), state.clone());
        let observed = clear(&svc, vm).await;

        assert!(
            observed.has_session,
            "the session was there before clearing"
        );
        assert_eq!(observed.port, port);

        assert!(!marker.exists(), "handoff marker must be removed");
        assert!(!pid_file.exists(), "pid file must be removed");
        assert!(!socket.exists(), "API socket must be removed");
        assert!(
            state.get_port(vm).await.is_none(),
            "the session must no longer be tracked"
        );
        // The freed port must be reservable again, otherwise the next
        // migration to this node loses a slot in the fixed range for good.
        assert_eq!(
            state.reserve_explicit_port(port).await.expect("re-reserve"),
            port
        );
    }

    /// Idempotent by design: a runbook step an operator repeats, or runs on
    /// the wrong node first, must not turn into an error to reason about.
    #[tokio::test]
    async fn clearing_a_node_with_no_session_is_a_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let svc = receive_svc(temp.path(), LiveMigrateState::new());

        let observed = clear(&svc, "vm-absent").await;
        assert!(observed.is_empty(), "nothing was there to begin with");
        assert!(!observed.vmm_alive);

        // Twice, for good measure: repeating it changes nothing and is still
        // reported as an empty node rather than an error.
        assert!(clear(&svc, "vm-absent").await.is_empty());
    }

    /// A stale pid file whose number has been recycled must not get an
    /// unrelated process killed. The reported state has to say so, and the
    /// file still has to be cleaned up.
    #[tokio::test]
    async fn clearing_does_not_kill_a_recycled_pid_but_still_clears_the_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = "vm-recycled";
        let pid_file = live_migrate::migrate_pid_path(temp.path(), vm);
        // The test process itself: alive, and definitely not this VM's VMM.
        std::fs::write(&pid_file, std::process::id().to_string()).expect("pid");

        let svc = receive_svc(temp.path(), LiveMigrateState::new());
        let observed = clear(&svc, vm).await;

        assert!(observed.vmm_alive);
        assert!(!observed.vmm_pid_matches_vm);
        assert!(!pid_file.exists(), "the stale pid file must be discarded");
        // If the guard had failed we would have SIGTERMed the test process.
        assert!(live_migrate::pid_is_alive(std::process::id()));
    }

    /// The state the operator sees comes from the same snapshot the cleanup
    /// uses, and taking it must not disturb the session.
    #[tokio::test]
    async fn observing_a_prepared_session_leaves_it_intact() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = LiveMigrateState::new();
        let port = state.reserve_port().await.expect("reserve");
        let vm = "vm-inspect";
        let ch_pid = dead_pid();
        state
            .insert(
                vm,
                ReceiveSession {
                    port,
                    ch_pid,
                    receive_task: tokio::spawn(async { Ok(()) }),
                },
            )
            .await;
        let marker = live_migrate::handoff_marker_path(temp.path(), vm);
        std::fs::write(&marker, b"1").expect("marker");

        let svc = receive_svc(temp.path(), state.clone());
        let observed =
            live_migrate::observe_receive(&svc.live_migrate, &svc.vm_socket_dir, vm).await;
        let wire = receive_state_to_proto(&observed);

        assert!(wire.has_session);
        assert_eq!(wire.port, port as i32);
        assert_eq!(wire.session_pid, ch_pid);
        assert!(wire.marker_present);
        assert!(!wire.summary.is_empty());

        // Read-only: inspecting must not disturb the session or its files.
        assert!(marker.exists(), "inspection must not delete the marker");
        assert_eq!(state.get_port(vm).await, Some(port));
    }

    /// Both handlers are behind the controller's client certificate, so an
    /// unauthenticated caller can neither read nor clear session state.
    #[tokio::test]
    async fn receive_session_rpcs_require_a_peer_certificate() {
        let temp = tempfile::tempdir().expect("tempdir");
        let svc = receive_svc(temp.path(), LiveMigrateState::new());

        let err =
            <AdminService as proto::node_admin_server::NodeAdmin>::get_live_migrate_receive_status(
                &svc,
                Request::new(proto::GetLiveMigrateReceiveStatusRequest {
                    vm_name: "vm-1".into(),
                }),
            )
            .await
            .expect_err("status must require mTLS");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        let err =
            <AdminService as proto::node_admin_server::NodeAdmin>::abort_live_migrate_receive(
                &svc,
                Request::new(proto::AbortLiveMigrateReceiveRequest {
                    vm_name: "vm-1".into(),
                    rbd_pool: String::new(),
                    rbd_image: String::new(),
                }),
            )
            .await
            .expect_err("abort must require mTLS");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    /// A superseded apply must not read as `running` forever: the controller
    /// polls until it gets a verdict, and `running` would make it wait out its
    /// whole timeout for a rebuild that was already killed.
    #[test]
    fn apply_phase_to_proto_reports_a_superseded_apply_as_unknown() {
        assert_eq!(
            apply_phase_to_proto(NIX_APPLY_SUPERSEDED),
            proto::NixApplyPhase::Unknown as i32
        );
        assert_eq!(
            apply_phase_to_proto(NIX_APPLY_RUNNING),
            proto::NixApplyPhase::Running as i32
        );
        assert_eq!(
            apply_phase_to_proto(NIX_APPLY_SUCCEEDED),
            proto::NixApplyPhase::Succeeded as i32
        );
        assert_eq!(
            apply_phase_to_proto(NIX_APPLY_FAILED),
            proto::NixApplyPhase::Failed as i32
        );
        assert_eq!(
            apply_phase_to_proto("something-else"),
            proto::NixApplyPhase::Unknown as i32
        );
    }

    #[tokio::test]
    async fn apply_nix_config_requires_mtls_in_insecure_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nix_path = temp.path().join("kcore-vms.nix");
        let svc = AdminService::new(nix_path.display().to_string());
        let req = proto::ApplyNixConfigRequest {
            configuration_nix: "{ ... }: { test = true; }\n".to_string(),
            rebuild: false,
            apply_id: String::new(),
        };

        let status = <AdminService as proto::node_admin_server::NodeAdmin>::apply_nix_config(
            &svc,
            Request::new(req),
        )
        .await
        .expect_err("apply should be denied without mTLS");

        assert_eq!(status.code(), tonic::Code::PermissionDenied);
        assert!(!nix_path.exists(), "denied request should not write config");
    }

    #[tokio::test]
    async fn insecure_mode_allows_only_discovery_and_install_admin_endpoints() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nix_path = temp.path().join("kcore-vms.nix");
        let svc = AdminService::new(nix_path.display().to_string());

        let apply = <AdminService as proto::node_admin_server::NodeAdmin>::apply_nix_config(
            &svc,
            Request::new(proto::ApplyNixConfigRequest {
                configuration_nix: "{ ... }: {}\n".to_string(),
                rebuild: false,
                apply_id: String::new(),
            }),
        )
        .await
        .expect_err("apply should require mTLS");
        assert_eq!(apply.code(), tonic::Code::PermissionDenied);

        let ensure_image = <AdminService as proto::node_admin_server::NodeAdmin>::ensure_image(
            &svc,
            Request::new(proto::EnsureImageRequest {
                image_url: "https://example.com/debian.raw".to_string(),
                image_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
                destination_path: "/var/lib/kcore/images/debian.raw".to_string(),
            }),
        )
        .await
        .expect_err("ensure_image should require mTLS");
        assert_eq!(ensure_image.code(), tonic::Code::PermissionDenied);

        let disks = <AdminService as proto::node_admin_server::NodeAdmin>::list_disks(
            &svc,
            Request::new(proto::ListDisksRequest {}),
        )
        .await;
        match disks {
            Ok(_) => {}
            Err(status) => {
                assert_ne!(
                    status.code(),
                    tonic::Code::PermissionDenied,
                    "list_disks should be allowed without TLS"
                );
            }
        }

        let nics = <AdminService as proto::node_admin_server::NodeAdmin>::list_network_interfaces(
            &svc,
            Request::new(proto::ListNetworkInterfacesRequest {}),
        )
        .await;
        match nics {
            Ok(_) => {}
            Err(status) => {
                assert_ne!(
                    status.code(),
                    tonic::Code::PermissionDenied,
                    "list_network_interfaces should be allowed without TLS"
                );
            }
        }

        let install = <AdminService as proto::node_admin_server::NodeAdmin>::install_to_disk(
            &svc,
            Request::new(proto::InstallToDiskRequest {
                os_disk: "/tmp/not-a-device".to_string(),
                data_disks: Vec::new(),
                controller: String::new(),
                run_controller: false,
                ca_cert_pem: String::new(),
                node_cert_pem: String::new(),
                node_key_pem: String::new(),
                controller_cert_pem: String::new(),
                controller_key_pem: String::new(),
                kctl_cert_pem: String::new(),
                kctl_key_pem: String::new(),
                data_disk_mode: String::new(),
                storage_backend: proto::StorageBackendType::Unspecified as i32,
                lvm_vg_name: String::new(),
                lvm_lv_prefix: String::new(),
                zfs_pool_name: String::new(),
                zfs_dataset_prefix: String::new(),
                disable_vxlan: false,
                sub_ca_cert_pem: String::new(),
                sub_ca_key_pem: String::new(),
                controllers: Vec::new(),
                dc_id: String::new(),
                hostname: String::new(),
                node_id: String::new(),
            }),
        )
        .await
        .expect_err("invalid disk path should fail after passing auth");
        assert_eq!(install.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn install_rejects_neither_controller_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nix_path = temp.path().join("kcore-vms.nix");
        let svc = AdminService::new(nix_path.display().to_string());

        let neither = <AdminService as proto::node_admin_server::NodeAdmin>::install_to_disk(
            &svc,
            Request::new(proto::InstallToDiskRequest {
                os_disk: "/dev/sda".to_string(),
                data_disks: Vec::new(),
                controller: String::new(),
                run_controller: false,
                ca_cert_pem: String::new(),
                node_cert_pem: String::new(),
                node_key_pem: String::new(),
                controller_cert_pem: String::new(),
                controller_key_pem: String::new(),
                kctl_cert_pem: String::new(),
                kctl_key_pem: String::new(),
                data_disk_mode: String::new(),
                storage_backend: proto::StorageBackendType::Unspecified as i32,
                lvm_vg_name: String::new(),
                lvm_lv_prefix: String::new(),
                zfs_pool_name: String::new(),
                zfs_dataset_prefix: String::new(),
                disable_vxlan: false,
                sub_ca_cert_pem: String::new(),
                sub_ca_key_pem: String::new(),
                controllers: Vec::new(),
                dc_id: String::new(),
                hostname: String::new(),
                node_id: String::new(),
            }),
        )
        .await
        .expect_err("missing controller mode should fail");
        assert_eq!(neither.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn build_install_args_accepts_run_controller_with_peers() {
        let req = proto::InstallToDiskRequest {
            os_disk: "/dev/sda".to_string(),
            data_disks: Vec::new(),
            controller: String::new(),
            run_controller: true,
            controllers: vec!["192.168.40.105:9090".to_string()],
            ca_cert_pem: String::new(),
            node_cert_pem: String::new(),
            node_key_pem: String::new(),
            controller_cert_pem: String::new(),
            controller_key_pem: String::new(),
            kctl_cert_pem: String::new(),
            kctl_key_pem: String::new(),
            data_disk_mode: String::new(),
            storage_backend: proto::StorageBackendType::Unspecified as i32,
            lvm_vg_name: String::new(),
            lvm_lv_prefix: String::new(),
            zfs_pool_name: String::new(),
            zfs_dataset_prefix: String::new(),
            disable_vxlan: false,
            sub_ca_cert_pem: String::new(),
            sub_ca_key_pem: String::new(),
            dc_id: String::new(),
            hostname: String::new(),
            node_id: String::new(),
        };
        let args = build_install_command_args(&req).expect("should accept both flags");
        assert!(args.contains(&"--run-controller".to_string()));
        assert!(args.contains(&"--controller".to_string()));
        assert!(args.contains(&"192.168.40.105:9090".to_string()));
    }

    #[test]
    fn build_install_command_args_join_controller_mode() {
        let req = proto::InstallToDiskRequest {
            os_disk: "/dev/sda".to_string(),
            data_disks: vec!["/dev/nvme0n1".to_string()],
            controller: "192.168.40.10:9090".to_string(),
            run_controller: false,
            ca_cert_pem: String::new(),
            node_cert_pem: String::new(),
            node_key_pem: String::new(),
            controller_cert_pem: String::new(),
            controller_key_pem: String::new(),
            kctl_cert_pem: String::new(),
            kctl_key_pem: String::new(),
            data_disk_mode: "filesystem".to_string(),
            storage_backend: proto::StorageBackendType::Filesystem as i32,
            lvm_vg_name: String::new(),
            lvm_lv_prefix: String::new(),
            zfs_pool_name: String::new(),
            zfs_dataset_prefix: String::new(),
            disable_vxlan: false,
            sub_ca_cert_pem: String::new(),
            sub_ca_key_pem: String::new(),
            controllers: vec![
                "192.168.40.10:9090".to_string(),
                "192.168.40.11:9090".to_string(),
            ],
            dc_id: "DC1".to_string(),
            hostname: String::new(),
            node_id: String::new(),
        };
        let args = build_install_command_args(&req).expect("args");
        assert!(args.contains(&"--controller".to_string()));
        assert!(args.contains(&"192.168.40.10:9090".to_string()));
        assert!(args.contains(&"192.168.40.11:9090".to_string()));
        assert_eq!(
            args.iter().filter(|v| v.as_str() == "--controller").count(),
            2
        );
        assert!(!args.contains(&"--run-controller".to_string()));
        assert!(args.contains(&"--data-disk".to_string()));
        assert!(args.contains(&"/dev/nvme0n1".to_string()));
        assert!(args.contains(&"--dc-id".to_string()));
        assert!(args.contains(&"DC1".to_string()));
    }

    #[test]
    fn build_install_command_args_run_controller_mode() {
        let req = proto::InstallToDiskRequest {
            os_disk: "/dev/sda".to_string(),
            data_disks: Vec::new(),
            controller: String::new(),
            run_controller: true,
            ca_cert_pem: String::new(),
            node_cert_pem: String::new(),
            node_key_pem: String::new(),
            controller_cert_pem: String::new(),
            controller_key_pem: String::new(),
            kctl_cert_pem: String::new(),
            kctl_key_pem: String::new(),
            data_disk_mode: "zfs".to_string(),
            storage_backend: proto::StorageBackendType::Zfs as i32,
            lvm_vg_name: String::new(),
            lvm_lv_prefix: String::new(),
            zfs_pool_name: "tank0".to_string(),
            zfs_dataset_prefix: "kcore-".to_string(),
            disable_vxlan: false,
            sub_ca_cert_pem: String::new(),
            sub_ca_key_pem: String::new(),
            controllers: Vec::new(),
            dc_id: String::new(),
            hostname: String::new(),
            node_id: String::new(),
        };
        let args = build_install_command_args(&req).expect("args");
        assert!(args.contains(&"--run-controller".to_string()));
        assert!(!args.contains(&"--controller".to_string()));
        assert!(args.contains(&"--data-disk-mode".to_string()));
        assert!(args.contains(&"zfs".to_string()));
        assert!(args.contains(&"--zfs-pool-name".to_string()));
        assert!(args.contains(&"tank0".to_string()));
        assert!(args.contains(&"--zfs-dataset-prefix".to_string()));
        assert!(args.contains(&"kcore-".to_string()));
        assert!(!args.contains(&"--disable-vxlan".to_string()));
    }

    #[test]
    fn build_install_command_args_disable_vxlan_flag() {
        let req = proto::InstallToDiskRequest {
            os_disk: "/dev/sda".to_string(),
            data_disks: Vec::new(),
            controller: "192.168.1.10:9090".to_string(),
            run_controller: false,
            ca_cert_pem: String::new(),
            node_cert_pem: String::new(),
            node_key_pem: String::new(),
            controller_cert_pem: String::new(),
            controller_key_pem: String::new(),
            kctl_cert_pem: String::new(),
            kctl_key_pem: String::new(),
            data_disk_mode: String::new(),
            storage_backend: proto::StorageBackendType::Filesystem as i32,
            lvm_vg_name: String::new(),
            lvm_lv_prefix: String::new(),
            zfs_pool_name: String::new(),
            zfs_dataset_prefix: String::new(),
            disable_vxlan: true,
            sub_ca_cert_pem: String::new(),
            sub_ca_key_pem: String::new(),
            controllers: Vec::new(),
            dc_id: String::new(),
            hostname: String::new(),
            node_id: String::new(),
        };
        let args = build_install_command_args(&req).expect("args");
        assert!(args.contains(&"--disable-vxlan".to_string()));
    }

    #[test]
    fn build_install_command_args_single_disk_no_data_disks() {
        let req = proto::InstallToDiskRequest {
            os_disk: "/dev/sda".to_string(),
            data_disks: Vec::new(),
            controller: "192.168.1.10:9090".to_string(),
            run_controller: false,
            ca_cert_pem: String::new(),
            node_cert_pem: String::new(),
            node_key_pem: String::new(),
            controller_cert_pem: String::new(),
            controller_key_pem: String::new(),
            kctl_cert_pem: String::new(),
            kctl_key_pem: String::new(),
            data_disk_mode: String::new(),
            storage_backend: proto::StorageBackendType::Filesystem as i32,
            lvm_vg_name: String::new(),
            lvm_lv_prefix: String::new(),
            zfs_pool_name: String::new(),
            zfs_dataset_prefix: String::new(),
            disable_vxlan: false,
            sub_ca_cert_pem: String::new(),
            sub_ca_key_pem: String::new(),
            controllers: Vec::new(),
            dc_id: String::new(),
            hostname: String::new(),
            node_id: String::new(),
        };
        let args = build_install_command_args(&req).expect("args");
        assert!(!args.contains(&"--data-disk".to_string()));
        assert!(args.contains(&"--data-disk-mode".to_string()));
        assert!(args.contains(&"filesystem".to_string()));
        assert!(args.contains(&"--disk".to_string()));
        assert!(args.contains(&"/dev/sda".to_string()));
    }

    #[test]
    fn build_install_args_normalizes_port() {
        let req = proto::InstallToDiskRequest {
            os_disk: "/dev/sda".to_string(),
            data_disks: Vec::new(),
            storage_backend: 0,
            controllers: vec!["192.168.1.10".to_string(), "10.0.0.5:7777".to_string()],
            controller: String::new(),
            run_controller: false,
            hostname: "test".to_string(),
            node_id: "test".to_string(),
            ca_cert_pem: String::new(),
            node_cert_pem: String::new(),
            node_key_pem: String::new(),
            disable_vxlan: false,
            dc_id: String::new(),
            data_disk_mode: String::new(),
            controller_cert_pem: String::new(),
            controller_key_pem: String::new(),
            kctl_cert_pem: String::new(),
            kctl_key_pem: String::new(),
            lvm_vg_name: String::new(),
            lvm_lv_prefix: String::new(),
            zfs_pool_name: String::new(),
            zfs_dataset_prefix: String::new(),
            sub_ca_cert_pem: String::new(),
            sub_ca_key_pem: String::new(),
        };
        let args = build_install_command_args(&req).expect("args");
        assert!(args.contains(&"192.168.1.10:9090".to_string()));
        assert!(args.contains(&"10.0.0.5:7777".to_string()));
        assert!(!args.contains(&"192.168.1.10".to_string()));
    }

    #[test]
    fn parse_lease_entry_parses_dnsmasq_format() {
        let line = "1711454677 52:54:00:4b:13:d6 10.240.0.113 ubuntu-noble-1 01:52:54:00:4b:13:d6";
        let parsed = parse_lease_entry(line).expect("parse lease");
        assert_eq!(parsed.0, "52:54:00:4b:13:d6");
        assert_eq!(parsed.1, "10.240.0.113");
        assert_eq!(parsed.2, "ubuntu-noble-1");
    }

    #[test]
    fn find_vm_ip_in_lease_file_matches_mac_or_hostname() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lease = temp.path().join("dnsmasq-default.leases");
        std::fs::write(
            &lease,
            "1711454677 52:54:00:aa:bb:cc 10.240.0.50 old-vm *\n\
             1711454678 52:54:00:4b:13:d6 10.240.0.113 ubuntu-noble-1 *\n",
        )
        .expect("write lease");

        let ip_by_host = find_vm_ip_in_lease_file(&lease, "ubuntu-noble-1", None);
        assert_eq!(ip_by_host.as_deref(), Some("10.240.0.113"));

        let ip_by_mac =
            find_vm_ip_in_lease_file(&lease, "different-name", Some("52:54:00:4b:13:d6"));
        assert_eq!(ip_by_mac.as_deref(), Some("10.240.0.113"));
    }

    #[test]
    fn validate_port_and_timeout_defaults_are_applied() {
        assert_eq!(validate_port_or_default(0), 22);
        assert_eq!(validate_port_or_default(-3), 22);
        assert_eq!(validate_port_or_default(2222), 2222);
        assert_eq!(validate_timeout_ms_or_default(0), 1500);
        assert_eq!(validate_timeout_ms_or_default(-1), 1500);
        assert_eq!(validate_timeout_ms_or_default(3000), 3000);
    }

    #[test]
    fn disk_management_mode_defaults_to_installer_only() {
        assert_eq!(normalize_disk_management_mode(""), DISK_MODE_INSTALLER_ONLY);
        assert_eq!(
            normalize_disk_management_mode("unknown"),
            DISK_MODE_INSTALLER_ONLY
        );
    }

    #[test]
    fn disk_timeout_defaults_and_caps() {
        assert_eq!(validate_disk_timeout_seconds_or_default(0), 300);
        assert_eq!(validate_disk_timeout_seconds_or_default(-1), 300);
        assert_eq!(validate_disk_timeout_seconds_or_default(120), 120);
        assert_eq!(validate_disk_timeout_seconds_or_default(7200), 3600);
    }

    #[test]
    fn parse_neigh_line_extracts_ip_and_mac() {
        let line = "10.240.0.113 dev kbr-default lladdr 52:54:00:4b:13:d6 REACHABLE";
        let parsed = parse_neigh_line(line).expect("parse neigh");
        assert_eq!(parsed.0, "10.240.0.113");
        assert_eq!(parsed.1, "52:54:00:4b:13:d6");
    }

    #[test]
    fn vm_unit_is_fatal_detects_failed_and_flapping() {
        let failed = VmUnitState {
            active_state: "failed".to_string(),
            sub_state: "failed".to_string(),
            result: "exit-code".to_string(),
            n_restarts: 1,
        };
        assert!(vm_unit_is_fatal(&failed));

        let flapping = VmUnitState {
            active_state: "activating".to_string(),
            sub_state: "auto-restart".to_string(),
            result: "exit-code".to_string(),
            n_restarts: 5,
        };
        assert!(vm_unit_is_fatal(&flapping));

        let transient = VmUnitState {
            active_state: "activating".to_string(),
            sub_state: "start".to_string(),
            result: "success".to_string(),
            n_restarts: 0,
        };
        assert!(!vm_unit_is_fatal(&transient));
    }

    #[test]
    fn normalize_endpoint_ipv4_no_port() {
        assert_eq!(normalize_endpoint("10.0.0.1", 9090), "10.0.0.1:9090");
    }

    #[test]
    fn normalize_endpoint_ipv4_with_port() {
        assert_eq!(normalize_endpoint("10.0.0.1:8080", 9090), "10.0.0.1:8080");
    }

    #[test]
    fn normalize_endpoint_ipv6_bracketed_no_port() {
        assert_eq!(
            normalize_endpoint("[2001:db8::10]", 9090),
            "[2001:db8::10]:9090"
        );
    }

    #[test]
    fn normalize_endpoint_ipv6_bracketed_with_port() {
        assert_eq!(
            normalize_endpoint("[2001:db8::10]:7070", 9090),
            "[2001:db8::10]:7070"
        );
    }

    #[test]
    fn normalize_endpoint_bare_ipv6() {
        assert_eq!(
            normalize_endpoint("2001:db8::10", 9090),
            "[2001:db8::10]:9090"
        );
    }
}

/// Property-based tests (Phase 2).
#[cfg(test)]
mod proptests {
    use super::{
        normalize_endpoint, parse_lease_entry, parse_neigh_line, parse_stopped_vms_from_nix,
        vm_unit_is_fatal, VmUnitState,
    };
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 2_000,
            .. ProptestConfig::default()
        })]

        /// Parsers must never panic on arbitrary input. (Both consume
        /// untrusted-ish text from `dnsmasq` lease files and `ip neigh`
        /// output respectively.)
        #[test]
        fn lease_and_neigh_parsers_never_panic(line in ".{0,128}") {
            let _ = parse_lease_entry(&line);
            let _ = parse_neigh_line(&line);
        }

        /// `parse_lease_entry` returns `Some` iff the line has at least
        /// 4 whitespace-delimited tokens.
        #[test]
        fn lease_entry_some_iff_four_fields(line in ".{0,128}") {
            let n = line.split_whitespace().count();
            prop_assert_eq!(parse_lease_entry(&line).is_some(), n >= 4);
        }

        /// On a well-formed lease line, the (mac, ip, hostname) tokens
        /// match field positions 1, 2, 3 (0-indexed).
        #[test]
        fn lease_entry_returns_correct_positions(
            ts in 100u32..=999,
            mac in "[0-9a-f:]{17}",
            ip in "[0-9.]{7,15}",
            hostname in "[a-z0-9-]{1,16}",
            client_id in "[a-z0-9*]{1,8}",
        ) {
            let line = format!("{ts} {mac} {ip} {hostname} {client_id}");
            let parsed = parse_lease_entry(&line).expect("must parse");
            prop_assert_eq!(parsed.0, mac.as_str());
            prop_assert_eq!(parsed.1, ip.as_str());
            prop_assert_eq!(parsed.2, hostname.as_str());
        }

        /// `parse_neigh_line` returns `None` for any line shorter than
        /// 5 fields, and any line missing the `lladdr` token.
        #[test]
        fn neigh_line_returns_none_when_lladdr_missing(line in "[a-z0-9. ]{0,64}") {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 5 || !fields.contains(&"lladdr") {
                prop_assert!(parse_neigh_line(&line).is_none());
            }
        }

        /// `parse_neigh_line` returns the IP and the token immediately
        /// after `lladdr` for well-formed lines.
        #[test]
        fn neigh_line_returns_ip_and_mac(
            ip in "[0-9.]{7,15}",
            mac in "[0-9a-f:]{17}",
        ) {
            let line = format!("{ip} dev br0 lladdr {mac} REACHABLE");
            let parsed = parse_neigh_line(&line).expect("well-formed");
            prop_assert_eq!(parsed.0, ip.as_str());
            prop_assert_eq!(parsed.1, mac.as_str());
        }

        /// `vm_unit_is_fatal` is **monotone** in `n_restarts` for the
        /// flapping branch: if a state is fatal at N restarts, it is
        /// fatal at N+k restarts (everything else equal).
        #[test]
        fn vm_unit_is_fatal_is_monotone_in_restarts(
            active in prop::sample::select(vec!["active", "activating", "failed", "inactive"]),
            sub in prop::sample::select(vec!["running", "auto-restart", "dead"]),
            result in prop::sample::select(vec!["exit-code", "success", "signal"]),
            n in 0u32..=10,
            extra in 1u32..=10,
        ) {
            let lo = VmUnitState {
                active_state: active.into(),
                sub_state: sub.into(),
                result: result.into(),
                n_restarts: n,
            };
            let hi = VmUnitState {
                active_state: lo.active_state.clone(),
                sub_state: lo.sub_state.clone(),
                result: lo.result.clone(),
                n_restarts: n.saturating_add(extra),
            };
            if vm_unit_is_fatal(&lo) {
                prop_assert!(vm_unit_is_fatal(&hi));
            }
        }

        /// `failed` active state is **always** fatal regardless of
        /// other fields.
        #[test]
        fn vm_unit_is_fatal_failed_is_always_fatal(
            sub in "[a-z-]{0,16}",
            result in "[a-z-]{0,16}",
            n in any::<u32>(),
        ) {
            let s = VmUnitState {
                active_state: "failed".into(),
                sub_state: sub,
                result,
                n_restarts: n,
            };
            prop_assert!(vm_unit_is_fatal(&s));
        }

        /// `normalize_endpoint` never panics on arbitrary input.
        #[test]
        fn normalize_endpoint_never_panics(s in ".{0,64}") {
            let _ = normalize_endpoint(&s, 9091);
        }

        /// `normalize_endpoint` is idempotent on inputs that already
        /// parse as `SocketAddr` (it should hand them straight back).
        #[test]
        fn normalize_endpoint_idempotent_on_socket_addr(
            a in 0u8..=255, b in 0u8..=255, c in 0u8..=255, d in 0u8..=255,
            port in 1u16..=65_535,
        ) {
            let addr = format!("{a}.{b}.{c}.{d}:{port}");
            prop_assert_eq!(normalize_endpoint(&addr, 9091), addr);
        }

        /// `parse_stopped_vms_from_nix` always returns a sorted,
        /// de-duplicated list.
        #[test]
        fn parse_stopped_vms_from_nix_is_sorted_and_deduped(s in ".{0,256}") {
            let out = parse_stopped_vms_from_nix(&s);
            let mut sorted = out.clone();
            sorted.sort();
            sorted.dedup();
            prop_assert_eq!(out, sorted);
        }
    }
}
