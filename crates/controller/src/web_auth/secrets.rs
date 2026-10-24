//! Password hashes, TOTP (RFC 6238), bearer tokens, and recovery codes.
//! Everything goes through aws-lc-rs, the controller's crypto library.

use std::num::NonZeroU32;

use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use aws_lc_rs::{constant_time, digest, hmac, pbkdf2};
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine as _;

/// OWASP 2023 floor for PBKDF2-HMAC-SHA256.
pub const PASSWORD_ITERATIONS: u32 = 600_000;
pub const MIN_PASSWORD_LEN: usize = 12;
pub const MAX_PASSWORD_LEN: usize = 1024;
const SALT_LEN: usize = 16;
const HASH_LEN: usize = 32;
const SCHEME: &str = "pbkdf2-sha256";

pub const TOTP_DIGITS: u32 = 6;
pub const TOTP_PERIOD_SECS: i64 = 30;
const TOTP_SECRET_LEN: usize = 20;
const TOTP_SKEW_STEPS: i64 = 1;

pub const RECOVERY_CODE_COUNT: usize = 10;

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    SystemRandom::new()
        .fill(&mut buf)
        .expect("system random source");
    buf
}

pub fn check_password_policy(password: &str) -> Result<(), String> {
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_LEN {
        return Err(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(format!("password must be at most {MAX_PASSWORD_LEN} bytes"));
    }
    if password.trim() != password {
        return Err("password must not start or end with whitespace".into());
    }
    Ok(())
}

pub fn hash_password(password: &str) -> String {
    hash_password_with(password, PASSWORD_ITERATIONS)
}

pub fn hash_password_with(password: &str, iterations: u32) -> String {
    let iterations = NonZeroU32::new(iterations.max(1)).expect("non-zero");
    let salt: [u8; SALT_LEN] = random_bytes();
    let mut out = [0u8; HASH_LEN];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        &salt,
        password.as_bytes(),
        &mut out,
    );
    format!(
        "{SCHEME}${}${}${}",
        iterations,
        STANDARD_NO_PAD.encode(salt),
        STANDARD_NO_PAD.encode(out)
    )
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some(SCHEME), Some(iter), Some(salt), Some(hash), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let Some(iterations) = iter.parse::<u32>().ok().and_then(NonZeroU32::new) else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (STANDARD_NO_PAD.decode(salt), STANDARD_NO_PAD.decode(hash)) else {
        return false;
    };
    pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        &salt,
        password.as_bytes(),
        &hash,
    )
    .is_ok()
}

/// Spend the same work as a real check so an unknown user is not faster.
pub fn burn_password_check(password: &str) {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let stored = DUMMY.get_or_init(|| hash_password("kcore-dummy-password-for-timing"));
    let _ = verify_password(password, stored);
}

pub fn sha256_hex(data: &[u8]) -> String {
    digest::digest(&digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A bearer token and the hash stored for it. The prefix tells a reader
/// what kind of token leaked: `kcs_` session, `kcp_` pending, `kct_` API.
pub fn new_token(prefix: &str) -> (String, String) {
    let raw: [u8; 32] = random_bytes();
    let token = format!("{prefix}{}", URL_SAFE_NO_PAD.encode(raw));
    let hash = sha256_hex(token.as_bytes());
    (token, hash)
}

pub fn new_token_id() -> String {
    let raw: [u8; 9] = random_bytes();
    URL_SAFE_NO_PAD.encode(raw)
}

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for ch in text.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let upper = ch.to_ascii_uppercase() as u8;
        let value = BASE32.iter().position(|&c| c == upper)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

pub fn new_totp_secret() -> String {
    let raw: [u8; TOTP_SECRET_LEN] = random_bytes();
    base32_encode(&raw)
}

pub fn totp_code(secret: &[u8], step: i64) -> u32 {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let tag = hmac::sign(&key, &step.to_be_bytes());
    let mac = tag.as_ref();
    let offset = (mac[mac.len() - 1] & 0x0f) as usize;
    let binary = (u32::from(mac[offset] & 0x7f) << 24)
        | (u32::from(mac[offset + 1]) << 16)
        | (u32::from(mac[offset + 2]) << 8)
        | u32::from(mac[offset + 3]);
    binary % 10u32.pow(TOTP_DIGITS)
}

/// The time step a code matches, within one step of `now`. The caller must
/// still claim the step so the same code cannot be used twice.
pub fn verify_totp(secret_b32: &str, code: &str, now_unix: i64) -> Option<i64> {
    let code = code.trim().replace(' ', "");
    if code.len() != TOTP_DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secret = base32_decode(secret_b32)?;
    if secret.is_empty() {
        return None;
    }
    let current = now_unix.div_euclid(TOTP_PERIOD_SECS);
    let mut found = None;
    for step in current - TOTP_SKEW_STEPS..=current + TOTP_SKEW_STEPS {
        let expected = format!(
            "{:0width$}",
            totp_code(&secret, step),
            width = TOTP_DIGITS as usize
        );
        if constant_time::verify_slices_are_equal(expected.as_bytes(), code.as_bytes()).is_ok() {
            found = Some(step);
        }
    }
    found
}

pub fn otpauth_uri(issuer: &str, account: &str, secret: &str) -> String {
    let issuer_q = url::form_urlencoded::byte_serialize(issuer.as_bytes()).collect::<String>();
    let label = format!("{issuer}:{account}");
    let label_q: String = url::form_urlencoded::byte_serialize(label.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!(
        "otpauth://totp/{label_q}?secret={secret}&issuer={issuer_q}&algorithm=SHA1&digits={TOTP_DIGITS}&period={TOTP_PERIOD_SECS}"
    )
}

const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

fn normalize_recovery_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Ten codes shaped `xxxxx-xxxxx`, and the hashes to store for them.
pub fn new_recovery_codes() -> (Vec<String>, Vec<String>) {
    let mut codes = Vec::with_capacity(RECOVERY_CODE_COUNT);
    let mut hashes = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let raw: [u8; 10] = random_bytes();
        let chars: String = raw
            .iter()
            .map(|b| RECOVERY_ALPHABET[usize::from(*b) % RECOVERY_ALPHABET.len()] as char)
            .collect();
        let code = format!("{}-{}", &chars[..5], &chars[5..]);
        hashes.push(sha256_hex(normalize_recovery_code(&code).as_bytes()));
        codes.push(code);
    }
    (codes, hashes)
}

/// Index of the stored hash that `code` matches.
pub fn match_recovery_code(code: &str, hashes: &[String]) -> Option<usize> {
    let normalized = normalize_recovery_code(code);
    if normalized.len() != 10 {
        return None;
    }
    let candidate = sha256_hex(normalized.as_bytes());
    hashes.iter().position(|stored| {
        constant_time::verify_slices_are_equal(stored.as_bytes(), candidate.as_bytes()).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let stored = hash_password_with("correct horse battery", 1000);
        assert!(stored.starts_with("pbkdf2-sha256$1000$"));
        assert!(verify_password("correct horse battery", &stored));
        assert!(!verify_password("correct horse batterx", &stored));
        assert!(!verify_password("x", "garbage"));
        assert!(!verify_password("x", ""));
    }

    #[test]
    fn password_policy() {
        assert!(check_password_policy("short").is_err());
        assert!(check_password_policy(" leading-space-pass").is_err());
        assert!(check_password_policy("twelve-chars").is_ok());
    }

    #[test]
    fn base32_matches_rfc4648() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw 6ytb oi====").unwrap(), b"foobar");
        assert!(base32_decode("not!base32").is_none());
    }

    // RFC 6238 appendix B, SHA-1, truncated to six digits.
    #[test]
    fn totp_matches_rfc6238() {
        let secret = b"12345678901234567890";
        for (time, code) in [
            (59i64, 287_082u32),
            (1_111_111_109, 81_804),
            (1_111_111_111, 50_471),
            (1_234_567_890, 5_924),
            (2_000_000_000, 279_037),
        ] {
            assert_eq!(totp_code(secret, time / 30), code, "t={time}");
        }
    }

    #[test]
    fn totp_accepts_one_step_of_skew() {
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(verify_totp(&secret, "287082", 59), Some(1));
        assert_eq!(verify_totp(&secret, "287082", 89), Some(1));
        assert_eq!(verify_totp(&secret, "287082", 120), None);
        assert_eq!(verify_totp(&secret, "28708", 59), None);
        assert_eq!(verify_totp(&secret, "abcdef", 59), None);
    }

    #[test]
    fn tokens_hash_and_differ() {
        let (a, ha) = new_token("kcs_");
        let (b, _) = new_token("kcs_");
        assert!(a.starts_with("kcs_"));
        assert_ne!(a, b);
        assert_eq!(ha, sha256_hex(a.as_bytes()));
        assert_eq!(ha.len(), 64);
    }

    #[test]
    fn recovery_codes_match_once_normalized() {
        let (codes, hashes) = new_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        let upper = codes[3].to_uppercase().replace('-', " ");
        assert_eq!(match_recovery_code(&upper, &hashes), Some(3));
        assert_eq!(match_recovery_code("nope", &hashes), None);
    }

    #[test]
    fn otpauth_uri_escapes_label() {
        let uri = otpauth_uri("kcore prod", "alice", "ABC");
        assert!(uri.starts_with("otpauth://totp/kcore%20prod%3Aalice?secret=ABC&issuer=kcore+prod"));
    }
}
