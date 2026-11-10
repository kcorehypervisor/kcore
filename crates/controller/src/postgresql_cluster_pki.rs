//! CA and member certificates for a PostgreSQL cluster.
//!
//! One CA per cluster. Each member gets one certificate used as PostgreSQL
//! server cert, etcd server and peer cert, Patroni REST cert and etcd client
//! cert, so it carries both serverAuth and clientAuth. SANs cover the
//! advertise address, the node hostname, localhost, 127.0.0.1 and the VIP.

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use time::{Duration, OffsetDateTime};

pub const CA_DAYS: i64 = 3650;
pub const MEMBER_DAYS: i64 = 365;

pub fn generate_ca(cluster: &str) -> Result<(String, String)> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + Duration::days(CA_DAYS);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("kcore-postgresql-{cluster}-ca"));
    dn.push(DnType::OrganizationName, "kcore");
    params.distinguished_name = dn;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// `(cert_pem, key_pem, not_after_unix)`.
pub fn issue_member(
    ca_cert_pem: &str,
    ca_key_pem: &str,
    node_id: &str,
    hostname: &str,
    addresses: &[String],
) -> Result<(String, String, i64)> {
    let mut params = CertificateParams::default();
    let not_after = OffsetDateTime::now_utc() + Duration::days(MEMBER_DAYS);
    params.not_before = OffsetDateTime::now_utc() - Duration::minutes(5);
    params.not_after = not_after;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, node_id);
    dn.push(DnType::OrganizationName, "kcore-postgresql");
    params.distinguished_name = dn;
    let mut sans = vec![SanType::DnsName("localhost".try_into()?)];
    for name in [node_id, hostname] {
        if !name.is_empty() {
            if let Ok(dns) = name.try_into() {
                let san = SanType::DnsName(dns);
                if !sans.contains(&san) {
                    sans.push(san);
                }
            }
        }
    }
    let mut ips = vec!["127.0.0.1".to_string()];
    ips.extend(addresses.iter().cloned());
    for ip in ips {
        let san = SanType::IpAddress(ip.parse().with_context(|| format!("invalid IP {ip}"))?);
        if !sans.contains(&san) {
            sans.push(san);
        }
    }
    params.subject_alt_names = sans;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, KeyPair::from_pem(ca_key_pem)?)?;
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, &issuer)?;
    Ok((cert.pem(), key.serialize_pem(), not_after.unix_timestamp()))
}

/// 32 random bytes as URL-safe base64 without padding: safe inside YAML,
/// shell `$(< file)` and libpq connection strings.
pub fn random_password() -> Result<String> {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| anyhow::anyhow!("random source failed"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_cert_chains_to_the_cluster_ca_and_covers_every_address() {
        let (ca, ca_key) = generate_ca("orders").unwrap();
        let (cert, key, not_after) = issue_member(
            &ca,
            &ca_key,
            "node-105",
            "node-105.lab",
            &["192.168.40.105".into(), "192.168.40.150".into()],
        )
        .unwrap();
        assert!(key.contains("PRIVATE KEY"));
        assert!(not_after > OffsetDateTime::now_utc().unix_timestamp());
        let der = pem::parse(&cert).unwrap();
        let (_, parsed) = x509_parser::parse_x509_certificate(der.contents()).unwrap();
        let san = parsed.subject_alternative_name().unwrap().unwrap();
        let mut ips = Vec::new();
        let mut dns = Vec::new();
        for name in &san.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::IPAddress(b) if b.len() == 4 => {
                    ips.push(std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string())
                }
                x509_parser::extensions::GeneralName::DNSName(d) => dns.push(d.to_string()),
                _ => {}
            }
        }
        for want in ["127.0.0.1", "192.168.40.105", "192.168.40.150"] {
            assert!(
                ips.iter().any(|ip| ip == want),
                "{want} missing from {ips:?}"
            );
        }
        for want in ["localhost", "node-105", "node-105.lab"] {
            assert!(dns.iter().any(|d| d == want), "{want} missing from {dns:?}");
        }
        let eku = parsed.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.server_auth && eku.client_auth);
    }

    #[test]
    fn passwords_are_random_and_shell_safe() {
        let a = random_password().unwrap();
        assert_ne!(a, random_password().unwrap());
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(a.len(), 43);
    }
}
