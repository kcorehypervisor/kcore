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

/// Property-based tests (Phase 1).
///
/// These guards are the security boundary for every node-agent operation
/// that takes a caller-supplied path or name, so the strongest guarantees
/// we want are: never panic on any input, and **soundness** of the
/// traversal check (any output we accept has no `..` component on it).
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 2_000,
            .. ProptestConfig::default()
        })]

        /// `validate_safe_segment` must never panic on arbitrary input.
        #[test]
        fn validate_safe_segment_never_panics(s in ".{0,256}") {
            let _ = validate_safe_segment(&s, "f");
        }

        /// `validate_path_under_root` must never panic on arbitrary input,
        /// for any root (we sample a small handful of realistic roots).
        #[test]
        fn validate_path_under_root_never_panics(
            s in ".{0,256}",
            root in prop::sample::select(vec![
                "/var/lib/kcore/images",
                "/var/lib/kcore/volumes/filesystem",
                "/srv/kcore",
            ]),
        ) {
            let _ = validate_path_under_root(&s, std::path::Path::new(root), "f");
        }

        /// **Soundness — segment side**: if `validate_safe_segment` returns
        /// `Ok`, the returned segment is guaranteed to (a) be non-empty,
        /// (b) contain no separators, no NUL, and no `..`, and (c) joining
        /// it under any clean absolute root produces a path whose
        /// components contain no `ParentDir`. This is the property that
        /// guarantees `prepare_storage_mount` cannot escape the volumes
        /// root no matter what the controller forwards.
        #[test]
        fn safe_segment_joined_under_root_never_escapes(
            s in ".{0,128}",
            root in prop::sample::select(vec![
                "/var/lib/kcore/images",
                "/var/lib/kcore/volumes/filesystem",
            ]),
        ) {
            if let Ok(seg) = validate_safe_segment(&s, "name") {
                prop_assert!(!seg.is_empty());
                prop_assert!(!seg.contains('/'));
                prop_assert!(!seg.contains('\\'));
                prop_assert!(!seg.contains('\0'));
                prop_assert!(seg != "." && seg != "..");

                let joined = std::path::Path::new(root).join(seg);
                let has_parent = joined
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir));
                prop_assert!(
                    !has_parent,
                    "joined path {} contains '..' after accepting segment {seg:?}",
                    joined.display()
                );
            }
        }

        /// **Soundness — under-root side**: if `validate_path_under_root`
        /// returns `Ok`, the path is absolute, lexically under the given
        /// root, and contains no `ParentDir` component. This is the
        /// property that protects `delete_image` and `delete_volume`.
        #[test]
        fn path_under_root_acceptance_implies_no_parent_dir(
            s in ".{0,128}",
            root in prop::sample::select(vec![
                "/var/lib/kcore/images",
                "/var/lib/kcore/volumes/filesystem",
            ]),
        ) {
            let root = std::path::Path::new(root);
            if let Ok(p) = validate_path_under_root(&s, root, "p") {
                prop_assert!(p.is_absolute());
                prop_assert!(p.starts_with(root));
                let has_parent = p
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir));
                prop_assert!(
                    !has_parent,
                    "accepted path {} contains '..'",
                    p.display()
                );
            }
        }

        /// Length bound: any string longer than `MAX_SAFE_SEGMENT_LEN`
        /// must always be rejected, regardless of content.
        #[test]
        fn segment_length_cap_is_enforced(
            extra in 1usize..=64,
            ch in prop::sample::select(vec!['a', '0', '_', '-', '.']),
        ) {
            // Avoid leading dash (separately rejected) by prefixing with
            // a safe character.
            let mut s = String::from("x");
            s.extend(std::iter::repeat_n(ch, MAX_SAFE_SEGMENT_LEN + extra));
            let result = validate_safe_segment(&s, "name");
            prop_assert!(
                result.is_err(),
                "overlong input ({} bytes) was accepted",
                s.len()
            );
        }
    }
}
