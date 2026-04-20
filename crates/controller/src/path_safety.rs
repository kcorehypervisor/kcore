//! Guards for path strings before filesystem access.
//!
//! The actual validators live (and are Kani-verified) in the leaf
//! `kcore-sanitize` crate. This module is a thin wrapper that
//! preserves the existing `(path, label) -> anyhow::Result<()>`
//! contract used by the rest of the controller, so that error
//! messages keep mentioning the field that failed validation.

use anyhow::Result;
use kcore_sanitize::SafePathError;

pub fn assert_safe_path(path: &str, label: &str) -> Result<()> {
    kcore_sanitize::assert_safe_path(path).map_err(|e| match e {
        SafePathError::Empty => anyhow::anyhow!("{label} must not be empty"),
        SafePathError::ContainsNul => anyhow::anyhow!("{label} must not contain NUL bytes"),
        SafePathError::ContainsParentDir => {
            anyhow::anyhow!("{label} must not contain parent directory references ('..')")
        }
    })
}
