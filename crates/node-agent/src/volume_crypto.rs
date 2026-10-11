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
        if mapper_backed_by(mapper_name, rbd) {
            return Ok(mapped);
        }
        // The nbd under this mapper was unmapped (or re-mapped elsewhere); an
        // open mapper over a dead or foreign device must not be reused.
        run_cryptsetup(&["close", mapper_name.trim()])?;
    }
    let key = write_keyfile(dek)?;
    let key_path = key.path().to_str().unwrap_or("");
    run_cryptsetup(&["open", "--key-file", key_path, rbd, mapper_name.trim()])?;
    Ok(mapped)
}

/// Grow an open mapper to the size of its backing device after `rbd resize`.
pub fn resize(mapper_name: &str, dek: &[u8]) -> Result<(), String> {
    validate_mapper_name(mapper_name)?;
    if !std::path::Path::new(&mapper_path(mapper_name)).exists() {
        return Ok(());
    }
    let key = write_keyfile(dek)?;
    let key_path = key.path().to_str().unwrap_or("");
    run_cryptsetup(&["resize", "--key-file", key_path, mapper_name.trim()])
}

/// True when `mapper_name`'s only backing device is the one `rbd_device` resolves to.
fn mapper_backed_by(mapper_name: &str, rbd_device: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(rbd_device) else {
        return false;
    };
    let rdev = meta.rdev();
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    let Ok(out) = Command::new("dmsetup")
        .args(["table", mapper_name.trim()])
        .output()
    else {
        return false;
    };
    out.status.success()
        && table_backing_device(&String::from_utf8_lossy(&out.stdout)) == Some((major, minor))
}

/// Backing `major:minor` from a `dmsetup table` crypt line
/// (`0 <len> crypt <cipher> <key> <iv_off> <major:minor> <offset>`).
fn table_backing_device(table: &str) -> Option<(u64, u64)> {
    let fields: Vec<&str> = table.split_whitespace().collect();
    if fields.get(2) != Some(&"crypt") {
        return None;
    }
    let (maj, min) = fields.get(6)?.split_once(':')?;
    Some((maj.parse().ok()?, min.parse().ok()?))
}

/// `/dev/nbdN` under an open crypt mapper, if any.
fn backing_nbd(mapper_name: &str) -> Option<String> {
    let out = Command::new("dmsetup")
        .args(["table", mapper_name.trim()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let (major, minor) = table_backing_device(&String::from_utf8_lossy(&out.stdout))?;
    let link = std::fs::read_link(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    let name = link.file_name()?.to_str()?;
    name.starts_with("nbd").then(|| format!("/dev/{name}"))
}

/// `cryptsetup close` without touching the nbd underneath (the caller owns it).
pub fn close_mapper_only(mapper_name: &str) -> Result<(), String> {
    validate_mapper_name(mapper_name)?;
    if !std::path::Path::new(&mapper_path(mapper_name)).exists() {
        return Ok(());
    }
    run_cryptsetup(&["close", mapper_name.trim()])
}

/// Close the mapper and release the nbd it was holding mapped.
pub fn close(mapper_name: &str) -> Result<(), String> {
    validate_mapper_name(mapper_name)?;
    let mapped = mapper_path(mapper_name);
    if !std::path::Path::new(&mapped).exists() {
        return Ok(());
    }
    let nbd = backing_nbd(mapper_name);
    run_cryptsetup(&["close", mapper_name.trim()])?;
    if let Some(dev) = nbd {
        if let Err(e) = crate::rbd_nbd::unmap_device(&dev) {
            tracing::warn!(device = %dev, error = %e, "nbd left mapped after closing mapper");
        }
    }
    Ok(())
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
    fn crypt_table_backing_device_parsed() {
        let t = "0 6258688 crypt aes-xts-plain64 :64:logon:cryptsetup:d14d-d0 0 43:3 32768\n";
        assert_eq!(table_backing_device(t), Some((43, 3)));
        assert_eq!(table_backing_device("0 10 linear 8:0 0"), None);
    }

    #[test]
    fn mapper_path_format() {
        assert_eq!(mapper_path("kcore-vol-x"), "/dev/mapper/kcore-vol-x");
    }
}
