//! Certificate lifecycle: inventory metadata, CRL generation, OCSP responder,
//! revocation enforcement and the plain-HTTP distribution endpoints.
//!
//! See `docs/mtls-bootstrap-and-auth.md` §4 for the operator-facing model.

pub mod crl;
pub mod http;
pub mod inventory;
pub mod ocsp;
pub mod revocation;

use time::OffsetDateTime;

/// Sentinel stored in `issued_certificates.revocation_reason` for rows that
/// are not revoked. RFC 5280 reason codes are all >= 0.
pub const REASON_NONE: i32 = -1;

/// Format an instant the way the `issued_certificates` and `crl_state` tables
/// store timestamps: RFC3339, UTC, second precision, `Z` suffix.
///
/// Fixed width means SQL string comparison is also chronological comparison,
/// which is what the inventory queries rely on.
pub fn format_ts(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Inverse of [`format_ts`]. Returns `None` for anything that is not the exact
/// `YYYY-MM-DDTHH:MM:SSZ` shape, so a malformed row can never be mistaken for
/// a valid instant.
pub fn parse_ts(s: &str) -> Option<OffsetDateTime> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| s.get(from..to)?.parse::<i32>().ok();
    let year = num(0, 4)?;
    let month = u8::try_from(num(5, 7)?).ok()?;
    let day = u8::try_from(num(8, 10)?).ok()?;
    let hour = u8::try_from(num(11, 13)?).ok()?;
    let minute = u8::try_from(num(14, 16)?).ok()?;
    let second = u8::try_from(num(17, 19)?).ok()?;
    let month = time::Month::try_from(month).ok()?;
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let time_of_day = time::Time::from_hms(hour, minute, second).ok()?;
    Some(date.with_time(time_of_day).assume_utc())
}

/// Whole days from `now` until `not_after`; negative once expired.
pub fn days_until(not_after: OffsetDateTime, now: OffsetDateTime) -> i32 {
    (not_after - now).whole_days() as i32
}

/// Uppercase hex without separators, matching `openssl x509 -serial` output.
pub fn hex_upper(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(
            char::from_digit((b >> 4) as u32, 16)
                .unwrap_or('0')
                .to_ascii_uppercase(),
        );
        s.push(
            char::from_digit((b & 0x0f) as u32, 16)
                .unwrap_or('0')
                .to_ascii_uppercase(),
        );
    }
    s
}

/// Lowercase hex without separators, matching `openssl dgst -sha256` output.
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    s
}

/// Normalise an operator-supplied serial: strip `0x`, colons and whitespace,
/// then uppercase. Leading zero bytes are preserved because CRL entries and
/// peer certificates are compared on the exact DER integer bytes.
pub fn normalize_serial(input: &str) -> String {
    input
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

/// SHA-256 over `data`, via the same aws-lc-rs backend the TLS stack uses.
pub fn sha256(data: &[u8]) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data)
        .as_ref()
        .to_vec()
}

/// SHA-1 over `data`. Used **only** for the RFC 6960 `CertID`
/// `issuerNameHash` / `issuerKeyHash` identifiers and the `ResponderID`
/// `KeyHash`, which the protocol pins to SHA-1. No signature or integrity
/// decision depends on it.
pub fn sha1_identifier(data: &[u8]) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, data)
        .as_ref()
        .to_vec()
}

/// Map an RFC 5280 reason code to the rcgen enum. Unknown or sentinel codes
/// become `Unspecified` so a bad row still produces a well-formed CRL entry.
pub fn rcgen_reason(code: i32) -> rcgen::RevocationReason {
    match code {
        1 => rcgen::RevocationReason::KeyCompromise,
        2 => rcgen::RevocationReason::CaCompromise,
        3 => rcgen::RevocationReason::AffiliationChanged,
        4 => rcgen::RevocationReason::Superseded,
        5 => rcgen::RevocationReason::CessationOfOperation,
        6 => rcgen::RevocationReason::CertificateHold,
        8 => rcgen::RevocationReason::RemoveFromCrl,
        9 => rcgen::RevocationReason::PrivilegeWithdrawn,
        10 => rcgen::RevocationReason::AaCompromise,
        _ => rcgen::RevocationReason::Unspecified,
    }
}

/// Accept only reason codes RFC 5280 §5.3.1 defines. 7 is deliberately unused
/// by the RFC.
pub fn is_valid_reason_code(code: i32) -> bool {
    matches!(code, 0..=6 | 8..=10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn format_and_parse_ts_round_trip() {
        let now = OffsetDateTime::now_utc().replace_nanosecond(0).expect("ns");
        let s = format_ts(now);
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert_eq!(parse_ts(&s), Some(now));
    }

    #[test]
    fn format_ts_is_lexicographically_ordered() {
        let a = OffsetDateTime::now_utc();
        let b = a + Duration::days(40);
        assert!(format_ts(a) < format_ts(b));
    }

    #[test]
    fn parse_ts_rejects_malformed_input() {
        assert_eq!(parse_ts(""), None);
        assert_eq!(parse_ts("2026-01-01"), None);
        assert_eq!(parse_ts("2026-01-01T00:00:00"), None);
        assert_eq!(parse_ts("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_ts("2026-01-01 00:00:00Z"), None);
        assert_eq!(parse_ts("XXXX-01-01T00:00:00Z"), None);
    }

    #[test]
    fn hex_helpers_use_expected_case() {
        assert_eq!(hex_upper(&[0x0a, 0xff, 0x00]), "0AFF00");
        assert_eq!(hex_lower(&[0x0a, 0xff, 0x00]), "0aff00");
    }

    #[test]
    fn normalize_serial_strips_decoration() {
        assert_eq!(normalize_serial(" 0x0a:ff:00 "), "0AFF00");
        assert_eq!(normalize_serial("0aff00"), "0AFF00");
        assert_eq!(normalize_serial("0A FF 00"), "0AFF00");
    }

    #[test]
    fn reason_code_validation_matches_rfc5280() {
        for code in [0, 1, 2, 3, 4, 5, 6, 8, 9, 10] {
            assert!(is_valid_reason_code(code), "{code} should be valid");
        }
        for code in [-1, 7, 11, 255] {
            assert!(!is_valid_reason_code(code), "{code} should be invalid");
        }
    }

    #[test]
    fn days_until_is_signed() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(days_until(now + Duration::days(10), now), 10);
        assert_eq!(days_until(now - Duration::days(3), now), -3);
    }

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("abc")
        assert_eq!(
            hex_lower(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha1_identifier_matches_known_vector() {
        // SHA-1("abc")
        assert_eq!(
            hex_lower(&sha1_identifier(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
}
