//! Host-side LUKS over mapped RBD devices (E2 per-volume encryption).

use std::io::Write;
use std::process::{Command, Stdio};

use tempfile::NamedTempFile;

pub fn mapper_path(mapper_name: &str) -> String {
    format!("/dev/mapper/{}", mapper_name.trim())
}

pub fn validate_mapper_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 {
        return Err("mapper_name must be 1..=64 chars".into());
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("mapper_name must be [A-Za-z0-9_-]+".into());
    }
    Ok(())
}

fn run_cryptsetup(args: &[&str]) -> Result<(), String> {
    let out = Command::new("cryptsetup")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("cryptsetup: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cryptsetup {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn write_keyfile(dek: &[u8]) -> Result<NamedTempFile, String> {
    if dek.is_empty() {
        return Err("dek is required".into());
    }
    let mut f = NamedTempFile::new().map_err(|e| format!("temp keyfile: {e}"))?;
    f.write_all(dek)
        .map_err(|e| format!("write keyfile: {e}"))?;
    f.flush().map_err(|e| format!("flush keyfile: {e}"))?;
    Ok(f)
}

/// LUKS2 format + open. Device must already be mapped (RBD).
pub fn format_and_open(rbd_device: &str, dek: &[u8], mapper_name: &str) -> Result<String, String> {
    validate_mapper_name(mapper_name)?;
    let rbd = rbd_device.trim();
    if rbd.is_empty() || !rbd.starts_with("/dev/") {
        return Err("rbd_device must be an absolute /dev path".into());
    }
    let key = write_keyfile(dek)?;
    let key_path = key.path().to_str().unwrap_or("");
    run_cryptsetup(&[
        "luksFormat",
        "--type",
        "luks2",
        "--batch-mode",
        "--key-file",
        key_path,
        rbd,
    ])?;
    run_cryptsetup(&["open", "--key-file", key_path, rbd, mapper_name.trim()])?;
    Ok(mapper_path(mapper_name))
}

pub fn open(rbd_device: &str, dek: &[u8], mapper_name: &str) -> Result<String, String> {
    validate_mapper_name(mapper_name)?;
    let rbd = rbd_device.trim();
    if rbd.is_empty() || !rbd.starts_with("/dev/") {
        return Err("rbd_device must be an absolute /dev path".into());
    }
    let mapped = mapper_path(mapper_name);
    if std::path::Path::new(&mapped).exists() {
        return Ok(mapped);
    }
    let key = write_keyfile(dek)?;
    let key_path = key.path().to_str().unwrap_or("");
    run_cryptsetup(&["open", "--key-file", key_path, rbd, mapper_name.trim()])?;
    Ok(mapped)
}

pub fn close(mapper_name: &str) -> Result<(), String> {
    validate_mapper_name(mapper_name)?;
    let mapped = mapper_path(mapper_name);
    if !std::path::Path::new(&mapped).exists() {
        return Ok(());
    }
    run_cryptsetup(&["close", mapper_name.trim()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapper_name_sanitize() {
        assert!(validate_mapper_name("kcore-vol-abc").is_ok());
        assert!(validate_mapper_name("../etc").is_err());
        assert!(validate_mapper_name("").is_err());
    }

    #[test]
    fn mapper_path_format() {
        assert_eq!(mapper_path("kcore-vol-x"), "/dev/mapper/kcore-vol-x");
    }
}
