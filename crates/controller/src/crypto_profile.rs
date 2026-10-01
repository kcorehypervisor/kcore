//! Cryptographic posture reported by `GetCryptoConfig` and the compliance report.
//!
//! The lists match `install_fips_crypto_provider` in `main.rs`: AES-GCM only,
//! P-256 and P-384, AWS-LC as the rustls backend.

pub const CRYPTO_LIBRARY: &str = "aws-lc-rs (AWS-LC, FIPS 140-3 #4816)";
pub const FIPS_CERTIFICATE: &str = "FIPS 140-3 #4816";

/// How published release artifacts are signed. This is a property of the
/// release process (`scripts/release.sh`), not of a particular RPC response.
pub const SIGNING_SCHEME: &str = "sigstore-keyless (cosign over SHA256SUMS)";

const TLS13: &[&str] = &["TLS_AES_256_GCM_SHA384", "TLS_AES_128_GCM_SHA256"];

const TLS12: &[&str] = &[
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
];

const KX: &[&str] = &["secp384r1 (P-384)", "secp256r1 (P-256)"];

const EXCLUDED: &[&str] = &["ChaCha20-Poly1305", "X25519", "RSA key exchange"];

pub fn tls13_cipher_suites() -> Vec<String> {
    owned(TLS13)
}

pub fn tls12_cipher_suites() -> Vec<String> {
    owned(TLS12)
}

pub fn kx_groups() -> Vec<String> {
    owned(KX)
}

pub fn excluded_algorithms() -> Vec<String> {
    owned(EXCLUDED)
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}
