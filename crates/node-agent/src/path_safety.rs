//! Path-traversal sanitization helpers for tenant-supplied path
//! segments (node-agent side).
//!
//! Implementation + Kani proofs live in the leaf `kcore-sanitize`
//! crate. This module wraps those validators so the existing
//! `(input, label) -> Result<_, String>` contract used by RPC
//! handlers stays unchanged — error messages keep mentioning the
//! field that failed validation.

use kcore_sanitize::{SafeRootedPathError, SafeSegmentError};

#[cfg(test)]
use kcore_sanitize::MAX_SAFE_SEGMENT_LEN;

/// Validate a single path *segment* (no slashes, no `..`, no NULs,
/// no leading `-`). Returns the trimmed segment on success.
pub fn validate_safe_segment<'a>(name: &'a str, label: &str) -> Result<&'a str, String> {
    kcore_sanitize::validate_safe_segment(name).map_err(|e| match e {
        SafeSegmentError::Empty => format!("{label} is required"),
        SafeSegmentError::TooLong { actual, max } => {
            format!("{label} is too long ({actual} bytes, max {max})")
        }
        SafeSegmentError::ContainsNul => format!("{label} must not contain NUL bytes"),
        SafeSegmentError::ContainsSeparator => format!("{label} must not contain path separators"),
        SafeSegmentError::DotOrDotDot => format!("{label} must not be '.' or '..'"),
        SafeSegmentError::LeadingDash => format!("{label} must not start with '-'"),
    })
}

/// Validate that an absolute path (provided by an RPC caller)
/// stays under `root`.
pub fn validate_path_under_root(
    raw: &str,
    root: &std::path::Path,
    label: &str,
) -> Result<std::path::PathBuf, String> {
    kcore_sanitize::validate_path_under_root(raw, root).map_err(|e| match e {
        SafeRootedPathError::Empty => format!("{label} is required"),
        SafeRootedPathError::ContainsNul => format!("{label} must not contain NUL bytes"),
        SafeRootedPathError::NotAbsolute => format!("{label} must be an absolute path"),
        SafeRootedPathError::NotUnderRoot => {
            format!("{label} must be under {}", root.display())
        }
        SafeRootedPathError::ContainsParentDir => format!("{label} must not contain '..' segments"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn segment_rejects_empty_and_dots() {
        assert!(validate_safe_segment("", "name").is_err());
        assert!(validate_safe_segment("   ", "name").is_err());
        assert!(validate_safe_segment(".", "name").is_err());
        assert!(validate_safe_segment("..", "name").is_err());
    }

    #[test]
    fn segment_rejects_separators() {
        assert!(validate_safe_segment("foo/bar", "name").is_err());
        assert!(validate_safe_segment("foo\\bar", "name").is_err());
        assert!(validate_safe_segment("/abs", "name").is_err());
    }

    #[test]
    fn segment_rejects_nul() {
        assert!(validate_safe_segment("foo\0bar", "name").is_err());
    }

    #[test]
    fn segment_rejects_leading_dash() {
        assert!(validate_safe_segment("-foo", "name").is_err());
    }

    #[test]
    fn segment_accepts_normal_names() {
        assert_eq!(
            validate_safe_segment("debian-12.qcow2", "image").unwrap(),
            "debian-12.qcow2"
        );
        assert_eq!(
            validate_safe_segment("  web-01  ", "name").unwrap(),
            "web-01"
        );
    }

    #[test]
    fn segment_rejects_overlong() {
        let huge = "a".repeat(MAX_SAFE_SEGMENT_LEN + 1);
        assert!(validate_safe_segment(&huge, "name").is_err());
    }

    #[test]
    fn under_root_rejects_relative_or_outside() {
        let root = Path::new("/var/lib/kcore/images");
        assert!(validate_path_under_root("relative.raw", root, "p").is_err());
        assert!(validate_path_under_root("/etc/passwd", root, "p").is_err());
        assert!(validate_path_under_root("", root, "p").is_err());
        assert!(validate_path_under_root("\0", root, "p").is_err());
    }

    #[test]
    fn under_root_rejects_dotdot_traversal_even_when_starts_with_root() {
        // CRITICAL: bare `starts_with` would PASS this check because
        // the lexical prefix is /var/lib/kcore/images. The fix is to
        // walk components and forbid ParentDir.
        let root = Path::new("/var/lib/kcore/images");
        let bad = "/var/lib/kcore/images/../../../etc/passwd";
        let err = validate_path_under_root(bad, root, "image_path")
            .expect_err("must reject .. traversal");
        assert!(err.contains(".."), "should mention dot-dot, got: {err}");
    }

    #[test]
    fn under_root_accepts_clean_path() {
        let root = Path::new("/var/lib/kcore/images");
        let p = validate_path_under_root("/var/lib/kcore/images/debian.qcow2", root, "p")
            .expect("clean path");
        assert_eq!(p, Path::new("/var/lib/kcore/images/debian.qcow2"));
    }
}
