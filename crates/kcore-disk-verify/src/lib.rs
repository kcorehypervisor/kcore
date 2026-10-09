//! Write-verify for commissioning: does this disk keep its data across a power cycle?
//!
//! BMC health and SMART do not catch a drive that acknowledges writes, reads
//! them back while powered, and returns zeros after power loss. That failure
//! was seen on a Dell T140 with an Intel SSDSC2BB300G4. This tool catches it
//! with two phases separated by a power cycle:
//!
//! 1. `write`: write a deterministic pattern to regions spread across the
//!    disk, read it back with direct I/O, report.
//! 2. power cycle the machine (`ForceOff` then `On`, not a warm reboot).
//! 3. `verify`: read the same regions and compare.
//!
//! The pattern is derived from the nonce, the drive serial and the offset, so
//! nothing has to be stored on the disk under test or carried across the
//! power cycle except the nonce.

pub mod device;

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};

/// Alignment for direct I/O: offsets, lengths and buffers are multiples of this.
pub const ALIGN: u64 = 4096;

pub const DEFAULT_REGIONS: usize = 32;
pub const DEFAULT_REGION_LEN: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Write,
    Verify,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Write => "write",
            Phase::Verify => "verify",
        }
    }
}

/// Result of one phase. Field names match `RecordDiskVerificationRequest`, so
/// the JSON can be sent to the controller as is (plus `machine_id`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    pub drive_serial: String,
    pub device: String,
    pub nonce: String,
    pub phase: String,
    pub passed: bool,
    pub bytes_checked: u64,
    pub regions_total: u32,
    pub regions_zeroed: u32,
    pub regions_mismatched: u32,
    pub read_errors: u32,
    pub detail: String,
}

/// A disk the phases can run against. `File` and in-memory cursors implement it.
pub trait Disk: Read + Write + Seek {
    fn flush_to_media(&mut self) -> std::io::Result<()>;
}

impl Disk for std::fs::File {
    fn flush_to_media(&mut self) -> std::io::Result<()> {
        self.sync_all()
    }
}

impl Disk for std::io::Cursor<Vec<u8>> {
    fn flush_to_media(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Fill `buf` with the pattern for the region starting at `offset`.
///
/// Each 32-byte block is `SHA-256(nonce, serial, offset, block index)`: not
/// repeating, not all zero, and different for every nonce, drive and offset.
pub fn fill_pattern(buf: &mut [u8], nonce: &str, serial: &str, offset: u64) {
    for (index, chunk) in buf.chunks_mut(32).enumerate() {
        let mut h = Sha256::new();
        h.update(nonce.as_bytes());
        h.update([0u8]);
        h.update(serial.as_bytes());
        h.update([0u8]);
        h.update(offset.to_le_bytes());
        h.update((index as u64).to_le_bytes());
        let digest = h.finalize();
        chunk.copy_from_slice(&digest[..chunk.len()]);
    }
}

fn align_down(v: u64) -> u64 {
    v - (v % ALIGN)
}

/// Pick region offsets: always the start and end of the disk (partition
/// tables and backup GPT live there), the middle, and pseudo-random ones.
///
/// Offsets are aligned, in bounds, sorted and do not overlap. The same inputs
/// give the same plan, so `verify` reproduces what `write` used.
pub fn plan(
    device_size: u64,
    region_len: u64,
    count: usize,
    nonce: &str,
    serial: &str,
) -> anyhow::Result<Vec<u64>> {
    anyhow::ensure!(
        region_len >= ALIGN && region_len.is_multiple_of(ALIGN),
        "region length must be a multiple of {} bytes",
        ALIGN
    );
    anyhow::ensure!(count >= 1, "at least one region is required");
    anyhow::ensure!(
        device_size >= region_len * 4,
        "device is too small: {} bytes, need at least {}",
        device_size,
        region_len * 4
    );

    // Leave room so random regions have somewhere to go.
    let max_regions = (device_size / (region_len * 2)).max(1) as usize;
    let count = count.min(max_regions);

    let last = align_down(device_size - region_len);
    let mut offsets = vec![0, last];
    if count >= 3 {
        offsets.push(align_down(device_size / 2 - region_len / 2));
    }

    let overlaps = |offsets: &[u64], candidate: u64| {
        offsets
            .iter()
            .any(|&o| candidate < o + region_len && o < candidate + region_len)
    };

    let mut counter = 0u64;
    while offsets.len() < count && counter < 100_000 {
        let mut h = Sha256::new();
        h.update(b"plan");
        h.update(nonce.as_bytes());
        h.update([0u8]);
        h.update(serial.as_bytes());
        h.update(counter.to_le_bytes());
        counter += 1;
        let digest = h.finalize();
        let raw = u64::from_le_bytes(digest[..8].try_into().unwrap());
        let candidate = align_down(raw % (device_size - region_len + 1));
        if !overlaps(&offsets, candidate) {
            offsets.push(candidate);
        }
    }

    offsets.truncate(count);
    offsets.sort_unstable();
    Ok(offsets)
}

/// A 4 KiB-aligned scratch buffer without `unsafe`: over-allocate and slice.
struct AlignedBuf {
    storage: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let storage = vec![0u8; len + ALIGN as usize];
        let start = storage.as_ptr().align_offset(ALIGN as usize);
        Self {
            storage,
            start,
            len,
        }
    }

    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.start + self.len]
    }

    fn as_ref(&self) -> &[u8] {
        &self.storage[self.start..self.start + self.len]
    }
}

pub struct Params<'a> {
    pub device: &'a str,
    pub serial: &'a str,
    pub nonce: &'a str,
    pub machine_id: Option<&'a str>,
    pub region_len: u64,
    pub regions: usize,
}

enum RegionState {
    Match,
    Zeroed,
    Mismatch,
    ReadError,
}

fn check_region<D: Disk>(
    disk: &mut D,
    offset: u64,
    expected: &[u8],
    scratch: &mut AlignedBuf,
) -> RegionState {
    if disk.seek(SeekFrom::Start(offset)).is_err() {
        return RegionState::ReadError;
    }
    let buf = scratch.as_mut();
    if disk.read_exact(buf).is_err() {
        return RegionState::ReadError;
    }
    if buf == expected {
        RegionState::Match
    } else if buf.iter().all(|&b| b == 0) {
        RegionState::Zeroed
    } else {
        RegionState::Mismatch
    }
}

/// Run one phase and report. `write` is destructive.
pub fn run_phase<D: Disk>(disk: &mut D, phase: Phase, p: &Params) -> anyhow::Result<Report> {
    let size = disk.seek(SeekFrom::End(0))?;
    let offsets = plan(size, p.region_len, p.regions, p.nonce, p.serial)?;
    let len = p.region_len as usize;

    let mut expected = AlignedBuf::new(len);
    let mut scratch = AlignedBuf::new(len);

    let (mut zeroed, mut mismatched, mut read_errors, mut write_errors) = (0u32, 0u32, 0u32, 0u32);
    let mut bytes_checked = 0u64;

    for &offset in &offsets {
        fill_pattern(expected.as_mut(), p.nonce, p.serial, offset);

        if phase == Phase::Write {
            let written = disk
                .seek(SeekFrom::Start(offset))
                .and_then(|_| disk.write_all(expected.as_ref()));
            if written.is_err() {
                write_errors += 1;
                continue;
            }
        }
    }

    if phase == Phase::Write {
        disk.flush_to_media()?;
    }

    for &offset in &offsets {
        fill_pattern(expected.as_mut(), p.nonce, p.serial, offset);
        match check_region(disk, offset, expected.as_ref(), &mut scratch) {
            RegionState::Match => bytes_checked += p.region_len,
            RegionState::Zeroed => zeroed += 1,
            RegionState::Mismatch => mismatched += 1,
            RegionState::ReadError => read_errors += 1,
        }
    }

    let total = offsets.len() as u32;
    let passed = zeroed == 0 && mismatched == 0 && read_errors == 0 && write_errors == 0;

    let detail = if passed {
        match phase {
            Phase::Write => format!("{} regions written and read back", total),
            Phase::Verify => format!("{} regions intact", total),
        }
    } else {
        let mut parts = Vec::new();
        if zeroed > 0 {
            parts.push(format!(
                "{} of {} regions read back as zeros",
                zeroed, total
            ));
        }
        if mismatched > 0 {
            parts.push(format!("{} regions hold wrong data", mismatched));
        }
        if read_errors > 0 {
            parts.push(format!("{} read errors", read_errors));
        }
        if write_errors > 0 {
            parts.push(format!("{} write errors", write_errors));
        }
        parts.join("; ")
    };

    Ok(Report {
        machine_id: p.machine_id.map(|s| s.to_string()),
        drive_serial: p.serial.to_string(),
        device: p.device.to_string(),
        nonce: p.nonce.to_string(),
        phase: phase.as_str().to_string(),
        passed,
        bytes_checked,
        regions_total: total,
        regions_zeroed: zeroed,
        regions_mismatched: mismatched,
        // Write failures are reported as read errors: the controller treats
        // both as an unreadable or unwritable region.
        read_errors: read_errors + write_errors,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const MIB: u64 = 1024 * 1024;

    fn params<'a>(nonce: &'a str, serial: &'a str) -> Params<'a> {
        Params {
            device: "/dev/test",
            serial,
            nonce,
            machine_id: None,
            region_len: MIB,
            regions: 8,
        }
    }

    fn disk(mib: usize) -> Cursor<Vec<u8>> {
        Cursor::new(vec![0u8; mib * MIB as usize])
    }

    #[test]
    fn pattern_is_deterministic_and_never_zero() {
        let mut a = vec![0u8; 4096];
        let mut b = vec![0u8; 4096];
        fill_pattern(&mut a, "n", "S", 0);
        fill_pattern(&mut b, "n", "S", 0);
        assert_eq!(a, b);
        assert!(a.iter().any(|&x| x != 0));
        assert!(!a.chunks(32).any(|c| c.iter().all(|&x| x == 0)));
    }

    #[test]
    fn pattern_differs_by_nonce_serial_and_offset() {
        let get = |n: &str, s: &str, o: u64| {
            let mut v = vec![0u8; 64];
            fill_pattern(&mut v, n, s, o);
            v
        };
        let base = get("n", "S", 0);
        assert_ne!(base, get("m", "S", 0));
        assert_ne!(base, get("n", "T", 0));
        assert_ne!(base, get("n", "S", 4096));
    }

    #[test]
    fn plan_is_aligned_in_bounds_sorted_and_non_overlapping() {
        let size = 300 * 1000 * MIB;
        let offsets = plan(size, MIB, 32, "n", "S").unwrap();
        assert_eq!(offsets.len(), 32);
        assert_eq!(offsets[0], 0);
        assert!(offsets.windows(2).all(|w| w[0] + MIB <= w[1]));
        assert!(offsets.iter().all(|&o| o % ALIGN == 0 && o + MIB <= size));
        assert!(offsets.contains(&align_down(size - MIB)));
    }

    #[test]
    fn plan_is_reproducible_and_depends_on_inputs() {
        let size = 64 * 1024 * MIB;
        let a = plan(size, MIB, 16, "n", "S").unwrap();
        assert_eq!(a, plan(size, MIB, 16, "n", "S").unwrap());
        assert_ne!(a, plan(size, MIB, 16, "other", "S").unwrap());
    }

    #[test]
    fn plan_rejects_bad_input() {
        assert!(plan(100 * MIB, 1000, 8, "n", "S").is_err());
        assert!(plan(3 * MIB, MIB, 8, "n", "S").is_err());
        assert!(plan(100 * MIB, MIB, 0, "n", "S").is_err());
    }

    #[test]
    fn plan_shrinks_the_region_count_on_small_disks() {
        let offsets = plan(16 * MIB, MIB, 32, "n", "S").unwrap();
        assert!(offsets.len() <= 8);
        assert!(offsets.windows(2).all(|w| w[0] + MIB <= w[1]));
    }

    #[test]
    fn healthy_disk_passes_write_then_verify() {
        let mut d = disk(64);
        let p = params("run1", "SER1");
        let w = run_phase(&mut d, Phase::Write, &p).unwrap();
        assert!(w.passed, "{w:?}");
        assert_eq!(w.regions_total, 8);
        assert_eq!(w.bytes_checked, 8 * MIB);

        let v = run_phase(&mut d, Phase::Verify, &p).unwrap();
        assert!(v.passed, "{v:?}");
        assert_eq!(v.phase, "verify");
    }

    /// The .22 incident in miniature: writes read back fine while powered,
    /// then the disk returns zeros after the power cycle.
    #[test]
    fn disk_that_forgets_everything_on_power_loss_is_caught() {
        let mut d = disk(64);
        let p = params("run1", "CVWL426100E9300PGN");

        let w = run_phase(&mut d, Phase::Write, &p).unwrap();
        assert!(w.passed, "the write phase cannot see this failure");

        d.get_mut().iter_mut().for_each(|b| *b = 0);

        let v = run_phase(&mut d, Phase::Verify, &p).unwrap();
        assert!(!v.passed);
        assert_eq!(v.regions_zeroed, v.regions_total);
        assert_eq!(v.regions_mismatched, 0);
        assert!(v.detail.contains("zeros"), "{}", v.detail);
    }

    #[test]
    fn corrupted_region_is_a_mismatch_not_zeros() {
        let mut d = disk(64);
        let p = params("run1", "SER1");
        run_phase(&mut d, Phase::Write, &p).unwrap();

        let offsets = plan(64 * MIB, MIB, 8, "run1", "SER1").unwrap();
        let victim = offsets[3] as usize;
        d.get_mut()[victim + 100] ^= 0xff;

        let v = run_phase(&mut d, Phase::Verify, &p).unwrap();
        assert!(!v.passed);
        assert_eq!(v.regions_mismatched, 1);
        assert_eq!(v.regions_zeroed, 0);
    }

    #[test]
    fn data_from_a_previous_run_does_not_pass_a_new_nonce() {
        let mut d = disk(64);
        run_phase(&mut d, Phase::Write, &params("old", "SER1")).unwrap();
        let v = run_phase(&mut d, Phase::Verify, &params("new", "SER1")).unwrap();
        assert!(!v.passed);
    }

    #[test]
    fn data_from_another_drive_does_not_pass() {
        let mut d = disk(64);
        run_phase(&mut d, Phase::Write, &params("n", "OTHER")).unwrap();
        let v = run_phase(&mut d, Phase::Verify, &params("n", "SER1")).unwrap();
        assert!(!v.passed);
    }

    /// Disk whose reads fail at and beyond `fail_from`.
    struct Flaky {
        inner: Cursor<Vec<u8>>,
        fail_from: u64,
    }

    impl Read for Flaky {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.inner.position() >= self.fail_from {
                return Err(std::io::Error::other("medium error"));
            }
            self.inner.read(buf)
        }
    }

    impl Write for Flaky {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    impl Seek for Flaky {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    impl Disk for Flaky {
        fn flush_to_media(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn unreadable_regions_are_counted_as_read_errors() {
        let mut d = Flaky {
            inner: disk(64),
            fail_from: 32 * MIB,
        };
        let r = run_phase(&mut d, Phase::Write, &params("run1", "SER1")).unwrap();
        assert!(!r.passed);
        assert!(r.read_errors > 0);
        assert!(r.regions_zeroed == 0 && r.regions_mismatched == 0);
        assert!(r.detail.contains("read errors"), "{}", r.detail);
    }

    #[test]
    fn report_serialises_with_controller_field_names() {
        let mut d = disk(64);
        let mut p = params("run1", "SER1");
        p.machine_id = Some("m-1");
        let r = run_phase(&mut d, Phase::Write, &p).unwrap();
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        for key in [
            "machine_id",
            "drive_serial",
            "device",
            "nonce",
            "phase",
            "passed",
            "bytes_checked",
            "regions_total",
            "regions_zeroed",
            "regions_mismatched",
            "read_errors",
            "detail",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }

        p.machine_id = None;
        let r = run_phase(&mut d, Phase::Write, &p).unwrap();
        assert!(serde_json::to_value(&r)
            .unwrap()
            .get("machine_id")
            .is_none());
    }
}
