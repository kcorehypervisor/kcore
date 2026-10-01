//! GuestOps: SSH into a guest to probe/grow block devices by virtio serial.
//!
//! Identity lives at `/var/lib/kcore/guest-ops/id_ed25519`. The matching public
//! key is injected into guest cloud-init by the controller so every node can
//! reach every guest after migrate.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::info;

pub const DEFAULT_IDENTITY_DIR: &str = "/var/lib/kcore/guest-ops";
pub const DEFAULT_SSH_USER: &str = "kcore";

#[derive(Debug, Clone)]
pub struct GuestOpsPaths {
    pub dir: PathBuf,
}

impl Default for GuestOpsPaths {
    fn default() -> Self {
        Self {
            dir: PathBuf::from(DEFAULT_IDENTITY_DIR),
        }
    }
}

impl GuestOpsPaths {
    pub fn identity_file(&self) -> PathBuf {
        self.dir.join("id_ed25519")
    }
    pub fn public_file(&self) -> PathBuf {
        self.dir.join("id_ed25519.pub")
    }
}

/// Ensure an ed25519 keypair exists; return the OpenSSH public key line.
pub async fn ensure_keypair(paths: &GuestOpsPaths) -> Result<String, String> {
    let priv_path = paths.identity_file();
    let pub_path = paths.public_file();
    if priv_path.is_file() && pub_path.is_file() {
        return std::fs::read_to_string(&pub_path)
            .map(|s| s.trim().to_string())
            .map_err(|e| format!("read guest-ops pubkey: {e}"));
    }
    tokio::fs::create_dir_all(&paths.dir)
        .await
        .map_err(|e| format!("mkdir {}: {e}", paths.dir.display()))?;
    let status = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            priv_path.to_str().unwrap_or(""),
            "-C",
            "kcore-guest-ops",
            "-q",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .map_err(|e| format!("ssh-keygen: {e}"))?;
    if !status.success() {
        return Err(format!("ssh-keygen failed with {status}"));
    }
    // Restrict private key perms (best-effort).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&priv_path, std::fs::Permissions::from_mode(0o600));
    }
    let pk = tokio::fs::read_to_string(&pub_path)
        .await
        .map_err(|e| format!("read pubkey: {e}"))?
        .trim()
        .to_string();
    info!(path = %pub_path.display(), "guest-ops keypair ready");
    Ok(pk)
}

pub fn read_public_key(paths: &GuestOpsPaths) -> Option<String> {
    std::fs::read_to_string(paths.public_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse `lsblk -bno NAME,SIZE,SERIAL,TYPE` output for a virtio serial.
/// Returns (device path like /dev/vdb, size_bytes). Prefers disk over part.
pub fn parse_lsblk_for_serial(stdout: &str, serial: &str) -> Option<(String, i64)> {
    let want = serial.trim();
    if want.is_empty() {
        return None;
    }
    let mut best: Option<(String, i64, bool)> = None; // path, size, is_disk
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        let name = cols[0];
        let size: i64 = cols[1].parse().unwrap_or(0);
        let ser = cols[2];
        let typ = cols.get(3).copied().unwrap_or("disk");
        if ser != want && ser != &format!("virtio-{want}") {
            continue;
        }
        let path = if name.starts_with('/') {
            name.to_string()
        } else {
            format!("/dev/{name}")
        };
        let is_disk = typ == "disk";
        match &best {
            None => best = Some((path, size, is_disk)),
            Some((_, _, was_disk)) if is_disk && !*was_disk => {
                best = Some((path, size, is_disk));
            }
            _ => {}
        }
    }
    best.map(|(p, s, _)| (p, s))
}

/// Shell snippet run in the guest to grow the filesystem on a disk serial.
pub fn grow_script(serial: &str) -> String {
    let serial = serial.replace('\'', "");
    format!(
        r#"set -euo pipefail
SERIAL='{serial}'
LINE=$(lsblk -bno NAME,SIZE,SERIAL,TYPE | awk -v s="$SERIAL" '$3==s || $3==("virtio-" s) {{print; if ($4=="disk") exit}}')
NAME=$(echo "$LINE" | awk '{{print $1}}')
test -n "$NAME"
DEV="/dev/$NAME"
# Prefer first partition if present
PART=$(lsblk -bno NAME,TYPE "$DEV" | awk '$2=="part" {{print $1; exit}}')
TARGET="$DEV"
if [ -n "${{PART:-}}" ]; then
  TARGET="/dev/$PART"
  if command -v growpart >/dev/null 2>&1; then
    growpart "$DEV" 1 || true
  fi
fi
if command -v resize2fs >/dev/null 2>&1 && blkid -o value -s TYPE "$TARGET" 2>/dev/null | grep -Eq 'ext[234]'; then
  resize2fs "$TARGET"
elif command -v xfs_growfs >/dev/null 2>&1 && blkid -o value -s TYPE "$TARGET" 2>/dev/null | grep -q xfs; then
  MP=$(findmnt -n -o TARGET "$TARGET" 2>/dev/null || true)
  if [ -n "$MP" ]; then xfs_growfs "$MP"; fi
fi
lsblk -bno SIZE,SERIAL,TYPE | awk -v s="$SERIAL" '$2==s || $2==("virtio-" s) {{ if ($3=="disk") {{print $1; exit}} }}'
"#
    )
}

/// Remote shell snippet: quiesce or release the guest root filesystem via `fsfreeze`.
pub fn fsfreeze_script(freeze: bool) -> &'static str {
    if freeze {
        "set -euo pipefail; command -v fsfreeze >/dev/null; fsfreeze --freeze /"
    } else {
        "set -euo pipefail; command -v fsfreeze >/dev/null; fsfreeze --unfreeze /"
    }
}

/// Run fsfreeze in the guest over SSH (same identity/options as grow/probe).
pub async fn guest_fsfreeze(
    paths: &GuestOpsPaths,
    user: &str,
    ip: &str,
    port: u16,
    freeze: bool,
    timeout: Duration,
) -> Result<(), String> {
    ssh_exec(
        &paths.identity_file(),
        user,
        ip,
        port,
        fsfreeze_script(freeze),
        timeout,
    )
    .await
    .map(|_| ())
}

pub async fn ssh_exec(
    identity: &Path,
    user: &str,
    ip: &str,
    port: u16,
    remote_cmd: &str,
    timeout: Duration,
) -> Result<String, String> {
    let target = format!("{user}@{ip}");
    let child = Command::new("ssh")
        .args([
            "-i",
            identity.to_str().unwrap_or(""),
            "-p",
            &port.to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=5",
            &target,
            remote_cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn ssh: {e}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| "ssh timed out".to_string())?
        .map_err(|e| format!("ssh wait: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsblk_prefers_disk() {
        let out = "\
vda 21474836480  rootdisk disk
vda1 21474836480 rootdisk part
vdb 107374182400 abcdef012345 disk
vdb1 107374182400 abcdef012345 part
";
        let (dev, size) = parse_lsblk_for_serial(out, "abcdef012345").unwrap();
        assert_eq!(dev, "/dev/vdb");
        assert_eq!(size, 107374182400);
    }

    #[test]
    fn parse_lsblk_accepts_virtio_prefix() {
        let out = "vdb 4096 virtio-deadbeef disk\n";
        let (dev, _) = parse_lsblk_for_serial(out, "deadbeef").unwrap();
        assert_eq!(dev, "/dev/vdb");
    }

    #[test]
    fn grow_script_mentions_serial_and_tools() {
        let s = grow_script("abc123");
        assert!(s.contains("abc123"));
        assert!(s.contains("growpart") || s.contains("resize2fs"));
    }

    #[test]
    fn fsfreeze_script_uses_root_mount() {
        assert!(fsfreeze_script(true).contains("fsfreeze --freeze /"));
        assert!(fsfreeze_script(false).contains("fsfreeze --unfreeze /"));
    }

    #[tokio::test]
    async fn ensure_keypair_round_trip_in_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = GuestOpsPaths {
            dir: dir.path().to_path_buf(),
        };
        let pk1 = ensure_keypair(&paths).await.expect("gen");
        assert!(pk1.starts_with("ssh-ed25519 "));
        let pk2 = ensure_keypair(&paths).await.expect("reuse");
        assert_eq!(pk1, pk2);
    }
}
