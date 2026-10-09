//! Opening block devices safely and identifying them.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;

/// Open `path` for the phase. Direct I/O bypasses the page cache so a verify
/// run reads the media, not memory.
pub fn open(path: &str, write: bool, direct: bool) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(write);
    if direct {
        opts.custom_flags(libc::O_DIRECT);
    }
    opts.open(path)
        .with_context(|| format!("cannot open {}", path))
}

/// Refuse anything that is not a plain block device, unless a regular file is
/// explicitly allowed (tests, loop images).
pub fn ensure_block_device(path: &str, allow_file: bool) -> Result<()> {
    let meta = std::fs::metadata(path).with_context(|| format!("cannot stat {}", path))?;
    let ft = meta.file_type();
    if ft.is_block_device() {
        return Ok(());
    }
    if allow_file && ft.is_file() {
        return Ok(());
    }
    bail!("{} is not a block device", path)
}

/// True when `source` (a `/proc/mounts` or `/proc/swaps` device column) is the
/// device itself or one of its partitions (`sdb1`, `nvme0n1p2`).
pub fn is_device_or_partition(source: &str, device: &str) -> bool {
    if source == device {
        return true;
    }
    let Some(rest) = source.strip_prefix(device) else {
        return false;
    };
    let digits = if device.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        match rest.strip_prefix('p') {
            Some(r) => r,
            None => return false,
        }
    } else {
        rest
    };
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Whether the device or any partition appears in a mounts/swaps listing.
pub fn in_use(listing: &str, device: &str) -> bool {
    listing
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .any(|src| is_device_or_partition(src, device))
}

/// Refuse devices that are mounted, used as swap, or held by another device
/// (LVM, dm-crypt, md).
pub fn ensure_unused(device: &str) -> Result<()> {
    for file in ["/proc/mounts", "/proc/swaps"] {
        if let Ok(text) = std::fs::read_to_string(file) {
            if in_use(&text, device) {
                bail!(
                    "{} is in use (listed in {}); refusing to write",
                    device,
                    file
                );
            }
        }
    }

    let real = std::fs::canonicalize(device).unwrap_or_else(|_| Path::new(device).to_path_buf());
    if let Some(name) = real.file_name().and_then(|n| n.to_str()) {
        let holders = format!("/sys/class/block/{}/holders", name);
        if let Ok(mut entries) = std::fs::read_dir(&holders) {
            if entries.next().is_some() {
                bail!("{} is held by another device; refusing to write", device);
            }
        }
        // Partitions of this disk may be held too.
        if let Ok(parts) = std::fs::read_dir(format!("/sys/class/block/{}", name)) {
            for part in parts.flatten() {
                let pname = part.file_name();
                let pname = pname.to_string_lossy();
                if pname.starts_with(name) {
                    if let Ok(mut h) = std::fs::read_dir(part.path().join("holders")) {
                        if h.next().is_some() {
                            bail!(
                                "a partition of {} is held by another device; refusing to write",
                                device
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Serial number as the BMC reports it. NVMe exposes `device/serial`; SATA and
/// SAS expose the unit serial in VPD page 0x80.
pub fn read_serial(device: &str) -> Result<String> {
    let real = std::fs::canonicalize(device)?;
    let name = real
        .file_name()
        .and_then(|n| n.to_str())
        .context("device has no name")?;

    let direct = format!("/sys/class/block/{}/device/serial", name);
    if let Ok(s) = std::fs::read_to_string(&direct) {
        let s = s.trim();
        if !s.is_empty() {
            return Ok(s.to_string());
        }
    }

    let vpd = format!("/sys/class/block/{}/device/vpd_pg80", name);
    if let Ok(bytes) = std::fs::read(&vpd) {
        if let Some(s) = parse_vpd_pg80(&bytes) {
            return Ok(s);
        }
    }

    bail!("cannot determine the serial of {}; pass --serial", device)
}

/// Whether a `/dev/disk/by-id` entry name identifies the drive with `serial`.
/// Partition links (`-partN`) never match.
pub fn by_id_matches(name: &str, serial: &str) -> bool {
    if serial.is_empty() || name.contains("-part") {
        return false;
    }
    let name = name.to_ascii_uppercase();
    let serial = serial.to_ascii_uppercase();
    name.ends_with(&format!("_{}", serial)) || name.ends_with(&format!("-{}", serial))
}

/// Find the device node for a drive serial through `/dev/disk/by-id`, so the
/// verify phase still finds the disk when kernel names change after a reboot.
pub fn resolve_by_serial(serial: &str) -> Result<String> {
    let mut found = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir("/dev/disk/by-id").context("cannot read /dev/disk/by-id")? {
        let entry = entry?;
        let name = entry.file_name();
        if by_id_matches(&name.to_string_lossy(), serial) {
            if let Ok(real) = std::fs::canonicalize(entry.path()) {
                found.insert(real.to_string_lossy().to_string());
            }
        }
    }
    let mut it = found.into_iter();
    match (it.next(), it.next()) {
        (Some(one), None) => Ok(one),
        (None, _) => bail!("no disk with serial {} found", serial),
        (Some(a), Some(b)) => bail!(
            "serial {} matches several devices ({}, {}, ...)",
            serial,
            a,
            b
        ),
    }
}

/// VPD page 0x80: 4-byte header, then the serial padded with spaces or NULs.
pub fn parse_vpd_pg80(bytes: &[u8]) -> Option<String> {
    let body = bytes.get(4..)?;
    let s: String = body
        .iter()
        .map(|&b| b as char)
        .filter(|c| !c.is_control())
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_are_recognised() {
        assert!(is_device_or_partition("/dev/sdb", "/dev/sdb"));
        assert!(is_device_or_partition("/dev/sdb1", "/dev/sdb"));
        assert!(is_device_or_partition("/dev/sdb12", "/dev/sdb"));
        assert!(!is_device_or_partition("/dev/sdba1", "/dev/sdb"));
        assert!(!is_device_or_partition("/dev/sdc1", "/dev/sdb"));
        assert!(!is_device_or_partition("/dev/sdb", "/dev/sdb1"));
        assert!(is_device_or_partition("/dev/nvme0n1p2", "/dev/nvme0n1"));
        assert!(!is_device_or_partition("/dev/nvme0n12", "/dev/nvme0n1"));
        assert!(!is_device_or_partition("/dev/nvme0n1", "/dev/nvme0n1p1"));
    }

    #[test]
    fn mounts_listing() {
        let mounts =
            "/dev/sda2 / ext4 rw 0 0\n/dev/sdb1 /data xfs rw 0 0\ntmpfs /run tmpfs rw 0 0\n";
        assert!(in_use(mounts, "/dev/sda"));
        assert!(in_use(mounts, "/dev/sdb"));
        assert!(!in_use(mounts, "/dev/sdc"));
        assert!(!in_use("", "/dev/sdb"));
    }

    #[test]
    fn by_id_names() {
        let s = "BTWL4220020V300PGN";
        assert!(by_id_matches(
            "ata-INTEL_SSDSC2BB300G4_BTWL4220020V300PGN",
            s
        ));
        assert!(by_id_matches(
            "ata-intel_ssdsc2bb300g4_btwl4220020v300pgn",
            s
        ));
        assert!(by_id_matches("nvme-Samsung_SSD_980_BTWL4220020V300PGN", s));
        assert!(!by_id_matches(
            "ata-INTEL_SSDSC2BB300G4_BTWL4220020V300PGN-part1",
            s
        ));
        assert!(!by_id_matches("ata-INTEL_SSDSC2BB300G4_OTHER", s));
        assert!(!by_id_matches("wwn-0x55cd2e404b64e3f8", s));
        assert!(!by_id_matches("anything", ""));
    }

    #[test]
    fn vpd_serial_parsing() {
        let mut page = vec![0x00, 0x80, 0x00, 0x14];
        page.extend_from_slice(b"BTWL4220020V300PGN  ");
        assert_eq!(parse_vpd_pg80(&page).as_deref(), Some("BTWL4220020V300PGN"));

        let mut nul = vec![0x00, 0x80, 0x00, 0x08];
        nul.extend_from_slice(b"ABC123\0\0");
        assert_eq!(parse_vpd_pg80(&nul).as_deref(), Some("ABC123"));

        assert_eq!(parse_vpd_pg80(&[0, 0x80, 0, 0]), None);
        assert_eq!(parse_vpd_pg80(&[0, 1]), None);
    }

    #[test]
    fn regular_files_need_explicit_permission() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let p = f.path().to_str().unwrap();
        assert!(ensure_block_device(p, true).is_ok());
        assert!(ensure_block_device(p, false).is_err());
        assert!(ensure_block_device("/dev/null", true).is_err());
    }
}
