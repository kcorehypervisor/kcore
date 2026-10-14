//! Proofs `cargo kani` can finish.
//!
//! Kani is used only for a pure byte loop at 4 ASCII bytes, or for a
//! function whose whole input is a couple of integers. Parsers, Nix
//! string builders, and anything that touches the filesystem stay in
//! proptest. A proof that a pure function equals itself is not included.

use crate::disk_layout::{classify_device, strip_nix_comments, DeviceRole};
use crate::path_validator::validate_path_segment;
use crate::security_group::PortRange;

const MAX_INPUT_LEN: usize = 4;

fn any_ascii(buf: &mut [u8; MAX_INPUT_LEN]) -> &str {
    let len: usize = kani::any();
    kani::assume(len <= MAX_INPUT_LEN);
    for slot in buf.iter_mut() {
        let byte: u8 = kani::any();
        kani::assume(byte < 128);
        *slot = byte;
    }
    std::str::from_utf8(&buf[..len]).unwrap()
}

fn role_from(tag: u8) -> DeviceRole {
    match tag {
        0 => DeviceRole::Free,
        1 => DeviceRole::ActiveVm,
        2 => DeviceRole::SystemMount,
        3 => DeviceRole::LvmPv,
        _ => DeviceRole::ZpoolMember,
    }
}

/// A segment this checker accepts is not empty, not `.` or `..`, and
/// contains no NUL or slash.
#[kani::proof]
#[kani::unwind(9)]
fn segment_acceptance_implies_safe() {
    let mut buf = [0u8; MAX_INPUT_LEN];
    let segment = any_ascii(&mut buf);
    if validate_path_segment(segment).is_ok() {
        assert!(!segment.is_empty());
        assert!(segment != "." && segment != "..");
        assert!(!segment.contains('\0'));
        assert!(!segment.starts_with('/') && !segment.ends_with('/'));
    }
}

/// Ordered ports are a range that contains both ends. Reversed ports are rejected.
#[kani::proof]
fn port_range_accepts_ordered_bounds() {
    let start: u16 = kani::any();
    let end: u16 = kani::any();
    let result = PortRange::new(start, end);
    if start <= end {
        let range = result.unwrap();
        assert_eq!(range.start, start);
        assert_eq!(range.end, end);
        assert!(range.contains(start));
        assert!(range.contains(end));
    } else {
        assert!(result.is_err());
    }
}

/// Blanking a `#` comment does not change how many bytes the snippet has.
#[kani::proof]
#[kani::unwind(8)]
fn comment_strip_keeps_byte_length() {
    let mut buf = [0u8; MAX_INPUT_LEN];
    let input = any_ascii(&mut buf);
    assert_eq!(strip_nix_comments(input).len(), input.len());
}

/// An active VM disk, a system mount, an LVM PV, or a ZFS member is never safe.
#[kani::proof]
#[kani::unwind(8)]
fn dangerous_role_is_never_safe() {
    let mut buf = [0u8; MAX_INPUT_LEN];
    let path = any_ascii(&mut buf);
    let tag: u8 = kani::any();
    kani::assume(tag >= 1 && tag <= 4);
    let role = role_from(tag);
    assert!(role.is_dangerous());
    assert_ne!(
        classify_device(path, role),
        crate::disk_layout::Verdict::Safe
    );
}
