// Proptest coverage for path validation
// Addresses Coverage Gap: Path Validator proptest coverage

use kcore_verification::path_validator::*;
use proptest::prelude::*;
use std::path::Path;

fn safe_path_segment() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,32}"
}

proptest! {
    #[test]
    fn test_safe_segments_accepted(segment in safe_path_segment()) {
        let result = validate_path_segment(&segment);
        prop_assert!(result.is_ok(), "Safe segment rejected: {}", segment);
    }

    #[test]
    fn test_segment_validation_deterministic(segment in safe_path_segment()) {
        let result1 = validate_path_segment(&segment);
        let result2 = validate_path_segment(&segment);
        prop_assert_eq!(result1.is_ok(), result2.is_ok(), "Non-deterministic validation");
    }

    #[test]
    fn test_segments_with_dots(segment in "[a-zA-Z0-9]{1,10}\\.[a-zA-Z0-9]{1,10}") {
        // Segments with dots in the middle (like file.txt) are valid
        let result = validate_path_segment(&segment);
        prop_assert!(result.is_ok(), "Segment with dot rejected: {}", segment);
    }

    #[test]
    fn test_segment_length_bounds(length in 1usize..256usize) {
        let segment: String = "a".repeat(length);
        let result = validate_path_segment(&segment);
        prop_assert!(result.is_ok(), "Length {} rejected", length);
    }

    #[test]
    fn test_alphanumeric_segments(segment in "[a-zA-Z0-9]{1,32}") {
        let result = validate_path_segment(&segment);
        prop_assert!(result.is_ok(), "Alphanumeric segment rejected: {}", segment);
    }

    #[test]
    fn test_segments_with_hyphens(segment in "[a-zA-Z0-9-]{1,32}") {
        let result = validate_path_segment(&segment);
        prop_assert!(result.is_ok(), "Segment with hyphens rejected: {}", segment);
    }

    #[test]
    fn test_segments_with_underscores(segment in "[a-zA-Z0-9_]{1,32}") {
        let result = validate_path_segment(&segment);
        prop_assert!(result.is_ok(), "Segment with underscores rejected: {}", segment);
    }

    #[test]
    fn test_path_under_root_rejects_absolute(
        segments in prop::collection::vec(safe_path_segment(), 1..5)
    ) {
        let temp = std::env::temp_dir();
        let path_str = format!("/{}", segments.join("/"));
        let path = Path::new(&path_str);

        let result = validate_path_under_root(&temp, path);
        prop_assert!(result.is_err(), "Absolute path not rejected: {}", path_str);
    }

    #[test]
    fn test_path_under_root_accepts_relative(
        segments in prop::collection::vec(safe_path_segment(), 1..5)
    ) {
        let temp = std::env::temp_dir();
        let path_str = segments.join("/");
        let path = Path::new(&path_str);

        let result = validate_path_under_root(&temp, path);
        prop_assert!(result.is_ok(), "Relative path rejected: {}", path_str);
    }

    #[test]
    fn test_deep_paths(
        segments in prop::collection::vec(safe_path_segment(), 1..20)
    ) {
        let temp = std::env::temp_dir();
        let path_str = segments.join("/");
        let path = Path::new(&path_str);

        let result = validate_path_under_root(&temp, path);
        prop_assert!(result.is_ok(), "Deep path rejected: {} segments", segments.len());
    }

    #[test]
    fn test_single_segment_path(segment in safe_path_segment()) {
        let temp = std::env::temp_dir();
        let path = Path::new(&segment);

        let result = validate_path_under_root(&temp, path);
        prop_assert!(result.is_ok(), "Single segment path rejected: {}", segment);
    }
}
