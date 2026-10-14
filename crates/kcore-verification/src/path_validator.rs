use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum PathValidationError {
    #[error("Path traversal detected: {0}")]
    PathTraversal(String),
    #[error("Invalid path component: {0}")]
    InvalidComponent(String),
    #[error("Path is not under root: {0}")]
    NotUnderRoot(String),
}

pub fn validate_path_segment(segment: &str) -> Result<(), PathValidationError> {
    if segment.is_empty() {
        return Err(PathValidationError::InvalidComponent(
            "empty segment".to_string(),
        ));
    }

    if segment == "." || segment == ".." {
        return Err(PathValidationError::PathTraversal(segment.to_string()));
    }

    if segment.contains('\0') {
        return Err(PathValidationError::InvalidComponent(
            "null byte in segment".to_string(),
        ));
    }

    if segment.starts_with('/') || segment.ends_with('/') {
        return Err(PathValidationError::InvalidComponent(
            "segment contains slash".to_string(),
        ));
    }

    Ok(())
}

pub fn validate_path_under_root(root: &Path, path: &Path) -> Result<PathBuf, PathValidationError> {
    let canonical_root = root
        .canonicalize()
        .map_err(|e| PathValidationError::InvalidComponent(format!("invalid root: {}", e)))?;

    let mut constructed = canonical_root.clone();

    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => {
                let segment_str = segment.to_str().ok_or_else(|| {
                    PathValidationError::InvalidComponent("non-UTF8 segment".to_string())
                })?;

                validate_path_segment(segment_str)?;
                constructed.push(segment);
            }
            std::path::Component::ParentDir => {
                return Err(PathValidationError::PathTraversal("..".to_string()));
            }
            std::path::Component::RootDir => {
                return Err(PathValidationError::InvalidComponent(
                    "absolute path not allowed".to_string(),
                ));
            }
            std::path::Component::CurDir => {
                continue;
            }
            std::path::Component::Prefix(_) => {
                return Err(PathValidationError::InvalidComponent(
                    "path prefix not allowed".to_string(),
                ));
            }
        }
    }

    if !constructed.starts_with(&canonical_root) {
        return Err(PathValidationError::NotUnderRoot(
            constructed.display().to_string(),
        ));
    }

    Ok(constructed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_segment() {
        assert!(validate_path_segment("valid").is_ok());
        assert!(validate_path_segment("valid-file.txt").is_ok());
        assert!(validate_path_segment("123").is_ok());
    }

    #[test]
    fn test_invalid_segments() {
        assert_eq!(
            validate_path_segment(".."),
            Err(PathValidationError::PathTraversal("..".to_string()))
        );
        assert_eq!(
            validate_path_segment("."),
            Err(PathValidationError::PathTraversal(".".to_string()))
        );
        assert!(validate_path_segment("").is_err());
        assert!(validate_path_segment("file\0name").is_err());
        assert!(validate_path_segment("/file").is_err());
        assert!(validate_path_segment("file/").is_err());
    }

    #[test]
    fn test_path_under_root() {
        let temp = std::env::temp_dir();
        let result = validate_path_under_root(&temp, Path::new("valid/subdir/file.txt"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_path_traversal_rejection() {
        let temp = std::env::temp_dir();
        let result = validate_path_under_root(&temp, Path::new("../etc/passwd"));
        assert!(result.is_err());
    }

    #[test]
    fn current_dir_component_is_skipped() {
        let temp = std::env::temp_dir();
        let result = validate_path_under_root(&temp, Path::new("foo/./bar")).unwrap();
        assert!(result.ends_with("foo/bar"));
        assert!(!result.ends_with("./bar"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_segment_is_rejected() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp = std::env::temp_dir();
        let path = OsString::from_vec(b"bad/\xff/file".to_vec());
        let err = validate_path_under_root(&temp, Path::new(&path)).unwrap_err();
        assert!(matches!(err, PathValidationError::InvalidComponent(_)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefix_is_rejected() {
        let temp = std::env::temp_dir();
        let err = validate_path_under_root(&temp, Path::new(r"C:\Windows")).unwrap_err();
        assert!(matches!(err, PathValidationError::InvalidComponent(_)));
    }
}
