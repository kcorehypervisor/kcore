//! Node-side certificate lifecycle.
//!
//! Three concerns live here:
//!
//! * [`rotate`] — generate a keypair and CSR, get it signed by the controller
//!   (`Controller.SignNodeCsr`), install the chain atomically and ask the
//!   listener to reload. The private key never leaves the node.
//! * [`reload`] — in-process TLS reload: rebuild the gRPC listener from the
//!   material on disk without exec'ing a new process.
//! * [`revocation`] — reject inbound peers whose certificate serial appears on
//!   the controller's CRL, with a configurable stale-data failure mode.
//!
//! See `docs/mtls-bootstrap-and-auth.md` §4.

pub mod ocsp_client;
pub mod reload;
pub mod revocation;
pub mod rotate;

use time::OffsetDateTime;

/// Uppercase hex without separators, matching `openssl x509 -serial` output
/// and the controller's inventory representation.
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

/// The facts about a leaf certificate the rotation and reload paths need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertFacts {
    /// Uppercase hex of the DER serial integer bytes.
    pub serial_hex: String,
    pub subject_cn: String,
    /// DNS and IP subjectAltNames, rendered as strings.
    pub sans: Vec<String>,
    pub not_before: OffsetDateTime,
    pub not_after: OffsetDateTime,
    /// DER SubjectPublicKeyInfo, used to prove a signed chain matches the key
    /// we generated for it.
    pub spki_der: Vec<u8>,
}

impl CertFacts {
    /// Whole days until `not_after`; negative once expired.
    pub fn days_remaining(&self, now: OffsetDateTime) -> i64 {
        (self.not_after - now).whole_days()
    }
}

/// Parse the **first** certificate of a PEM bundle. Node cert files hold
/// `leaf + sub-CA`, and everything here is about the leaf.
pub fn facts_from_pem(pem_bundle: &str) -> Result<CertFacts, String> {
    let block = pem::parse(pem_bundle).map_err(|e| format!("PEM parse error: {e}"))?;
    if block.tag() != "CERTIFICATE" {
        return Err(format!(
            "expected a CERTIFICATE block, got '{}'",
            block.tag()
        ));
    }
    facts_from_der(block.contents())
}

/// [`facts_from_pem`] for DER input, which is also what rustls hands us for
/// peer certificates.
pub fn facts_from_der(der: &[u8]) -> Result<CertFacts, String> {
    use x509_parser::prelude::FromDer;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der)
        .map_err(|e| format!("X.509 parse error: {e}"))?;

    let subject_cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|c| c.as_str().ok())
        .unwrap_or_default()
        .to_string();

    let mut sans = Vec::new();
    if let Ok(Some(ext)) = cert.subject_alternative_name() {
        for name in &ext.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::DNSName(n) => sans.push(n.to_string()),
                x509_parser::extensions::GeneralName::IPAddress(bytes) => {
                    if let Some(rendered) = render_ip(bytes) {
                        sans.push(rendered);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(CertFacts {
        serial_hex: hex_upper(cert.raw_serial()),
        subject_cn,
        sans,
        not_before: cert.validity().not_before.to_datetime(),
        not_after: cert.validity().not_after.to_datetime(),
        spki_der: cert.public_key().raw.to_vec(),
    })
}

/// Render the 4- or 16-byte network-order address from an X.509 iPAddress SAN.
fn render_ip(bytes: &[u8]) -> Option<String> {
    match bytes.len() {
        4 => {
            let octets: [u8; 4] = bytes.try_into().ok()?;
            Some(std::net::Ipv4Addr::from(octets).to_string())
        }
        16 => {
            let octets: [u8; 16] = bytes.try_into().ok()?;
            Some(std::net::Ipv6Addr::from(octets).to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
pub mod test_support {
    /// Install the aws-lc-rs provider once per test process.
    ///
    /// `main` does this via `install_fips_crypto_provider`, but unit tests
    /// never run `main`, and `rustls` is built with `default-features = false`
    /// so there is no implicit default provider to fall back on. Any test that
    /// builds a `ClientTlsConfig` needs this first.
    pub fn ensure_crypto_provider() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
    }

    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    };
    use time::{Duration, OffsetDateTime};

    /// Self-signed CA, returned as `(cert_pem, key_pem)`.
    pub fn test_ca(cn: &str) -> (String, String) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, cn);
        params.not_before = OffsetDateTime::now_utc() - Duration::days(1);
        params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
        let key = KeyPair::generate().expect("ca key");
        let cert = params.self_signed(&key).expect("self-sign ca");
        (cert.pem(), key.serialize_pem())
    }

    /// Node leaf signed by `ca`, mirroring what the controller issues.
    /// Returns `(chain_pem, key_pem)` where the chain is `leaf + CA`.
    pub fn node_leaf(
        ca_cert_pem: &str,
        ca_key_pem: &str,
        host: &str,
        validity: Duration,
    ) -> (String, String) {
        let mut params = CertificateParams::new(vec![host.to_string()]).expect("san");
        params
            .distinguished_name
            .push(DnType::CommonName, format!("kcore-node-{host}"));
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::minutes(1);
        params.not_after = now + validity;

        let ca_key = KeyPair::from_pem(ca_key_pem).expect("ca key");
        let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key).expect("issuer");
        let leaf_key = KeyPair::generate().expect("leaf key");
        let leaf = params.signed_by(&leaf_key, &issuer).expect("sign leaf");
        (
            format!("{}{}", leaf.pem(), ca_cert_pem),
            leaf_key.serialize_pem(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn hex_upper_is_uppercase_and_padded() {
        assert_eq!(hex_upper(&[0x00, 0x0a, 0xff]), "000AFF");
        assert_eq!(hex_upper(&[]), "");
    }

    #[test]
    fn facts_from_pem_reads_cn_san_and_validity() {
        let (ca_cert, ca_key) = test_support::test_ca("test-sub-ca");
        let (chain, _key) =
            test_support::node_leaf(&ca_cert, &ca_key, "10.0.0.7", Duration::days(30));

        let facts = facts_from_pem(&chain).expect("parse chain");
        assert_eq!(facts.subject_cn, "kcore-node-10.0.0.7");
        assert_eq!(facts.sans, vec!["10.0.0.7".to_string()]);
        assert!(!facts.serial_hex.is_empty());
        assert!(!facts.spki_der.is_empty());
        let days = facts.days_remaining(OffsetDateTime::now_utc());
        assert!((29..=30).contains(&days), "expected ~30 days, got {days}");
    }

    #[test]
    fn facts_from_pem_takes_the_leaf_not_the_ca() {
        let (ca_cert, ca_key) = test_support::test_ca("test-sub-ca");
        let (chain, _key) =
            test_support::node_leaf(&ca_cert, &ca_key, "10.0.0.8", Duration::days(30));
        // The chain is leaf-first; parsing must not walk to the CA block.
        assert_eq!(
            facts_from_pem(&chain).expect("parse").subject_cn,
            "kcore-node-10.0.0.8"
        );
        assert_eq!(
            facts_from_pem(&ca_cert).expect("parse ca").subject_cn,
            "test-sub-ca"
        );
    }

    #[test]
    fn facts_from_pem_rejects_non_certificate_input() {
        assert!(facts_from_pem("not a pem at all").is_err());
        let key = rcgen::KeyPair::generate().expect("key");
        let err = facts_from_pem(&key.serialize_pem()).expect_err("key is not a cert");
        assert!(err.contains("CERTIFICATE"), "{err}");
    }

    #[test]
    fn render_ip_handles_v4_v6_and_rejects_other_lengths() {
        assert_eq!(render_ip(&[10, 0, 0, 1]), Some("10.0.0.1".to_string()));
        assert_eq!(render_ip(&[0u8; 16]), Some("::".to_string()));
        assert_eq!(render_ip(&[1, 2, 3]), None);
    }
}
