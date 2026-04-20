//! Guards for path strings before filesystem access (kctl side).
//!
//! Implementation + proofs live in the leaf `kcore-sanitize` crate.

use anyhow::Result;
use kcore_sanitize::SafePathError;

#[cfg(test)]
use kcore_sanitize::path_segments_include_dot_dot;

pub fn assert_safe_path(path: &str, label: &str) -> Result<()> {
    kcore_sanitize::assert_safe_path(path).map_err(|e| match e {
        SafePathError::Empty => anyhow::anyhow!("{label} must not be empty"),
        SafePathError::ContainsNul => anyhow::anyhow!("{label} must not contain NUL bytes"),
        SafePathError::ContainsParentDir => {
            anyhow::anyhow!("{label} must not contain parent directory references ('..')")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_dot_dot_in_unix_paths() {
        assert!(path_segments_include_dot_dot("../etc/passwd"));
        assert!(path_segments_include_dot_dot("foo/../bar"));
        assert!(path_segments_include_dot_dot("/abs/../sneaky"));
        assert!(path_segments_include_dot_dot(".."));
    }

    #[test]
    fn detects_dot_dot_in_windows_style_paths() {
        assert!(path_segments_include_dot_dot("foo\\..\\bar"));
        assert!(path_segments_include_dot_dot("..\\windows"));
    }

    #[test]
    fn allows_clean_paths() {
        assert!(!path_segments_include_dot_dot(""));
        assert!(!path_segments_include_dot_dot("foo/bar"));
        assert!(
            !path_segments_include_dot_dot("..foo"),
            "embedded .. is not a segment"
        );
        assert!(!path_segments_include_dot_dot("foo..bar"));
        assert!(!path_segments_include_dot_dot("..."));
    }

    #[test]
    fn assert_safe_path_rejects_empty() {
        let err = assert_safe_path("", "p").expect_err("empty must fail");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn assert_safe_path_rejects_nul() {
        let err = assert_safe_path("foo\0bar", "p").expect_err("NUL must fail");
        assert!(err.to_string().contains("NUL"));
    }

    #[test]
    fn assert_safe_path_rejects_traversal_with_both_separators() {
        assert!(assert_safe_path("a/../b", "p").is_err());
        assert!(assert_safe_path("a\\..\\b", "p").is_err());
    }

    #[test]
    fn assert_safe_path_accepts_clean_paths() {
        assert!(assert_safe_path("foo/bar/baz.txt", "p").is_ok());
        assert!(assert_safe_path("/absolute/path", "p").is_ok());
        assert!(assert_safe_path("file..with..dots", "p").is_ok());
    }
}
