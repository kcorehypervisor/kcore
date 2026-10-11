//! Wipe a disk before it leaves commissioning, and check that it reads back as zeros.
//!
//! `quick` zeroes the first and last 32 MiB, which holds partition tables,
//! RAID and LVM metadata and the backup GPT. `full` zeroes every byte. Both
//! read the zeroed ranges back with direct I/O. A quick wipe does not make
//! file contents unrecoverable; use `full`, or a drive's secure erase, when
//! that matters.

use crate::{AlignedBuf, Disk, ALIGN};
use serde::Serialize;
use std::io::SeekFrom;

/// Bytes zeroed at each end of the disk in quick mode.
pub const QUICK_EDGE: u64 = 32 * 1024 * 1024;
const CHUNK: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WipeMode {
    Quick,
    Full,
}

impl WipeMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            WipeMode::Quick => "quick",
            WipeMode::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "quick" => Some(WipeMode::Quick),
            "full" => Some(WipeMode::Full),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WipeReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    pub drive_serial: String,
    pub device: String,
    pub phase: String,
    pub mode: String,
    pub passed: bool,
    pub bytes_zeroed: u64,
    pub bytes_verified: u64,
    pub write_errors: u32,
    pub read_errors: u32,
    pub nonzero_chunks: u32,
    pub detail: String,
}

/// Byte ranges `[start, end)` to zero. Aligned and non-overlapping.
pub fn ranges(size: u64, mode: WipeMode) -> Vec<(u64, u64)> {
    let size = size - size % ALIGN;
    if size == 0 {
        return Vec::new();
    }
    match mode {
        WipeMode::Full => vec![(0, size)],
        WipeMode::Quick => {
            if size <= QUICK_EDGE * 2 {
                vec![(0, size)]
            } else {
                vec![(0, QUICK_EDGE), (size - QUICK_EDGE, size)]
            }
        }
    }
}

pub struct WipeParams<'a> {
    pub device: &'a str,
    pub serial: &'a str,
    pub machine_id: Option<&'a str>,
    pub mode: WipeMode,
}

pub fn wipe<D: Disk>(disk: &mut D, p: &WipeParams) -> anyhow::Result<WipeReport> {
    let size = disk.seek(SeekFrom::End(0))?;
    let todo = ranges(size, p.mode);
    anyhow::ensure!(!todo.is_empty(), "device is smaller than {} bytes", ALIGN);

    let mut buf = AlignedBuf::new(CHUNK);
    let (mut zeroed, mut verified) = (0u64, 0u64);
    let (mut write_errors, mut read_errors, mut nonzero) = (0u32, 0u32, 0u32);

    for &(start, end) in &todo {
        let mut pos = start;
        while pos < end {
            let n = ((end - pos).min(CHUNK as u64)) as usize;
            buf.as_mut()[..n].fill(0);
            let r = disk
                .seek(SeekFrom::Start(pos))
                .and_then(|_| disk.write_all(&buf.as_ref()[..n]));
            match r {
                Ok(()) => zeroed += n as u64,
                Err(_) => write_errors += 1,
            }
            pos += n as u64;
        }
    }
    disk.flush_to_media()?;

    for &(start, end) in &todo {
        let mut pos = start;
        while pos < end {
            let n = ((end - pos).min(CHUNK as u64)) as usize;
            buf.as_mut()[..n].fill(0xA5);
            let r = disk
                .seek(SeekFrom::Start(pos))
                .and_then(|_| disk.read_exact(&mut buf.as_mut()[..n]));
            match r {
                Ok(()) if buf.as_ref()[..n].iter().all(|&b| b == 0) => verified += n as u64,
                Ok(()) => nonzero += 1,
                Err(_) => read_errors += 1,
            }
            pos += n as u64;
        }
    }

    let passed = write_errors == 0 && read_errors == 0 && nonzero == 0;
    let detail = if passed {
        format!(
            "{} wipe: {} bytes zeroed and read back as zeros",
            p.mode.as_str(),
            verified
        )
    } else {
        let mut parts = Vec::new();
        if write_errors > 0 {
            parts.push(format!("{} write errors", write_errors));
        }
        if read_errors > 0 {
            parts.push(format!("{} read errors", read_errors));
        }
        if nonzero > 0 {
            parts.push(format!("{} chunks did not read back as zeros", nonzero));
        }
        parts.join("; ")
    };

    Ok(WipeReport {
        machine_id: p.machine_id.map(|s| s.to_string()),
        drive_serial: p.serial.to_string(),
        device: p.device.to_string(),
        phase: "wipe".to_string(),
        mode: p.mode.as_str().to_string(),
        passed,
        bytes_zeroed: zeroed,
        bytes_verified: verified,
        write_errors,
        read_errors,
        nonzero_chunks: nonzero,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, Write};

    const MIB: u64 = 1024 * 1024;

    fn params(mode: WipeMode) -> WipeParams<'static> {
        WipeParams {
            device: "/dev/test",
            serial: "SER1",
            machine_id: None,
            mode,
        }
    }

    fn dirty(mib: usize) -> Cursor<Vec<u8>> {
        Cursor::new(vec![0xEE; mib * MIB as usize])
    }

    #[test]
    fn quick_ranges_cover_both_ends_only() {
        let r = ranges(1000 * MIB, WipeMode::Quick);
        assert_eq!(
            r,
            vec![(0, QUICK_EDGE), (1000 * MIB - QUICK_EDGE, 1000 * MIB)]
        );
    }

    #[test]
    fn small_disks_are_wiped_whole_in_quick_mode() {
        assert_eq!(ranges(40 * MIB, WipeMode::Quick), vec![(0, 40 * MIB)]);
        assert_eq!(ranges(64 * MIB, WipeMode::Quick), vec![(0, 64 * MIB)]);
    }

    #[test]
    fn ranges_drop_a_partial_trailing_sector_block() {
        assert_eq!(ranges(8192 + 100, WipeMode::Full), vec![(0, 8192)]);
        assert!(ranges(100, WipeMode::Full).is_empty());
    }

    #[test]
    fn quick_wipe_zeroes_the_ends_and_leaves_the_middle() {
        let mut d = dirty(200);
        let r = wipe(&mut d, &params(WipeMode::Quick)).unwrap();
        assert!(r.passed, "{r:?}");
        assert_eq!(r.bytes_zeroed, 2 * QUICK_EDGE);
        assert_eq!(r.bytes_verified, 2 * QUICK_EDGE);

        let data = d.get_ref();
        let edge = QUICK_EDGE as usize;
        assert!(data[..edge].iter().all(|&b| b == 0));
        assert!(data[data.len() - edge..].iter().all(|&b| b == 0));
        assert!(data[edge..data.len() - edge].iter().all(|&b| b == 0xEE));
    }

    #[test]
    fn full_wipe_zeroes_everything() {
        let mut d = dirty(48);
        let r = wipe(&mut d, &params(WipeMode::Full)).unwrap();
        assert!(r.passed, "{r:?}");
        assert_eq!(r.bytes_zeroed, 48 * MIB);
        assert!(d.get_ref().iter().all(|&b| b == 0));
    }

    /// Reads return stale data from `bad_from`, as a drive that ignores writes would.
    struct Ignores {
        inner: Cursor<Vec<u8>>,
        bad_from: u64,
    }

    impl Read for Ignores {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let pos = self.inner.position();
            let n = self.inner.read(buf)?;
            if pos >= self.bad_from {
                buf[..n].fill(0xEE);
            }
            Ok(n)
        }
    }

    impl Write for Ignores {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Ignores {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    impl Disk for Ignores {
        fn flush_to_media(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_disk_that_does_not_keep_the_zeros_fails() {
        let mut d = Ignores {
            inner: dirty(48),
            bad_from: 16 * MIB,
        };
        let r = wipe(&mut d, &params(WipeMode::Full)).unwrap();
        assert!(!r.passed);
        assert!(r.nonzero_chunks > 0);
        assert!(r.detail.contains("did not read back"), "{}", r.detail);
    }

    #[test]
    fn report_serialises_with_expected_fields() {
        let mut d = dirty(48);
        let r = wipe(&mut d, &params(WipeMode::Quick)).unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["phase"], "wipe");
        assert_eq!(v["mode"], "quick");
        assert!(v.get("machine_id").is_none());
    }

    #[test]
    fn parse_modes() {
        assert_eq!(WipeMode::parse("quick"), Some(WipeMode::Quick));
        assert_eq!(WipeMode::parse("full"), Some(WipeMode::Full));
        assert_eq!(WipeMode::parse("secure"), None);
    }
}
