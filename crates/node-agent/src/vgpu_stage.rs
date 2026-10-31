//! Stage an NVIDIA Virtual GPU Manager `.run` into the Nix store.
//!
//! The controller never copies the proprietary file into git. The node
//! downloads it, checks the sha256, and `nix-store --add-fixed` so the
//! `requireFile` derivation in `ch-vm.vgpu.managerRun` resolves locally.

use std::path::{Path, PathBuf};
use std::process::Command;

use tonic::Status;

const STAGE_DIR: &str = "/var/lib/kcore/vgpu";

pub fn validate_manager_file(file: &str) -> Result<String, Status> {
    let file = file.trim();
    if file.is_empty()
        || file.len() > 180
        || file.contains('/')
        || file.contains('\\')
        || file.starts_with('.')
        || !file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(Status::invalid_argument(
            "manager file must be a single path segment of letters, digits, '.', '_' or '-'",
        ));
    }
    if !file.ends_with(".run") {
        return Err(Status::invalid_argument(
            "manager file must be the NVIDIA .run installer",
        ));
    }
    Ok(file.to_string())
}

pub fn validate_manager_url(url: &str) -> Result<String, Status> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) || url.contains(' ') {
        return Err(Status::invalid_argument(
            "manager url must be an http or https URL",
        ));
    }
    Ok(url.to_string())
}

pub fn validate_manager_sha256(sha: &str) -> Result<String, Status> {
    let sha = sha.trim().to_ascii_lowercase();
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Status::invalid_argument(
            "manager sha256 must be 64 hexadecimal characters",
        ));
    }
    Ok(sha)
}

fn sha256_file(path: &Path) -> Result<String, Status> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| Status::internal(format!("running sha256sum: {e}")))?;
    if !out.status.success() {
        return Err(Status::internal(format!(
            "sha256sum failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(|s| s.to_ascii_lowercase())
        .ok_or_else(|| Status::internal("sha256sum produced no digest"))
}

/// Download `url` to `/var/lib/kcore/vgpu/<file>` when the digest differs,
/// then add the file to the Nix store. Returns the store path.
pub fn stage_manager(url: &str, sha256: &str, file: &str) -> Result<String, Status> {
    let dir = PathBuf::from(STAGE_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| Status::internal(format!("creating {}: {e}", dir.display())))?;
    let dest = dir.join(file);
    let have = dest
        .is_file()
        .then(|| sha256_file(&dest).ok())
        .flatten()
        .unwrap_or_default();
    if have != sha256 {
        let tmp = dir.join(format!("{file}.part"));
        let status = Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(&tmp)
            .arg(url)
            .status()
            .map_err(|e| Status::internal(format!("starting curl: {e}")))?;
        if !status.success() {
            let _ = std::fs::remove_file(&tmp);
            return Err(Status::internal(format!(
                "download of the Virtual GPU Manager failed ({url})"
            )));
        }
        let got = sha256_file(&tmp)?;
        if got != sha256 {
            let _ = std::fs::remove_file(&tmp);
            return Err(Status::failed_precondition(format!(
                "Virtual GPU Manager sha256 mismatch (expected {sha256}, got {got})"
            )));
        }
        std::fs::rename(&tmp, &dest).map_err(|e| {
            Status::internal(format!(
                "moving {} to {}: {e}",
                tmp.display(),
                dest.display()
            ))
        })?;
    }
    let out = Command::new("nix-store")
        .args(["--add-fixed", "sha256"])
        .arg(&dest)
        .output()
        .map_err(|e| Status::internal(format!("running nix-store: {e}")))?;
    if !out.status.success() {
        return Err(Status::internal(format!(
            "nix-store --add-fixed failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let store_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !store_path.starts_with("/nix/store/") {
        return Err(Status::internal(format!(
            "nix-store did not return a store path: {store_path}"
        )));
    }
    Ok(store_path)
}

/// Schedule a reboot that happens after this process has answered the RPC.
pub fn schedule_reboot() -> Result<(), Status> {
    let status = Command::new("systemd-run")
        .args([
            "--on-active=2s",
            "--unit=kcore-node-reboot",
            "--collect",
            "--",
            "systemctl",
            "reboot",
        ])
        .status()
        .map_err(|e| Status::internal(format!("starting reboot timer: {e}")))?;
    if status.success() {
        return Ok(());
    }
    // A previous timer may still be queued. Replace it.
    let _ = Command::new("systemctl")
        .args([
            "reset-failed",
            "kcore-node-reboot.timer",
            "kcore-node-reboot.service",
        ])
        .status();
    let status = Command::new("systemd-run")
        .args([
            "--on-active=2s",
            "--unit=kcore-node-reboot",
            "--collect",
            "--",
            "systemctl",
            "reboot",
        ])
        .status()
        .map_err(|e| Status::internal(format!("starting reboot timer: {e}")))?;
    if !status.success() {
        return Err(Status::internal("systemd-run did not schedule the reboot"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_manager_file, validate_manager_sha256, validate_manager_url};

    #[test]
    fn manager_file_rejects_paths() {
        assert!(validate_manager_file("NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run").is_ok());
        assert!(validate_manager_file("../x.run").is_err());
        assert!(validate_manager_file("nvidia.rpm").is_err());
        assert!(validate_manager_sha256(
            "2bc7f9be263d45a92331d781df33c14af1af01fcfa45e80102e80caf1ffaa74d"
        )
        .is_ok());
        assert!(validate_manager_sha256("abcd").is_err());
        assert!(validate_manager_url("http://192.168.40.10:18083/a.run").is_ok());
        assert!(validate_manager_url("file:///tmp/a.run").is_err());
    }
}
