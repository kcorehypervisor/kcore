use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DnType, ExtendedKeyUsagePurpose, Issuer,
    KeyPair, SanType,
};
use time::{Duration, OffsetDateTime};

pub const CERT_VALIDITY_DAYS: i64 = 365;

/// Build the canonical node leaf parameters. The controller always authors
/// these itself — including for CSR-based rotation — so a node cannot request
/// a CN, SAN or EKU it is not entitled to.
fn node_cert_params(node_host: &str, validity_days: i64) -> Result<CertificateParams, String> {
    let mut params = CertificateParams::new(vec![node_host.to_string()])
        .map_err(|e| format!("invalid SAN: {e}"))?;
    params
        .distinguished_name
        .push(DnType::CommonName, format!("kcore-node-{node_host}"));
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let now = OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + Duration::days(validity_days);
    Ok(params)
}

/// Sign a new node certificate using the sub-CA, returning (chain_pem, key_pem).
/// chain_pem = leaf cert + sub-CA cert concatenated.
///
/// This path generates the node private key on the controller and is only used
/// for bootstrap (`IssueNodeBootstrapCert`) and the deprecated
/// `RenewNodeCert`. Rotation of a running node goes through
/// [`sign_node_csr`], which keeps the key on the node.
pub fn sign_node_cert(
    sub_ca_cert_pem: &str,
    sub_ca_key_pem: &str,
    node_host: &str,
) -> Result<(String, String), String> {
    sign_node_cert_with_validity(
        sub_ca_cert_pem,
        sub_ca_key_pem,
        node_host,
        CERT_VALIDITY_DAYS,
    )
}

/// [`sign_node_cert`] with an explicit lifetime. Tests use short lifetimes so
/// expiry behaviour is exercised without waiting.
pub fn sign_node_cert_with_validity(
    sub_ca_cert_pem: &str,
    sub_ca_key_pem: &str,
    node_host: &str,
    validity_days: i64,
) -> Result<(String, String), String> {
    let params = node_cert_params(node_host, validity_days)?;

    let ca_key =
        KeyPair::from_pem(sub_ca_key_pem).map_err(|e| format!("loading sub-CA key: {e}"))?;
    let issuer = Issuer::from_ca_cert_pem(sub_ca_cert_pem, ca_key)
        .map_err(|e| format!("loading sub-CA cert: {e}"))?;

    let cert_key = KeyPair::generate().map_err(|e| format!("generating node key: {e}"))?;
    let cert = params
        .signed_by(&cert_key, &issuer)
        .map_err(|e| format!("signing node cert: {e}"))?;

    let chain_pem = format!("{}{}", cert.pem(), sub_ca_cert_pem);
    Ok((chain_pem, cert_key.serialize_pem()))
}

/// Sign a node-submitted PKCS#10 CSR with the sub-CA, returning the
/// `leaf + sub-CA` chain PEM.
///
/// The CSR contributes **only** its public key. `rcgen` verifies the CSR's
/// self-signature during parsing (proof of possession of the private key), and
/// we then discard the requested subject and extensions in favour of
/// controller-authored parameters for `node_host`. The requested CN, when
/// present, must match the expected one so a misdirected CSR is rejected loudly
/// instead of being silently rewritten.
pub fn sign_node_csr(
    sub_ca_cert_pem: &str,
    sub_ca_key_pem: &str,
    csr_pem: &str,
    node_host: &str,
    validity_days: i64,
) -> Result<String, String> {
    let mut csr = CertificateSigningRequestParams::from_pem(csr_pem)
        .map_err(|e| format!("invalid CSR: {e}"))?;

    let expected_cn = format!("kcore-node-{node_host}");
    if let Some(requested_cn) = csr
        .params
        .distinguished_name
        .get(&DnType::CommonName)
        .and_then(dn_value_str)
    {
        if requested_cn != expected_cn {
            return Err(format!(
                "CSR subject CN '{requested_cn}' does not match expected '{expected_cn}'"
            ));
        }
    }
    for san in &csr.params.subject_alt_names {
        let matches = match san {
            SanType::DnsName(name) => name.as_str() == node_host,
            SanType::IpAddress(ip) => ip.to_string() == node_host,
            _ => false,
        };
        if !matches {
            return Err(format!(
                "CSR requests a subjectAltName that is not '{node_host}'"
            ));
        }
    }

    csr.params = node_cert_params(node_host, validity_days)?;

    let ca_key =
        KeyPair::from_pem(sub_ca_key_pem).map_err(|e| format!("loading sub-CA key: {e}"))?;
    let issuer = Issuer::from_ca_cert_pem(sub_ca_cert_pem, ca_key)
        .map_err(|e| format!("loading sub-CA cert: {e}"))?;

    let cert = csr
        .signed_by(&issuer)
        .map_err(|e| format!("signing CSR: {e}"))?;

    Ok(format!("{}{}", cert.pem(), sub_ca_cert_pem))
}

/// Text of a DN attribute, for the printable string types we can compare.
/// Non-ASCII encodings return `None`, which means "no CN assertion to check" —
/// the controller substitutes its own subject regardless.
fn dn_value_str(value: &rcgen::DnValue) -> Option<&str> {
    match value {
        rcgen::DnValue::Utf8String(s) => Some(s.as_str()),
        rcgen::DnValue::PrintableString(s) => Some(s.as_str()),
        rcgen::DnValue::Ia5String(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Sign an operator client certificate (`CN=kctl:<name>`) using the sub-CA.
/// Leaf has ClientAuth only; chain_pem = leaf + sub-CA concatenated.
pub fn sign_operator_cert(
    sub_ca_cert_pem: &str,
    sub_ca_key_pem: &str,
    operator_name: &str,
) -> Result<(String, String), String> {
    let cn = format!("kctl:{operator_name}");
    let san = format!("kctl.operator.{operator_name}");
    let mut params = CertificateParams::new(vec![san]).map_err(|e| format!("invalid SAN: {e}"))?;
    params.distinguished_name.push(DnType::CommonName, cn);
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + Duration::days(CERT_VALIDITY_DAYS);

    let ca_key =
        KeyPair::from_pem(sub_ca_key_pem).map_err(|e| format!("loading sub-CA key: {e}"))?;
    let issuer = Issuer::from_ca_cert_pem(sub_ca_cert_pem, ca_key)
        .map_err(|e| format!("loading sub-CA cert: {e}"))?;

    let cert_key = KeyPair::generate().map_err(|e| format!("generating operator key: {e}"))?;
    let cert = params
        .signed_by(&cert_key, &issuer)
        .map_err(|e| format!("signing operator cert: {e}"))?;

    let chain_pem = format!("{}{}", cert.pem(), sub_ca_cert_pem);
    Ok((chain_pem, cert_key.serialize_pem()))
}

/// Validate that a PEM string is a parseable X.509 certificate with CA
/// basicConstraints.
pub fn validate_sub_ca_cert(cert_pem: &str) -> Result<(), String> {
    let pem = pem::parse(cert_pem).map_err(|e| format!("PEM parse error: {e}"))?;
    use x509_parser::prelude::FromDer;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(pem.contents())
        .map_err(|e| format!("X.509 parse error: {e}"))?;
    let bc = cert
        .basic_constraints()
        .map_err(|e| format!("reading basicConstraints: {e}"))?
        .ok_or("certificate has no basicConstraints extension")?;
    if !bc.value.ca {
        return Err("certificate is not a CA".to_string());
    }
    Ok(())
}

/// PKI fixtures shared by the signing, inventory, CRL and OCSP test modules.
#[cfg(test)]
pub mod test_support {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
    use time::{Duration, OffsetDateTime};

    /// Self-signed root CA, returned as `(cert_pem, key_pem)`.
    pub fn generate_test_ca() -> (String, String) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "test-ca");
        params.not_before = OffsetDateTime::now_utc();
        params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    /// Root-signed intermediate with `pathlen:0`, returned as `(cert_pem, key_pem)`.
    pub fn generate_test_sub_ca(ca_cert_pem: &str, ca_key_pem: &str) -> (String, String) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params
            .distinguished_name
            .push(DnType::CommonName, "test-sub-ca");
        params.not_before = OffsetDateTime::now_utc();
        params.not_after = OffsetDateTime::now_utc() + Duration::days(1825);
        let ca_key = KeyPair::from_pem(ca_key_pem).unwrap();
        let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key).unwrap();
        let sub_key = KeyPair::generate().unwrap();
        let sub_cert = params.signed_by(&sub_key, &issuer).unwrap();
        (sub_cert.pem(), sub_key.serialize_pem())
    }

    /// A node CSR as the node-agent would produce it: locally generated key,
    /// CN and SAN for `node_host`. Returns `(csr_pem, key_pem)`.
    pub fn generate_node_csr(node_host: &str) -> (String, String) {
        generate_csr_with_cn(node_host, &format!("kcore-node-{node_host}"))
    }

    /// A node CSR with an arbitrary CN, for negative tests.
    pub fn generate_csr_with_cn(node_host: &str, cn: &str) -> (String, String) {
        let mut params = CertificateParams::new(vec![node_host.to_string()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, cn);
        let key = KeyPair::generate().unwrap();
        let csr = params.serialize_request(&key).unwrap();
        (csr.pem().unwrap(), key.serialize_pem())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        generate_csr_with_cn, generate_node_csr, generate_test_ca, generate_test_sub_ca,
    };
    use super::*;

    #[test]
    fn sign_node_cert_produces_chain() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (chain, key) = sign_node_cert(&sub_cert, &sub_key, "10.0.0.50").unwrap();
        assert!(key.contains("BEGIN PRIVATE KEY"));
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 2);
    }

    #[test]
    fn sign_operator_cert_uses_kctl_cn_and_client_auth() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (chain, key) = sign_operator_cert(&sub_cert, &sub_key, "alice").unwrap();
        assert!(key.contains("BEGIN PRIVATE KEY"));
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 2);
        let end = chain
            .find("-----END CERTIFICATE-----")
            .expect("first cert end");
        let first_pem = &chain[..end + "-----END CERTIFICATE-----".len()];
        let pem = pem::parse(first_pem).expect("pem leaf");
        use x509_parser::prelude::FromDer;
        let (_, cert) =
            x509_parser::certificate::X509Certificate::from_der(pem.contents()).expect("x509");
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cn, "kctl:alice");
    }

    #[test]
    fn validate_sub_ca_cert_accepts_ca() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, _) = generate_test_sub_ca(&ca_cert, &ca_key);
        validate_sub_ca_cert(&sub_cert).unwrap();
    }

    #[test]
    fn validate_sub_ca_cert_rejects_leaf() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (leaf, _) = sign_node_cert(
            &{
                let (sc, sk) = generate_test_sub_ca(&ca_cert, &ca_key);
                let _ = sk;
                sc
            },
            &generate_test_sub_ca(&ca_cert, &ca_key).1,
            "10.0.0.1",
        )
        .unwrap();
        let first_cert = leaf
            .split("-----END CERTIFICATE-----")
            .next()
            .unwrap()
            .to_string()
            + "-----END CERTIFICATE-----\n";
        let err = validate_sub_ca_cert(&first_cert);
        assert!(err.is_err());
    }

    #[test]
    fn sign_node_csr_issues_chain_bound_to_the_csr_public_key() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (csr, node_key) = generate_node_csr("10.0.0.20");

        let chain = sign_node_csr(&sub_cert, &sub_key, &csr, "10.0.0.20", 30).expect("sign csr");
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 2);

        // The issued leaf must carry the public key of the node-held key, so
        // the node can serve TLS with the key it never transmitted.
        let leaf = pem::parse(
            chain
                .split_inclusive("-----END CERTIFICATE-----")
                .next()
                .expect("leaf pem"),
        )
        .expect("pem");
        use x509_parser::prelude::FromDer;
        let (_, cert) =
            x509_parser::certificate::X509Certificate::from_der(leaf.contents()).expect("x509");
        let node_pub = KeyPair::from_pem(&node_key)
            .expect("key")
            .public_key_raw()
            .to_vec();
        assert_eq!(cert.public_key().subject_public_key.data.to_vec(), node_pub);
    }

    #[test]
    fn sign_node_csr_authors_cn_san_and_eku_itself() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (csr, _) = generate_node_csr("10.0.0.21");

        let chain = sign_node_csr(&sub_cert, &sub_key, &csr, "10.0.0.21", 7).expect("sign csr");
        let meta = crate::pki::inventory::metadata_from_pem(&chain).expect("metadata");
        assert_eq!(meta.subject_cn, "kcore-node-10.0.0.21");
        assert_eq!(meta.issuer_cn, "test-sub-ca");
        // Lifetime comes from the controller argument, not the CSR.
        let lifetime = (meta.not_after - meta.not_before).whole_days();
        assert_eq!(lifetime, 7);
    }

    #[test]
    fn sign_node_csr_rejects_mismatched_common_name() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (csr, _) = generate_csr_with_cn("10.0.0.22", "kcore-controller-10.0.0.22");

        let err = sign_node_csr(&sub_cert, &sub_key, &csr, "10.0.0.22", 30)
            .expect_err("CN mismatch must be rejected");
        assert!(err.contains("does not match expected"), "unexpected: {err}");
    }

    #[test]
    fn sign_node_csr_rejects_san_for_another_host() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        // CSR asks for the right CN but a SAN belonging to a different node.
        let (csr, _) = generate_csr_with_cn("10.0.0.99", "kcore-node-10.0.0.23");

        let err = sign_node_csr(&sub_cert, &sub_key, &csr, "10.0.0.23", 30)
            .expect_err("foreign SAN must be rejected");
        assert!(err.contains("subjectAltName"), "unexpected: {err}");
    }

    #[test]
    fn sign_node_csr_rejects_malformed_csr() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let err = sign_node_csr(&sub_cert, &sub_key, "not-a-csr", "10.0.0.24", 30)
            .expect_err("garbage CSR must be rejected");
        assert!(err.contains("invalid CSR"), "unexpected: {err}");
    }

    #[test]
    fn sign_node_cert_with_validity_honours_short_lifetimes() {
        let (ca_cert, ca_key) = generate_test_ca();
        let (sub_cert, sub_key) = generate_test_sub_ca(&ca_cert, &ca_key);
        let (chain, _) =
            sign_node_cert_with_validity(&sub_cert, &sub_key, "10.0.0.25", 2).expect("sign");
        let meta = crate::pki::inventory::metadata_from_pem(&chain).expect("metadata");
        assert_eq!((meta.not_after - meta.not_before).whole_days(), 2);
    }
}
