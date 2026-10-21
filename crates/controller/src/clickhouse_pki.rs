//! Cluster CA and per-node certificates for ClickHouse TLS.
//! SANs are the advertise address, the node hostname, localhost, and 127.0.0.1.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use time::{Duration, OffsetDateTime};

pub struct IssuedCert {
    pub cert_pem: String,
    pub key_pem: String,
    pub not_after_unix: i64,
}

pub fn issue_ca(cluster: &str) -> Result<IssuedCert, String> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let now = OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + Duration::days(3650);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("kcore-clickhouse-{cluster}"));
    params.distinguished_name = dn;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
    Ok(IssuedCert {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        not_after_unix: params.not_after.unix_timestamp(),
    })
}

pub fn issue_member(
    cluster: &str,
    node_id: &str,
    hostname: &str,
    advertise: &str,
    ca_cert_pem: &str,
    ca_key_pem: &str,
) -> Result<IssuedCert, String> {
    let mut params = CertificateParams::default();
    let now = OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + Duration::days(365);
    let mut dn = DistinguishedName::new();
    dn.push(
        DnType::CommonName,
        format!("{node_id}.{cluster}.clickhouse.kcore"),
    );
    params.distinguished_name = dn;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let mut sans = vec![
        SanType::DnsName(
            "localhost"
                .try_into()
                .map_err(|_| "localhost san".to_string())?,
        ),
        SanType::IpAddress(
            "127.0.0.1"
                .parse()
                .map_err(|_| "loopback san".to_string())?,
        ),
    ];
    if !hostname.is_empty() && hostname != "localhost" {
        if let Ok(name) = hostname.try_into() {
            sans.push(SanType::DnsName(name));
        }
    }
    if let Ok(ip) = advertise.parse() {
        sans.push(SanType::IpAddress(ip));
    } else if let Ok(name) = advertise.try_into() {
        sans.push(SanType::DnsName(name));
    }
    params.subject_alt_names = sans;
    let ca_key = KeyPair::from_pem(ca_key_pem).map_err(|e| e.to_string())?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key).map_err(|e| e.to_string())?;
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    let cert = params.signed_by(&key, &issuer).map_err(|e| e.to_string())?;
    Ok(IssuedCert {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        not_after_unix: params.not_after.unix_timestamp(),
    })
}
