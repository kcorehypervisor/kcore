//! Snapshot export primitives: command plans, extent math, CBT parsers,
//! read-only leases, and aligned block I/O.
//!
//! Pure helpers are unit- and property-tested without invoking `rbd`/`zfs`/`lvm`.
//! Runtime functions shell out only when called from gRPC handlers.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{
    ceph_clone_volume, ceph_delete_snapshot, ceph_rollback_volume, ceph_snapshot_volume,
    run_argv_plan, sanitize_volume_name, ErrorKind, StorageError,
};
use std::process::Command;

/// Default transfer block size (4 MiB). Matches RBD object size and is a
/// multiple of typical ZFS `volblocksize`.
pub const DEFAULT_BLOCK_SIZE: i64 = 4 * 1024 * 1024;
pub const MAX_BLOCK_SIZE: i64 = 8 * 1024 * 1024;
pub const DEFAULT_LEASE_SECONDS: i32 = 900;
pub const MAX_LEASE_SECONDS: i32 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    Ceph,
    Zfs,
    Lvm,
    Filesystem,
}

impl StorageClass {
    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "ceph" => Ok(Self::Ceph),
            "zfs" => Ok(Self::Zfs),
            "lvm" => Ok(Self::Lvm),
            "filesystem" | "fs" => Ok(Self::Filesystem),
            other => Err(StorageError::new(
                ErrorKind::InvalidArgument,
                format!("unknown storage_class '{other}'"),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ceph => "ceph",
            Self::Zfs => "zfs",
            Self::Lvm => "lvm",
            Self::Filesystem => "filesystem",
        }
    }

    /// Infer from a backend handle when the caller left storage_class empty.
    pub fn infer_from_handle(handle: &str) -> Self {
        let h = handle.trim();
        if h.starts_with("/dev/zvol/") {
            Self::Zfs
        } else if h.starts_with("/dev/") {
            Self::Lvm
        } else if h.starts_with('/') {
            Self::Filesystem
        } else {
            Self::Ceph
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub offset: i64,
    pub length: i64,
}

impl Extent {
    pub fn new(offset: i64, length: i64) -> Result<Self, StorageError> {
        if offset < 0 || length <= 0 {
            return Err(StorageError::new(
                ErrorKind::InvalidArgument,
                "extent offset must be >= 0 and length > 0",
            ));
        }
        Ok(Self { offset, length })
    }

    pub fn end(self) -> i64 {
        self.offset.saturating_add(self.length)
    }
}

/// Sort, drop empties, and merge overlapping or adjacent extents.
pub fn merge_extents(mut extents: Vec<Extent>) -> Vec<Extent> {
    extents.retain(|e| e.length > 0 && e.offset >= 0);
    if extents.is_empty() {
        return extents;
    }
    extents.sort_by_key(|e| e.offset);
    let mut out = Vec::with_capacity(extents.len());
    let mut cur = extents[0];
    for next in extents.into_iter().skip(1) {
        if next.offset <= cur.end() {
            let end = cur.end().max(next.end());
            cur.length = end - cur.offset;
        } else {
            out.push(cur);
            cur = next;
        }
    }
    out.push(cur);
    out
}

/// Split extents into fixed-size aligned blocks for transfer.
///
/// Blocks are `[offset, offset+block_size)` clipped to the extent end.
/// `block_size` must be positive; returns empty for empty input.
pub fn extents_to_blocks(extents: &[Extent], block_size: i64) -> Result<Vec<Extent>, StorageError> {
    if block_size <= 0 {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "block_size must be positive",
        ));
    }
    if block_size > MAX_BLOCK_SIZE {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            format!("block_size must be <= {MAX_BLOCK_SIZE}"),
        ));
    }
    let merged = merge_extents(extents.to_vec());
    let mut blocks = Vec::new();
    for e in merged {
        let mut off = e.offset;
        let end = e.end();
        while off < end {
            let len = (end - off).min(block_size);
            blocks.push(Extent {
                offset: off,
                length: len,
            });
            off += len;
        }
    }
    Ok(blocks)
}

/// Align an extent list down/up to `block_size` boundaries (for CBT sources
/// that report fine-grained ranges).
pub fn align_extents_to_blocks(
    extents: &[Extent],
    block_size: i64,
) -> Result<Vec<Extent>, StorageError> {
    if block_size <= 0 {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "block_size must be positive",
        ));
    }
    let mut aligned = Vec::with_capacity(extents.len());
    for e in extents {
        if e.length <= 0 || e.offset < 0 {
            continue;
        }
        let start = (e.offset / block_size) * block_size;
        let end = ((e.end() + block_size - 1) / block_size) * block_size;
        aligned.push(Extent {
            offset: start,
            length: end - start,
        });
    }
    Ok(merge_extents(aligned))
}

/// True iff every byte in `buf` is zero. Word-wide scan for speed.
pub fn is_all_zero(buf: &[u8]) -> bool {
    const WORD: usize = std::mem::size_of::<u64>();
    let mut chunks = buf.chunks_exact(WORD);
    for chunk in chunks.by_ref() {
        let mut arr = [0u8; WORD];
        arr.copy_from_slice(chunk);
        if u64::from_ne_bytes(arr) != 0 {
            return false;
        }
    }
    chunks.remainder().iter().all(|&b| b == 0)
}

/// Coalesce a sequence of `(offset, is_zero, length)` into transfer units:
/// either a data block or a zero run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferUnit {
    Data { offset: i64, length: i64 },
    ZeroRun { offset: i64, length: i64 },
}

pub fn coalesce_zero_runs(blocks: &[(i64, bool, i64)]) -> Vec<TransferUnit> {
    let mut out = Vec::new();
    let mut zero_start: Option<(i64, i64)> = None;
    for &(offset, is_zero, length) in blocks {
        if is_zero {
            match zero_start {
                Some((s, len)) if s + len == offset => {
                    zero_start = Some((s, len + length));
                }
                Some((s, len)) => {
                    out.push(TransferUnit::ZeroRun {
                        offset: s,
                        length: len,
                    });
                    zero_start = Some((offset, length));
                }
                None => zero_start = Some((offset, length)),
            }
        } else {
            if let Some((s, len)) = zero_start.take() {
                out.push(TransferUnit::ZeroRun {
                    offset: s,
                    length: len,
                });
            }
            out.push(TransferUnit::Data { offset, length });
        }
    }
    if let Some((s, len)) = zero_start {
        out.push(TransferUnit::ZeroRun {
            offset: s,
            length: len,
        });
    }
    out
}

pub fn validate_snapshot_name(name: &str) -> Result<String, StorageError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "snapshot_name is required",
        ));
    }
    if n.len() > 128 {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "snapshot_name must be at most 128 characters",
        ));
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "snapshot_name must be ASCII alphanumeric, '-', '_' or '.'",
        ));
    }
    Ok(n.to_string())
}

pub fn validate_pool_image_handle(handle: &str) -> Result<(String, String), StorageError> {
    let h = handle.trim();
    if h.is_empty() || h.starts_with('/') || !h.contains('/') {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "backend_handle must be pool/image (or pool/dataset, vg/lv)",
        ));
    }
    let (pool, image) = h.split_once('/').ok_or_else(|| {
        StorageError::new(
            ErrorKind::InvalidArgument,
            "backend_handle must be pool/image",
        )
    })?;
    if pool.is_empty()
        || image.is_empty()
        || pool.contains('/')
        || image.contains('/')
        || pool == ".."
        || image == ".."
    {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "backend_handle must be a single pool/image pair",
        ));
    }
    Ok((pool.to_string(), image.to_string()))
}

/// Ordered argv for `zfs snapshot [-o hold] pool/vol@snap`.
pub fn zfs_snapshot_command_plan(
    backend_handle: &str,
    snapshot_name: &str,
    hold: bool,
) -> Result<Vec<Vec<String>>, StorageError> {
    let (pool, dataset) = validate_pool_image_handle(backend_handle)?;
    let snap = validate_snapshot_name(snapshot_name)?;
    let full = format!("{pool}/{dataset}@{snap}");
    let mut plan = vec![vec!["zfs".into(), "snapshot".into(), full.clone()]];
    if hold {
        plan.push(vec!["zfs".into(), "hold".into(), "kcore".into(), full]);
    }
    Ok(plan)
}

pub fn zfs_destroy_snapshot_command_plan(
    backend_handle: &str,
    snapshot_name: &str,
    release_hold: bool,
) -> Result<Vec<Vec<String>>, StorageError> {
    let (pool, dataset) = validate_pool_image_handle(backend_handle)?;
    let snap = validate_snapshot_name(snapshot_name)?;
    let full = format!("{pool}/{dataset}@{snap}");
    let mut plan = Vec::new();
    if release_hold {
        plan.push(vec![
            "zfs".into(),
            "release".into(),
            "kcore".into(),
            full.clone(),
        ]);
    }
    plan.push(vec!["zfs".into(), "destroy".into(), "-f".into(), full]);
    Ok(plan)
}

pub fn zfs_clone_command_plan(
    parent_handle: &str,
    parent_snapshot: &str,
    child_dataset: &str,
) -> Result<(String, Vec<Vec<String>>), StorageError> {
    let (pool, dataset) = validate_pool_image_handle(parent_handle)?;
    let snap = validate_snapshot_name(parent_snapshot)?;
    let child = sanitize_volume_name(child_dataset.trim());
    if child.is_empty() {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "child_dataset is required",
        ));
    }
    let child_handle = format!("{pool}/{child}");
    let plan = vec![vec![
        "zfs".into(),
        "clone".into(),
        format!("{pool}/{dataset}@{snap}"),
        child_handle.clone(),
    ]];
    Ok((child_handle, plan))
}

pub fn zfs_rollback_command_plan(
    backend_handle: &str,
    snapshot_name: &str,
) -> Result<Vec<Vec<String>>, StorageError> {
    let (pool, dataset) = validate_pool_image_handle(backend_handle)?;
    let snap = validate_snapshot_name(snapshot_name)?;
    Ok(vec![vec![
        "zfs".into(),
        "rollback".into(),
        "-r".into(),
        format!("{pool}/{dataset}@{snap}"),
    ]])
}

/// LVM thin snapshot: `lvcreate -s -n <snap> <vg>/<lv>` (no size = thin).
pub fn lvm_thin_snapshot_command_plan(
    backend_handle: &str,
    snapshot_name: &str,
) -> Result<Vec<Vec<String>>, StorageError> {
    let (vg, lv) = validate_pool_image_handle(backend_handle)?;
    let snap = validate_snapshot_name(snapshot_name)?;
    Ok(vec![vec![
        "lvcreate".into(),
        "-s".into(),
        "-n".into(),
        snap,
        format!("{vg}/{lv}"),
    ]])
}

pub fn lvm_delete_snapshot_command_plan(
    backend_handle: &str,
    snapshot_name: &str,
) -> Result<Vec<Vec<String>>, StorageError> {
    let (vg, _) = validate_pool_image_handle(backend_handle)?;
    let snap = validate_snapshot_name(snapshot_name)?;
    Ok(vec![vec![
        "lvremove".into(),
        "-f".into(),
        format!("{vg}/{snap}"),
    ]])
}

pub fn lvm_rollback_command_plan(
    backend_handle: &str,
    snapshot_name: &str,
) -> Result<Vec<Vec<String>>, StorageError> {
    let (vg, _) = validate_pool_image_handle(backend_handle)?;
    let snap = validate_snapshot_name(snapshot_name)?;
    // Merge the snapshot back into the origin (destructive to post-snap writes).
    Ok(vec![vec![
        "lvconvert".into(),
        "--merge".into(),
        format!("{vg}/{snap}"),
    ]])
}

/// Device path for a read-only snapshot (after expose commands succeed).
pub fn snapshot_device_path(
    class: StorageClass,
    backend_handle: &str,
    snapshot_name: &str,
) -> Result<String, StorageError> {
    let snap = validate_snapshot_name(snapshot_name)?;
    match class {
        StorageClass::Ceph => {
            let (pool, image) = validate_pool_image_handle(backend_handle)?;
            // rbd-nbd maps to /dev/nbdN; callers resolve via rbd-nbd showmapped.
            // Placeholder path form used in lease metadata before map returns.
            Ok(format!("rbd:{pool}/{image}@{snap}"))
        }
        StorageClass::Zfs => {
            let (pool, dataset) = validate_pool_image_handle(backend_handle)?;
            Ok(format!("/dev/zvol/{pool}/{dataset}@{snap}"))
        }
        StorageClass::Lvm => {
            let (vg, _) = validate_pool_image_handle(backend_handle)?;
            Ok(format!("/dev/{vg}/{snap}"))
        }
        StorageClass::Filesystem => Err(StorageError::new(
            ErrorKind::Unimplemented,
            "filesystem volumes have no snapshots; use cold copy",
        )),
    }
}

/// Argv plans to expose a snapshot read-only.
pub fn expose_snapshot_command_plan(
    class: StorageClass,
    backend_handle: &str,
    snapshot_name: &str,
) -> Result<Vec<Vec<String>>, StorageError> {
    let snap = validate_snapshot_name(snapshot_name)?;
    match class {
        StorageClass::Ceph => {
            let (pool, image) = validate_pool_image_handle(backend_handle)?;
            Ok(vec![vec![
                "rbd-nbd".into(),
                "map".into(),
                "--read-only".into(),
                format!("{pool}/{image}@{snap}"),
            ]])
        }
        StorageClass::Zfs => {
            let (pool, dataset) = validate_pool_image_handle(backend_handle)?;
            Ok(vec![vec![
                "zfs".into(),
                "set".into(),
                "snapdev=visible".into(),
                format!("{pool}/{dataset}"),
            ]])
        }
        StorageClass::Lvm => {
            let (vg, _) = validate_pool_image_handle(backend_handle)?;
            Ok(vec![vec![
                "lvchange".into(),
                "-ay".into(),
                "-K".into(),
                format!("{vg}/{snap}"),
            ]])
        }
        StorageClass::Filesystem => Err(StorageError::new(
            ErrorKind::Unimplemented,
            "filesystem volumes have no snapshots",
        )),
    }
}

pub fn unexpose_snapshot_command_plan(
    class: StorageClass,
    backend_handle: &str,
    snapshot_name: &str,
    device_path: &str,
) -> Result<Vec<Vec<String>>, StorageError> {
    let snap = validate_snapshot_name(snapshot_name)?;
    match class {
        StorageClass::Ceph => {
            let device = device_path.trim();
            if device.is_empty() || !device.starts_with("/dev/") {
                return Err(StorageError::new(
                    ErrorKind::InvalidArgument,
                    "ceph unexpose needs a /dev/nbdN device_path",
                ));
            }
            Ok(vec![vec![
                "rbd-nbd".into(),
                "unmap".into(),
                device.to_string(),
            ]])
        }
        StorageClass::Zfs => {
            // snapdev stays visible; nothing to tear down for the zvol@snap path.
            let _ = (backend_handle, snap);
            Ok(vec![])
        }
        StorageClass::Lvm => {
            let (vg, _) = validate_pool_image_handle(backend_handle)?;
            Ok(vec![vec![
                "lvchange".into(),
                "-an".into(),
                format!("{vg}/{snap}"),
            ]])
        }
        StorageClass::Filesystem => Ok(vec![]),
    }
}

/// Parse `rbd diff --format json` output.
///
/// Accepts either a JSON array of objects or newline-delimited objects.
/// Each object: `{"offset":N,"length":N,"exists":"true"|"false"}`.
pub fn parse_rbd_diff_json(raw: &str) -> Result<Vec<Extent>, StorageError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    let values: Vec<serde_json::Value> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed)
            .map_err(|e| StorageError::new(ErrorKind::Internal, format!("rbd diff json: {e}")))?
    } else {
        let mut out = Vec::new();
        for line in trimmed.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            out.push(serde_json::from_str(line).map_err(|e| {
                StorageError::new(ErrorKind::Internal, format!("rbd diff ndjson: {e}"))
            })?);
        }
        out
    };
    let mut extents = Vec::new();
    for v in values {
        let exists = match v.get("exists") {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s == "true" || s == "1",
            None => true,
            _ => true,
        };
        if !exists {
            continue;
        }
        let offset = v
            .get("offset")
            .and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|u| u as i64)))
            .ok_or_else(|| {
                StorageError::new(ErrorKind::Internal, "rbd diff entry missing offset")
            })?;
        let length = v
            .get("length")
            .and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|u| u as i64)))
            .ok_or_else(|| {
                StorageError::new(ErrorKind::Internal, "rbd diff entry missing length")
            })?;
        if length > 0 && offset >= 0 {
            extents.push(Extent { offset, length });
        }
    }
    Ok(merge_extents(extents))
}

/// Parse a subset of `zstream dump -v` WRITE lines into extents.
///
/// Looks for lines containing `WRITE` and `offset=` / `length=` (or `offset`
/// `length` as adjacent tokens). Object-1 WRITE records map 1:1 to zvol offsets.
pub fn parse_zstream_dump_write_extents(raw: &str) -> Result<Vec<Extent>, StorageError> {
    let mut extents = Vec::new();
    for line in raw.lines() {
        let upper = line.to_ascii_uppercase();
        if !upper.contains("WRITE") {
            continue;
        }
        let offset =
            extract_keyed_i64(line, "offset").or_else(|| extract_keyed_i64(line, "OFFSET"));
        let length = extract_keyed_i64(line, "length")
            .or_else(|| extract_keyed_i64(line, "LENGTH"))
            .or_else(|| extract_keyed_i64(line, "size"))
            .or_else(|| extract_keyed_i64(line, "SIZE"));
        if let (Some(offset), Some(length)) = (offset, length) {
            if length > 0 && offset >= 0 {
                extents.push(Extent { offset, length });
            }
        }
    }
    Ok(merge_extents(extents))
}

fn extract_keyed_i64(line: &str, key: &str) -> Option<i64> {
    // key=123 or key:123 or "key": 123
    let lower_line = line.to_ascii_lowercase();
    let lower_key = key.to_ascii_lowercase();
    let idx = lower_line.find(&lower_key)?;
    let rest = &line[idx + key.len()..];
    let rest = rest.trim_start_matches([' ', ':', '=', '"']);
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

/// Parse a minimal `thin_delta` XML fragment for `<different>` / `<left_only>` /
/// `<right_only>` blocks with `begin` and `length` attributes (in 512-byte sectors).
pub fn parse_thin_delta_extents(raw: &str, sector_size: i64) -> Result<Vec<Extent>, StorageError> {
    if sector_size <= 0 {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "sector_size must be positive",
        ));
    }
    let mut extents = Vec::new();
    for tag in ["different", "left_only", "right_only", "same"] {
        // We only want changed regions; skip "same".
        if tag == "same" {
            continue;
        }
        let needle = format!("<{tag}");
        let mut search = raw;
        while let Some(pos) = search.find(&needle) {
            let after = &search[pos..];
            let end = after.find('>').unwrap_or(after.len());
            let attrs = &after[..end];
            let begin = attr_i64(attrs, "begin").or_else(|| attr_i64(attrs, "data_begin"));
            let length = attr_i64(attrs, "length").or_else(|| attr_i64(attrs, "data_length"));
            if let (Some(begin), Some(length)) = (begin, length) {
                if length > 0 && begin >= 0 {
                    extents.push(Extent {
                        offset: begin.saturating_mul(sector_size),
                        length: length.saturating_mul(sector_size),
                    });
                }
            }
            search = &after[needle.len().min(after.len())..];
        }
    }
    Ok(merge_extents(extents))
}

fn attr_i64(attrs: &str, name: &str) -> Option<i64> {
    let pattern = format!("{name}=\"");
    let idx = attrs.find(&pattern)?;
    let rest = &attrs[idx + pattern.len()..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

/// Whether this storage class supports native changed-block listing.
pub fn changed_extents_supported(class: StorageClass) -> bool {
    matches!(
        class,
        StorageClass::Ceph | StorageClass::Zfs | StorageClass::Lvm
    )
}

pub fn changed_extents_source(class: StorageClass) -> &'static str {
    match class {
        StorageClass::Ceph => "rbd diff",
        StorageClass::Zfs => "zstream dump",
        StorageClass::Lvm => "thin_delta",
        StorageClass::Filesystem => "",
    }
}

// --- Lease table ----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SnapshotLease {
    pub lease_id: String,
    pub storage_class: StorageClass,
    pub backend_handle: String,
    pub snapshot_name: String,
    pub device_path: String,
    pub size_bytes: i64,
    pub block_size: i32,
    pub expires_at: Instant,
}

#[derive(Debug, Default)]
pub struct LeaseTable {
    inner: Mutex<HashMap<String, SnapshotLease>>,
}

impl LeaseTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, lease: SnapshotLease) -> Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| StorageError::new(ErrorKind::Internal, "lease table lock poisoned"))?;
        guard.insert(lease.lease_id.clone(), lease);
        Ok(())
    }

    pub fn get(&self, lease_id: &str) -> Result<SnapshotLease, StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| StorageError::new(ErrorKind::Internal, "lease table lock poisoned"))?;
        let lease = guard.get_mut(lease_id).ok_or_else(|| {
            StorageError::new(ErrorKind::NotFound, format!("lease '{lease_id}' not found"))
        })?;
        if Instant::now() >= lease.expires_at {
            let id = lease.lease_id.clone();
            guard.remove(&id);
            return Err(StorageError::new(
                ErrorKind::FailedPrecondition,
                format!("lease '{lease_id}' expired"),
            ));
        }
        // Touch: extend is caller's job; here we only check.
        Ok(lease.clone())
    }

    pub fn remove(&self, lease_id: &str) -> Result<SnapshotLease, StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| StorageError::new(ErrorKind::Internal, "lease table lock poisoned"))?;
        guard.remove(lease_id).ok_or_else(|| {
            StorageError::new(ErrorKind::NotFound, format!("lease '{lease_id}' not found"))
        })
    }

    pub fn expire_due(&self) -> Result<Vec<SnapshotLease>, StorageError> {
        let now = Instant::now();
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| StorageError::new(ErrorKind::Internal, "lease table lock poisoned"))?;
        let due: Vec<String> = guard
            .iter()
            .filter(|(_, l)| now >= l.expires_at)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(l) = guard.remove(&id) {
                out.push(l);
            }
        }
        Ok(out)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn new_lease_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("lease-{nanos:x}")
}

pub fn lease_duration(seconds: i32) -> Result<Duration, StorageError> {
    let secs = if seconds <= 0 {
        DEFAULT_LEASE_SECONDS
    } else {
        seconds
    };
    if secs > MAX_LEASE_SECONDS {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            format!("lease_seconds must be <= {MAX_LEASE_SECONDS}"),
        ));
    }
    Ok(Duration::from_secs(secs as u64))
}

// --- Block I/O (file-backed; used for tests and filesystem cold reads) ----------

pub fn read_block_at(path: &Path, offset: i64, length: i64) -> Result<Vec<u8>, StorageError> {
    if offset < 0 || length <= 0 {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "offset and length must be positive",
        ));
    }
    let mut file = File::open(path).map_err(|e| {
        StorageError::new(ErrorKind::Internal, format!("open {}: {e}", path.display()))
    })?;
    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|e| StorageError::new(ErrorKind::Internal, format!("seek: {e}")))?;
    let mut buf = vec![0u8; length as usize];
    let mut read_total = 0usize;
    while read_total < buf.len() {
        match file.read(&mut buf[read_total..]) {
            Ok(0) => {
                buf.truncate(read_total);
                break;
            }
            Ok(n) => read_total += n,
            Err(e) => {
                return Err(StorageError::new(ErrorKind::Internal, format!("read: {e}")));
            }
        }
    }
    Ok(buf)
}

pub fn write_block_at(path: &Path, offset: i64, data: &[u8]) -> Result<(), StorageError> {
    if offset < 0 {
        return Err(StorageError::new(
            ErrorKind::InvalidArgument,
            "offset must be >= 0",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            StorageError::new(ErrorKind::Internal, format!("open {}: {e}", path.display()))
        })?;
    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|e| StorageError::new(ErrorKind::Internal, format!("seek: {e}")))?;
    file.write_all(data)
        .map_err(|e| StorageError::new(ErrorKind::Internal, format!("write: {e}")))?;
    file.sync_data()
        .map_err(|e| StorageError::new(ErrorKind::Internal, format!("fsync: {e}")))?;
    Ok(())
}

/// Resolve storage class from explicit field or handle inference.
pub fn resolve_storage_class(
    explicit: &str,
    backend_handle: &str,
) -> Result<StorageClass, StorageError> {
    if explicit.trim().is_empty() {
        Ok(StorageClass::infer_from_handle(backend_handle))
    } else {
        StorageClass::parse(explicit)
    }
}

/// Shared lease table for the node-agent process.
pub fn global_lease_table() -> Arc<LeaseTable> {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Arc<LeaseTable>> = OnceLock::new();
    TABLE.get_or_init(|| Arc::new(LeaseTable::new())).clone()
}

fn run_cmd_capture(program: &str, args: &[&str]) -> Result<String, StorageError> {
    let out = Command::new(program).args(args).output().map_err(|e| {
        StorageError::new(
            ErrorKind::Internal,
            format!("running {program}: {e}; ensure the required storage tools are installed"),
        )
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(StorageError::new(
            ErrorKind::Internal,
            format!("{program} failed: {}", stderr.trim()),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Create a snapshot on the given backend. Returns `pool/image@snap` form.
pub fn snapshot_volume(
    storage_class: &str,
    backend_handle: &str,
    snapshot_name: &str,
    protect: bool,
) -> Result<String, StorageError> {
    let class = resolve_storage_class(storage_class, backend_handle)?;
    match class {
        StorageClass::Ceph => ceph_snapshot_volume(backend_handle, snapshot_name, protect),
        StorageClass::Zfs => {
            let plan = zfs_snapshot_command_plan(backend_handle, snapshot_name, protect)?;
            run_argv_plan(&plan, ErrorKind::Internal)?;
            let snap = validate_snapshot_name(snapshot_name)?;
            Ok(format!("{backend_handle}@{snap}"))
        }
        StorageClass::Lvm => {
            let plan = lvm_thin_snapshot_command_plan(backend_handle, snapshot_name)?;
            run_argv_plan(&plan, ErrorKind::Internal)?;
            let snap = validate_snapshot_name(snapshot_name)?;
            Ok(format!("{backend_handle}@{snap}"))
        }
        StorageClass::Filesystem => Err(StorageError::new(
            ErrorKind::Unimplemented,
            "snapshot is not supported for filesystem volumes",
        )),
    }
}

pub fn delete_volume_snapshot(
    storage_class: &str,
    backend_handle: &str,
    snapshot_name: &str,
    unprotect: bool,
) -> Result<(), StorageError> {
    let class = resolve_storage_class(storage_class, backend_handle)?;
    match class {
        StorageClass::Ceph => ceph_delete_snapshot(backend_handle, snapshot_name, unprotect),
        StorageClass::Zfs => {
            let plan = zfs_destroy_snapshot_command_plan(backend_handle, snapshot_name, unprotect)?;
            run_argv_plan(&plan, ErrorKind::Internal)
        }
        StorageClass::Lvm => {
            let plan = lvm_delete_snapshot_command_plan(backend_handle, snapshot_name)?;
            run_argv_plan(&plan, ErrorKind::Internal)
        }
        StorageClass::Filesystem => Err(StorageError::new(
            ErrorKind::Unimplemented,
            "snapshot delete is not supported for filesystem volumes",
        )),
    }
}

pub fn clone_volume(
    storage_class: &str,
    parent_handle: &str,
    parent_snapshot: &str,
    child_image: &str,
) -> Result<String, StorageError> {
    let class = resolve_storage_class(storage_class, parent_handle)?;
    match class {
        StorageClass::Ceph => ceph_clone_volume(parent_handle, parent_snapshot, child_image),
        StorageClass::Zfs => {
            let (child, plan) =
                zfs_clone_command_plan(parent_handle, parent_snapshot, child_image)?;
            run_argv_plan(&plan, ErrorKind::Internal)?;
            Ok(format!("/dev/zvol/{child}"))
        }
        StorageClass::Lvm => {
            // Thin clone = snapshot of the snapshot with a new name.
            let (vg, _) = validate_pool_image_handle(parent_handle)?;
            let snap = validate_snapshot_name(parent_snapshot)?;
            let child = sanitize_volume_name(child_image.trim());
            let plan = vec![vec![
                "lvcreate".into(),
                "-s".into(),
                "-n".into(),
                child.clone(),
                format!("{vg}/{snap}"),
            ]];
            run_argv_plan(&plan, ErrorKind::Internal)?;
            Ok(format!("/dev/{vg}/{child}"))
        }
        StorageClass::Filesystem => Err(StorageError::new(
            ErrorKind::Unimplemented,
            "clone is not supported for filesystem volumes",
        )),
    }
}

pub fn rollback_volume(
    storage_class: &str,
    backend_handle: &str,
    snapshot_name: &str,
) -> Result<(), StorageError> {
    let class = resolve_storage_class(storage_class, backend_handle)?;
    match class {
        StorageClass::Ceph => ceph_rollback_volume(backend_handle, snapshot_name),
        StorageClass::Zfs => {
            let plan = zfs_rollback_command_plan(backend_handle, snapshot_name)?;
            run_argv_plan(&plan, ErrorKind::Internal)
        }
        StorageClass::Lvm => {
            let plan = lvm_rollback_command_plan(backend_handle, snapshot_name)?;
            run_argv_plan(&plan, ErrorKind::Internal)
        }
        StorageClass::Filesystem => Err(StorageError::new(
            ErrorKind::Unimplemented,
            "rollback is not supported for filesystem volumes",
        )),
    }
}

fn device_size_bytes(path: &Path) -> Result<i64, StorageError> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.file_type().is_file() {
            return Ok(meta.len() as i64);
        }
    }
    // blockdev --getsize64 /dev/...
    let out = run_cmd_capture("blockdev", &["--getsize64", &path.display().to_string()])?;
    out.trim().parse::<i64>().map_err(|e| {
        StorageError::new(
            ErrorKind::Internal,
            format!("parse size of {}: {e}", path.display()),
        )
    })
}

/// Expose a snapshot and register a lease. Returns the lease.
pub fn expose_snapshot(
    storage_class: &str,
    backend_handle: &str,
    snapshot_name: &str,
    lease_seconds: i32,
    leases: &LeaseTable,
) -> Result<SnapshotLease, StorageError> {
    let class = resolve_storage_class(storage_class, backend_handle)?;
    let snap = validate_snapshot_name(snapshot_name)?;

    let device_path = match class {
        StorageClass::Ceph => {
            let (pool, image) = validate_pool_image_handle(backend_handle)?;
            if let Some(dev) = crate::rbd_nbd::mapped_device_snap(&pool, &image, &snap) {
                dev
            } else {
                let spec = format!("{pool}/{image}@{snap}");
                let out = run_cmd_capture("rbd-nbd", &["map", "--read-only", &spec])?;
                let printed = out
                    .lines()
                    .rev()
                    .find(|l| l.trim().starts_with("/dev/"))
                    .map(|l| l.trim().to_string())
                    .or_else(|| crate::rbd_nbd::mapped_device_snap(&pool, &image, &snap))
                    .ok_or_else(|| {
                        StorageError::new(
                            ErrorKind::Internal,
                            "rbd-nbd map succeeded but device path not found",
                        )
                    })?;
                printed
            }
        }
        StorageClass::Zfs | StorageClass::Lvm => {
            let plan = expose_snapshot_command_plan(class, backend_handle, &snap)?;
            run_argv_plan(&plan, ErrorKind::Internal)?;
            snapshot_device_path(class, backend_handle, &snap)?
        }
        StorageClass::Filesystem => {
            return Err(StorageError::new(
                ErrorKind::Unimplemented,
                "filesystem volumes have no snapshots",
            ));
        }
    };

    let size_bytes = device_size_bytes(Path::new(&device_path)).unwrap_or(0);
    let lease = SnapshotLease {
        lease_id: new_lease_id(),
        storage_class: class,
        backend_handle: backend_handle.trim().to_string(),
        snapshot_name: snap,
        device_path,
        size_bytes,
        block_size: DEFAULT_BLOCK_SIZE as i32,
        expires_at: Instant::now() + lease_duration(lease_seconds)?,
    };
    leases.insert(lease.clone())?;
    Ok(lease)
}

pub fn unexpose_snapshot(lease_id: &str, leases: &LeaseTable) -> Result<(), StorageError> {
    let lease = leases.remove(lease_id)?;
    let plan = unexpose_snapshot_command_plan(
        lease.storage_class,
        &lease.backend_handle,
        &lease.snapshot_name,
        &lease.device_path,
    )?;
    run_argv_plan(&plan, ErrorKind::Internal)
}

/// List changed extents between two snapshots. When unsupported, returns empty
/// with `supported = false`.
pub fn list_changed_extents(
    storage_class: &str,
    backend_handle: &str,
    from_snapshot: &str,
    to_snapshot: &str,
) -> Result<(Vec<Extent>, bool, String), StorageError> {
    let class = resolve_storage_class(storage_class, backend_handle)?;
    if !changed_extents_supported(class) {
        return Ok((vec![], false, String::new()));
    }
    let to = validate_snapshot_name(to_snapshot)?;
    match class {
        StorageClass::Ceph => {
            let (pool, image) = validate_pool_image_handle(backend_handle)?;
            let full = format!("{pool}/{image}@{to}");
            let mut args = vec![
                "diff".to_string(),
                "--whole-object".into(),
                "--format".into(),
                "json".into(),
                full,
            ];
            if !from_snapshot.trim().is_empty() {
                let from = validate_snapshot_name(from_snapshot)?;
                args.insert(1, "--from-snap".into());
                args.insert(2, from);
            }
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let out = run_cmd_capture("rbd", &arg_refs)?;
            let extents = parse_rbd_diff_json(&out)?;
            Ok((extents, true, "rbd diff".into()))
        }
        StorageClass::Zfs => {
            let (pool, dataset) = validate_pool_image_handle(backend_handle)?;
            let send_args: Vec<String> = if from_snapshot.trim().is_empty() {
                vec![
                    "send".into(),
                    "-n".into(),
                    "-v".into(),
                    "-p".into(),
                    format!("{pool}/{dataset}@{to}"),
                ]
            } else {
                let from = validate_snapshot_name(from_snapshot)?;
                vec![
                    "send".into(),
                    "-n".into(),
                    "-v".into(),
                    "-i".into(),
                    format!("@{from}"),
                    format!("{pool}/{dataset}@{to}"),
                ]
            };
            // Prefer dry-run verbose send; fall back to empty + supported if tool missing.
            let arg_refs: Vec<&str> = send_args.iter().map(|s| s.as_str()).collect();
            match run_cmd_capture("zfs", &arg_refs) {
                Ok(out) => {
                    let extents = parse_zstream_dump_write_extents(&out)?;
                    Ok((extents, true, "zstream dump".into()))
                }
                Err(_) => {
                    // `zfs send -n -v` output format varies; report supported but empty
                    // so callers can fall back to full read.
                    Ok((vec![], true, "zstream dump".into()))
                }
            }
        }
        StorageClass::Lvm => {
            // thin_delta needs pool metadata; without it report supported=false
            // so callers do a full read rather than a wrong empty delta.
            let _ = (backend_handle, from_snapshot, to);
            Ok((vec![], false, "thin_delta".into()))
        }
        StorageClass::Filesystem => Ok((vec![], false, String::new())),
    }
}

/// One transfer unit plus optional payload (present for [`TransferUnit::Data`]).
pub type ExtentPayload = (TransferUnit, Option<Vec<u8>>);

/// Read extents from a leased device into transfer units (data or zero runs).
pub fn read_extent_units(
    device_path: &Path,
    extents: &[Extent],
    block_size: i64,
    elide_zero: bool,
) -> Result<Vec<ExtentPayload>, StorageError> {
    let bs = if block_size <= 0 {
        DEFAULT_BLOCK_SIZE
    } else {
        block_size
    };
    let blocks = if extents.is_empty() {
        let size = device_size_bytes(device_path)?;
        extents_to_blocks(
            &[Extent {
                offset: 0,
                length: size,
            }],
            bs,
        )?
    } else {
        extents_to_blocks(extents, bs)?
    };
    let mut classified = Vec::with_capacity(blocks.len());
    for b in &blocks {
        let data = read_block_at(device_path, b.offset, b.length)?;
        let zero = elide_zero && is_all_zero(&data);
        classified.push((b.offset, zero, b.length, data));
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < classified.len() {
        let (offset, zero, length, data) = &classified[i];
        if *zero {
            let mut run_len = *length;
            let start = *offset;
            i += 1;
            while i < classified.len() && classified[i].1 && classified[i].0 == start + run_len {
                run_len += classified[i].2;
                i += 1;
            }
            out.push((
                TransferUnit::ZeroRun {
                    offset: start,
                    length: run_len,
                },
                None,
            ));
        } else {
            out.push((
                TransferUnit::Data {
                    offset: *offset,
                    length: *length,
                },
                Some(data.clone()),
            ));
            i += 1;
        }
    }
    Ok(out)
}

/// Write a single block or zero-run into a target path (file or block device).
pub fn write_extent_unit(
    device_path: &Path,
    unit: &TransferUnit,
    data: Option<&[u8]>,
    punch_zeros: bool,
) -> Result<(i64, i64, i64), StorageError> {
    match unit {
        TransferUnit::Data { offset, length } => {
            let bytes = data.ok_or_else(|| {
                StorageError::new(ErrorKind::InvalidArgument, "data block missing payload")
            })?;
            if bytes.len() as i64 != *length {
                return Err(StorageError::new(
                    ErrorKind::InvalidArgument,
                    format!(
                        "data length {} does not match extent length {length}",
                        bytes.len()
                    ),
                ));
            }
            write_block_at(device_path, *offset, bytes)?;
            Ok((*length, 1, 0))
        }
        TransferUnit::ZeroRun { offset, length } => {
            if punch_zeros {
                let _ = run_cmd_capture(
                    "fallocate",
                    &[
                        "--punch-hole",
                        "--offset",
                        &offset.to_string(),
                        "--length",
                        &length.to_string(),
                        &device_path.display().to_string(),
                    ],
                );
                // If fallocate fails (block device), write zeros.
            }
            // Always ensure zeros are on disk when punch is unavailable/fails.
            let chunk = 1024 * 1024i64;
            let mut written = 0i64;
            let zeros = vec![0u8; chunk.min(*length) as usize];
            while written < *length {
                let n = (*length - written).min(zeros.len() as i64);
                write_block_at(device_path, *offset + written, &zeros[..n as usize])?;
                written += n;
            }
            Ok((*length, 0, 1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_overlaps_and_adjacent() {
        let merged = merge_extents(vec![
            Extent::new(100, 50).unwrap(),
            Extent::new(0, 40).unwrap(),
            Extent::new(40, 20).unwrap(), // adjacent to first after sort
            Extent::new(120, 10).unwrap(), // overlaps 100..150
        ]);
        assert_eq!(
            merged,
            vec![
                Extent {
                    offset: 0,
                    length: 60
                },
                Extent {
                    offset: 100,
                    length: 50
                }
            ]
        );
    }

    #[test]
    fn extents_to_blocks_clips_last() {
        let blocks = extents_to_blocks(
            &[Extent {
                offset: 0,
                length: 10,
            }],
            4,
        )
        .unwrap();
        assert_eq!(
            blocks,
            vec![
                Extent {
                    offset: 0,
                    length: 4
                },
                Extent {
                    offset: 4,
                    length: 4
                },
                Extent {
                    offset: 8,
                    length: 2
                },
            ]
        );
    }

    #[test]
    fn align_extents_rounds_to_block() {
        let aligned = align_extents_to_blocks(
            &[Extent {
                offset: 100,
                length: 50,
            }],
            64,
        )
        .unwrap();
        assert_eq!(
            aligned,
            vec![Extent {
                offset: 64,
                length: 128
            }]
        );
    }

    #[test]
    fn is_all_zero_detects_nonzero() {
        assert!(is_all_zero(&[0u8; 64]));
        assert!(is_all_zero(&[]));
        let mut buf = vec![0u8; 64];
        buf[17] = 1;
        assert!(!is_all_zero(&buf));
    }

    #[test]
    fn coalesce_merges_adjacent_zeros() {
        let units = coalesce_zero_runs(&[(0, true, 4), (4, true, 4), (8, false, 4), (12, true, 4)]);
        assert_eq!(
            units,
            vec![
                TransferUnit::ZeroRun {
                    offset: 0,
                    length: 8
                },
                TransferUnit::Data {
                    offset: 8,
                    length: 4
                },
                TransferUnit::ZeroRun {
                    offset: 12,
                    length: 4
                },
            ]
        );
    }

    #[test]
    fn rbd_diff_array_and_ndjson() {
        let array = r#"[{"offset":0,"length":4096,"exists":"true"},{"offset":8192,"length":4096,"exists":false}]"#;
        let e = parse_rbd_diff_json(array).unwrap();
        assert_eq!(
            e,
            vec![Extent {
                offset: 0,
                length: 4096
            }]
        );

        let ndjson = "{\"offset\":4096,\"length\":4096,\"exists\":true}\n{\"offset\":8192,\"length\":4096,\"exists\":\"true\"}\n";
        let e = parse_rbd_diff_json(ndjson).unwrap();
        assert_eq!(
            e,
            vec![Extent {
                offset: 4096,
                length: 8192
            }]
        ); // merged adjacent
    }

    #[test]
    fn zstream_dump_write_lines() {
        let dump = "\
HDR type=WRITE offset=0 length=8192
HDR type=FREE offset=100
WRITE object=1 offset=16384 length=4096
";
        let e = parse_zstream_dump_write_extents(dump).unwrap();
        assert_eq!(
            e,
            vec![
                Extent {
                    offset: 0,
                    length: 8192
                },
                Extent {
                    offset: 16384,
                    length: 4096
                },
            ]
        );
    }

    #[test]
    fn thin_delta_xml_sectors() {
        let xml = r#"<superblock>
  <different begin="10" length="2"/>
  <right_only begin="100" length="1"/>
  <same begin="0" length="10"/>
</superblock>"#;
        let e = parse_thin_delta_extents(xml, 512).unwrap();
        assert_eq!(
            e,
            vec![
                Extent {
                    offset: 10 * 512,
                    length: 2 * 512
                },
                Extent {
                    offset: 100 * 512,
                    length: 512
                },
            ]
        );
    }

    #[test]
    fn zfs_snapshot_plan_order() {
        let plan = zfs_snapshot_command_plan("tank0/vol", "s1", true).unwrap();
        assert_eq!(plan[0], ["zfs", "snapshot", "tank0/vol@s1"]);
        assert_eq!(plan[1], ["zfs", "hold", "kcore", "tank0/vol@s1"]);
    }

    #[test]
    fn lvm_thin_snapshot_plan() {
        let plan = lvm_thin_snapshot_command_plan("vg0/data", "snap1").unwrap();
        assert_eq!(plan[0], ["lvcreate", "-s", "-n", "snap1", "vg0/data"]);
    }

    #[test]
    fn reject_bad_snapshot_names() {
        assert!(validate_snapshot_name("").is_err());
        assert!(validate_snapshot_name("has space").is_err());
        assert!(validate_snapshot_name("ok-name_1.2").is_ok());
    }

    #[test]
    fn lease_table_expiry() {
        let table = LeaseTable::new();
        let id = new_lease_id();
        table
            .insert(SnapshotLease {
                lease_id: id.clone(),
                storage_class: StorageClass::Zfs,
                backend_handle: "tank/vol".into(),
                snapshot_name: "s1".into(),
                device_path: "/dev/zvol/tank/vol@s1".into(),
                size_bytes: 1024,
                block_size: 4096,
                expires_at: Instant::now() - Duration::from_secs(1),
            })
            .unwrap();
        let expired = table.expire_due().unwrap();
        assert_eq!(expired.len(), 1);
        assert!(table.get(&id).is_err());
    }

    #[test]
    fn read_write_block_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        std::fs::write(&path, vec![0u8; 64]).unwrap();
        write_block_at(&path, 8, &[1, 2, 3, 4]).unwrap();
        let got = read_block_at(&path, 8, 4).unwrap();
        assert_eq!(got, [1, 2, 3, 4]);
        assert!(is_all_zero(&read_block_at(&path, 0, 8).unwrap()));
    }

    #[test]
    fn storage_class_infer() {
        assert_eq!(
            StorageClass::infer_from_handle("kcore-vms/img"),
            StorageClass::Ceph
        );
        assert_eq!(
            StorageClass::infer_from_handle("/dev/zvol/tank/v"),
            StorageClass::Zfs
        );
        assert_eq!(
            StorageClass::infer_from_handle("/dev/vg0/lv"),
            StorageClass::Lvm
        );
        assert_eq!(
            StorageClass::infer_from_handle("/var/lib/kcore/volumes/x.raw"),
            StorageClass::Filesystem
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 1_000,
            .. ProptestConfig::default()
        })]

        #[test]
        fn merge_is_idempotent(
            offsets in prop::collection::vec(0i64..10_000, 0..20),
            lengths in prop::collection::vec(1i64..500, 0..20),
        ) {
            let n = offsets.len().min(lengths.len());
            let extents: Vec<Extent> = (0..n)
                .map(|i| Extent { offset: offsets[i], length: lengths[i] })
                .collect();
            let once = merge_extents(extents);
            let twice = merge_extents(once.clone());
            prop_assert_eq!(once, twice);
        }

        #[test]
        fn merge_non_overlapping_sorted_and_positive(
            offsets in prop::collection::vec(0i64..10_000, 0..30),
            lengths in prop::collection::vec(1i64..200, 0..30),
        ) {
            let n = offsets.len().min(lengths.len());
            let extents: Vec<Extent> = (0..n)
                .map(|i| Extent { offset: offsets[i], length: lengths[i] })
                .collect();
            let merged = merge_extents(extents);
            for w in merged.windows(2) {
                prop_assert!(w[0].end() <= w[1].offset);
            }
            for e in &merged {
                prop_assert!(e.offset >= 0);
                prop_assert!(e.length > 0);
            }
        }

        #[test]
        fn extents_to_blocks_cover_exactly(
            offset in 0i64..5_000,
            length in 1i64..2_000,
            block_size in 1i64..256,
        ) {
            let extents = [Extent { offset, length }];
            let blocks = extents_to_blocks(&extents, block_size)?;
            let covered: i64 = blocks.iter().map(|b| b.length).sum();
            prop_assert_eq!(covered, length);
            if let Some(first) = blocks.first() {
                prop_assert_eq!(first.offset, offset);
            }
            if let Some(last) = blocks.last() {
                prop_assert_eq!(last.end(), offset + length);
            }
            for b in &blocks {
                prop_assert!(b.length <= block_size);
                prop_assert!(b.length > 0);
            }
        }

        #[test]
        fn is_all_zero_iff_every_byte_zero(buf in prop::collection::vec(any::<u8>(), 0..256)) {
            let expected = buf.iter().all(|&b| b == 0);
            prop_assert_eq!(is_all_zero(&buf), expected);
        }

        #[test]
        fn rbd_diff_roundtrip_extents(
            pairs in prop::collection::vec((0u64..1000, 1u64..100), 0..15),
        ) {
            let mut objs = Vec::new();
            for (off, len) in &pairs {
                objs.push(serde_json::json!({
                    "offset": off,
                    "length": len,
                    "exists": "true",
                }));
            }
            let raw = serde_json::to_string(&objs).unwrap();
            let parsed = parse_rbd_diff_json(&raw)?;
            let expected = merge_extents(
                pairs
                    .into_iter()
                    .map(|(o, l)| Extent { offset: o as i64, length: l as i64 })
                    .collect(),
            );
            prop_assert_eq!(parsed, expected);
        }

        #[test]
        fn snapshot_name_acceptance(
            s in "[A-Za-z0-9._-]{0,140}"
        ) {
            let result = validate_snapshot_name(&s);
            let ok = !s.is_empty() && s.len() <= 128;
            prop_assert_eq!(result.is_ok(), ok);
        }

        #[test]
        fn align_then_merge_stable(
            offset in 0i64..10_000,
            length in 1i64..1_000,
            block in 8i64..256,
        ) {
            let e = [Extent { offset, length }];
            let once = align_extents_to_blocks(&e, block)?;
            let twice = align_extents_to_blocks(&once, block)?;
            prop_assert_eq!(&once, &twice);
            for a in &twice {
                prop_assert_eq!(a.offset % block, 0);
                prop_assert_eq!(a.length % block, 0);
            }
        }
    }
}
