use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use std::collections::HashMap;
use time::{Duration, OffsetDateTime};

/// Complete Kubernetes PKI for a cluster
#[derive(Debug, Clone)]
pub struct KubernetesPki {
    /// Kubernetes CA certificate (PEM)
    pub ca_cert: String,
    /// Kubernetes CA private key (PEM)
    pub ca_key: String,
    /// Service account signing key (PEM)
    pub sa_key: String,
    /// Service account public key (PEM)
    pub sa_pub: String,
    /// etcd CA certificate (PEM)
    pub etcd_ca_cert: String,
    /// etcd CA private key (PEM)
    pub etcd_ca_key: String,
    /// API server certificate (PEM)
    pub apiserver_cert: String,
    /// API server private key (PEM)
    pub apiserver_key: String,
    /// API server kubelet client certificate (PEM)
    pub apiserver_kubelet_client_cert: String,
    /// API server kubelet client private key (PEM)
    pub apiserver_kubelet_client_key: String,
    /// API server etcd client certificate (PEM)
    pub apiserver_etcd_client_cert: String,
    /// API server etcd client private key (PEM)
    pub apiserver_etcd_client_key: String,
    /// Per-node etcd server certificates: node_id -> (cert_pem, key_pem)
    pub etcd_node_certs: HashMap<String, (String, String)>,
    /// Per-node etcd peer certificates: node_id -> (cert_pem, key_pem)
    pub etcd_node_peer_certs: HashMap<String, (String, String)>,
    /// Admin kubeconfig content
    pub admin_kubeconfig: String,
}

/// Node information for certificate generation
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub node_id: String,
    pub hostname: String,
    pub ip_address: String,
}

/// Generate complete Kubernetes PKI for a cluster
pub fn generate_kubernetes_pki(
    cluster_name: &str,
    api_endpoint: &str,
    service_cidr: &str,
    nodes: &[NodeInfo],
) -> Result<KubernetesPki> {
    // Parse API endpoint
    let api_vip = api_endpoint
        .split(':')
        .next()
        .context("invalid API endpoint")?;

    // Parse service cluster IP (first IP in service CIDR)
    let service_cluster_ip = parse_first_ip(service_cidr)?;

    // Generate Kubernetes CA
    let (ca_cert, ca_key) = generate_ca("kubernetes-ca", "kubernetes", 3650)?;

    // Generate etcd CA
    let (etcd_ca_cert, etcd_ca_key) = generate_ca("etcd-ca", "etcd", 3650)?;

    // Generate service account keys
    let (sa_key, sa_pub) = generate_key_pair()?;

    // Build API server SANs
    let mut api_sans = vec![
        SanType::DnsName("kubernetes".to_string()),
        SanType::DnsName("kubernetes.default".to_string()),
        SanType::DnsName("kubernetes.default.svc".to_string()),
        SanType::DnsName("kubernetes.default.svc.cluster.local".to_string()),
        SanType::IpAddress(service_cluster_ip.parse().context("invalid service IP")?),
        SanType::IpAddress(api_vip.parse().context("invalid API VIP")?),
        SanType::IpAddress("127.0.0.1".parse().unwrap()),
    ];

    for node in nodes {
        api_sans.push(SanType::DnsName(node.hostname.clone()));
        api_sans.push(SanType::IpAddress(
            node.ip_address.parse().context("invalid node IP")?,
        ));
    }

    // Generate API server certificate
    let (apiserver_cert, apiserver_key) = generate_server_cert(
        "kube-apiserver",
        "kubernetes",
        &api_sans,
        &ca_cert,
        &ca_key,
        365,
    )?;

    // Generate API server kubelet client certificate
    let (apiserver_kubelet_client_cert, apiserver_kubelet_client_key) = generate_client_cert(
        "kube-apiserver-kubelet-client",
        "system:masters",
        &ca_cert,
        &ca_key,
        365,
    )?;

    // Generate API server etcd client certificate
    let (apiserver_etcd_client_cert, apiserver_etcd_client_key) = generate_client_cert(
        "kube-apiserver-etcd-client",
        "system:masters",
        &etcd_ca_cert,
        &etcd_ca_key,
        365,
    )?;

    // Generate per-node etcd certificates
    let mut etcd_node_certs = HashMap::new();
    let mut etcd_node_peer_certs = HashMap::new();

    for node in nodes {
        let etcd_sans = vec![
            SanType::DnsName(node.hostname.clone()),
            SanType::DnsName("localhost".to_string()),
            SanType::IpAddress(node.ip_address.parse().context("invalid node IP")?),
            SanType::IpAddress("127.0.0.1".parse().unwrap()),
        ];

        let (server_cert, server_key) = generate_server_cert(
            &node.hostname,
            "etcd",
            &etcd_sans,
            &etcd_ca_cert,
            &etcd_ca_key,
            365,
        )?;

        let (peer_cert, peer_key) = generate_server_cert(
            &node.hostname,
            "etcd",
            &etcd_sans,
            &etcd_ca_cert,
            &etcd_ca_key,
            365,
        )?;

        etcd_node_certs.insert(node.node_id.clone(), (server_cert, server_key));
        etcd_node_peer_certs.insert(node.node_id.clone(), (peer_cert, peer_key));
    }

    // Generate admin kubeconfig
    let (admin_cert, admin_key) = generate_client_cert(
        "kubernetes-admin",
        "system:masters",
        &ca_cert,
        &ca_key,
        365,
    )?;

    let admin_kubeconfig = generate_kubeconfig(
        cluster_name,
        &format!("https://{api_endpoint}"),
        &ca_cert,
        &admin_cert,
        &admin_key,
        "kubernetes-admin",
    )?;

    Ok(KubernetesPki {
        ca_cert,
        ca_key,
        sa_key,
        sa_pub,
        etcd_ca_cert,
        etcd_ca_key,
        apiserver_cert,
        apiserver_key,
        apiserver_kubelet_client_cert,
        apiserver_kubelet_client_key,
        apiserver_etcd_client_cert,
        apiserver_etcd_client_key,
        etcd_node_certs,
        etcd_node_peer_certs,
        admin_kubeconfig,
    })
}

fn generate_ca(cn: &str, org: &str, days: u32) -> Result<(String, String)> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + Duration::days(days as i64);

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, org);
    params.distinguished_name = dn;

    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];

    let cert = Certificate::from_params(params).context("failed to generate CA certificate")?;
    let cert_pem = cert.serialize_pem().context("failed to serialize CA cert")?;
    let key_pem = cert.serialize_private_key_pem();

    Ok((cert_pem, key_pem))
}

fn generate_server_cert(
    cn: &str,
    org: &str,
    sans: &[SanType],
    ca_cert_pem: &str,
    ca_key_pem: &str,
    days: u32,
) -> Result<(String, String)> {
    let ca_cert_params = CertificateParams::from_ca_cert_pem(ca_cert_pem, KeyPair::from_pem(ca_key_pem)?)
        .context("failed to parse CA certificate")?;
    let ca_cert = Certificate::from_params(ca_cert_params)?;

    let mut params = CertificateParams::default();
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + Duration::days(days as i64);

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, org);
    params.distinguished_name = dn;

    params.subject_alt_names = sans.to_vec();

    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];

    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];

    let cert = Certificate::from_params(params).context("failed to generate server certificate")?;
    let cert_pem = cert
        .serialize_pem_with_signer(&ca_cert)
        .context("failed to serialize server cert")?;
    let key_pem = cert.serialize_private_key_pem();

    Ok((cert_pem, key_pem))
}

fn generate_client_cert(
    cn: &str,
    org: &str,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    days: u32,
) -> Result<(String, String)> {
    let ca_cert_params = CertificateParams::from_ca_cert_pem(ca_cert_pem, KeyPair::from_pem(ca_key_pem)?)
        .context("failed to parse CA certificate")?;
    let ca_cert = Certificate::from_params(ca_cert_params)?;

    let mut params = CertificateParams::default();
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + Duration::days(days as i64);

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, org);
    params.distinguished_name = dn;

    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];

    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];

    let cert = Certificate::from_params(params).context("failed to generate client certificate")?;
    let cert_pem = cert
        .serialize_pem_with_signer(&ca_cert)
        .context("failed to serialize client cert")?;
    let key_pem = cert.serialize_private_key_pem();

    Ok((cert_pem, key_pem))
}

fn generate_key_pair() -> Result<(String, String)> {
    let key = KeyPair::generate(&rcgen::PKCS_RSA_SHA256)?;
    let private_pem = key.serialize_pem();
    let public_pem = key
        .public_key_pem()
        .lines()
        .skip(1)
        .take_while(|line| !line.starts_with("-----END"))
        .collect::<Vec<_>>()
        .join("\n");
    let public_pem = format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", public_pem);

    Ok((private_pem, public_pem))
}

fn generate_kubeconfig(
    cluster_name: &str,
    server: &str,
    ca_cert: &str,
    client_cert: &str,
    client_key: &str,
    user_name: &str,
) -> Result<String> {
    let ca_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, ca_cert.as_bytes());
    let cert_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, client_cert.as_bytes());
    let key_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, client_key.as_bytes());

    let kubeconfig = format!(
        r#"apiVersion: v1
kind: Config
clusters:
- cluster:
    certificate-authority-data: {ca_b64}
    server: {server}
  name: {cluster_name}
contexts:
- context:
    cluster: {cluster_name}
    user: {user_name}
  name: {user_name}@{cluster_name}
current-context: {user_name}@{cluster_name}
users:
- name: {user_name}
  user:
    client-certificate-data: {cert_b64}
    client-key-data: {key_b64}
"#
    );

    Ok(kubeconfig)
}

fn parse_first_ip(cidr: &str) -> Result<String> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid CIDR: {}", cidr);
    }

    let ip_parts: Vec<&str> = parts[0].split('.').collect();
    if ip_parts.len() != 4 {
        anyhow::bail!("invalid IP in CIDR: {}", cidr);
    }

    let last_octet: u8 = ip_parts[3].parse().context("invalid IP octet")?;
    Ok(format!(
        "{}.{}.{}.{}",
        ip_parts[0],
        ip_parts[1],
        ip_parts[2],
        last_octet + 1
    ))
}
