//! Nix expression escaping and attribute-key sanitisation.
//!
//! Generated Nix runs as root during `nixos-rebuild switch`. These functions
//! are the injection boundary for tenant-supplied strings.

/// Escape `input` so it can be placed inside a Nix double-quoted string.
///
/// Escapes `\`, `"`, newlines, carriage returns, tabs, and the `${`
/// interpolation introducer. Other bytes, including non-ASCII UTF-8, are
/// copied unchanged.
pub fn nix_escape(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len().saturating_mul(2));
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => out.extend_from_slice(br"\\"),
            b'"' => out.extend_from_slice(br#"\""#),
            b'\n' => out.extend_from_slice(br"\n"),
            b'\r' => out.extend_from_slice(br"\r"),
            b'\t' => out.extend_from_slice(br"\t"),
            b'$' if index + 1 < bytes.len() && bytes[index + 1] == b'{' => {
                out.extend_from_slice(br"\${");
                index += 1;
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// True when `escaped` can sit inside a Nix double-quoted string without
/// ending the literal or opening an interpolation.
pub fn nix_escape_is_safe(escaped: &str) -> bool {
    let bytes = escaped.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if index + 1 >= bytes.len() {
                return false;
            }
            match bytes[index + 1] {
                b'\\' | b'"' | b'n' | b'r' | b't' | b'$' => {
                    index += 2;
                    continue;
                }
                _ => return false,
            }
        }
        if bytes[index] == b'"'
            || bytes[index] == b'\n'
            || bytes[index] == b'\r'
            || bytes[index] == b'\t'
        {
            return false;
        }
        if bytes[index] == b'$' && index + 1 < bytes.len() && bytes[index + 1] == b'{' {
            return false;
        }
        index += 1;
    }
    true
}

/// Map an arbitrary string onto a Nix attribute key.
///
/// Every resulting character is ASCII alphanumeric, `-`, or `_`. The mapping
/// is one character to one character, so the length in Unicode scalars is
/// preserved.
pub fn sanitize_nix_attr_key(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// True when every character is a legal Nix attribute-key character.
pub fn nix_attr_key_is_safe(key: &str) -> bool {
    key.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quote_backslash_and_interpolation() {
        assert_eq!(nix_escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(nix_escape(r"a\b"), r"a\\b");
        assert_eq!(nix_escape("cost is ${price}"), r"cost is \${price}");
        assert_eq!(nix_escape("line\n"), r"line\n");
    }

    #[test]
    fn dollar_without_brace_is_literal() {
        assert_eq!(nix_escape("price is $5"), "price is $5");
        assert!(nix_escape_is_safe(&nix_escape("price is $5")));
    }

    #[test]
    fn escaped_output_is_safe() {
        let samples = ["", "plain", "\"", "\\", "${", "\n\r\t", "a${b\"c\\d"];
        for sample in samples {
            assert!(nix_escape_is_safe(&nix_escape(sample)), "{sample:?}");
        }
    }

    #[test]
    fn sanitize_replaces_unsafe_and_preserves_length() {
        let key = sanitize_nix_attr_key("vm name/1");
        assert_eq!(key, "vm_name_1");
        assert!(nix_attr_key_is_safe(&key));
        assert_eq!(key.chars().count(), "vm name/1".chars().count());
    }

    #[test]
    fn sanitize_keeps_unicode_length() {
        let input = "café";
        let key = sanitize_nix_attr_key(input);
        assert_eq!(key.chars().count(), input.chars().count());
        assert!(nix_attr_key_is_safe(&key));
        assert_eq!(key, "caf_");
    }
}
