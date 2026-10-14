use kcore_verification::nix_sanitize::*;
use proptest::prelude::*;

proptest! {
    #[test]
    fn escaped_nix_is_safe(input in "\\PC{0,64}") {
        let escaped = nix_escape(&input);
        prop_assert!(nix_escape_is_safe(&escaped));
    }

    #[test]
    fn escape_is_deterministic(input in "\\PC{0,48}") {
        prop_assert_eq!(nix_escape(&input), nix_escape(&input));
    }

    #[test]
    fn interpolation_is_escaped(prefix in "[a-z]{0,8}", suffix in "[a-z]{0,8}") {
        let input = format!("{prefix}${{{suffix}}}");
        let escaped = nix_escape(&input);
        let hides_interpolation = escaped.contains("\\${");
        prop_assert!(hides_interpolation);
        prop_assert!(nix_escape_is_safe(&escaped));
    }

    #[test]
    fn quotes_and_backslashes_are_escaped(body in "[a-zA-Z0-9 ]{0,16}") {
        let input = format!("\"{body}\\");
        let escaped = nix_escape(&input);
        let ends_cleanly = !escaped.ends_with('\\') || escaped.ends_with("\\\\");
        prop_assert!(nix_escape_is_safe(&escaped));
        prop_assert!(ends_cleanly);
    }

    #[test]
    fn attr_key_is_safe_and_same_length(input in "\\PC{0,32}") {
        let key = sanitize_nix_attr_key(&input);
        prop_assert!(nix_attr_key_is_safe(&key));
        prop_assert_eq!(key.chars().count(), input.chars().count());
    }

    #[test]
    fn already_safe_keys_are_unchanged(input in "[A-Za-z0-9_-]{0,24}") {
        prop_assert_eq!(sanitize_nix_attr_key(&input), input);
    }

    #[test]
    fn sanitize_is_deterministic(input in "\\PC{0,24}") {
        prop_assert_eq!(sanitize_nix_attr_key(&input), sanitize_nix_attr_key(&input));
    }
}
