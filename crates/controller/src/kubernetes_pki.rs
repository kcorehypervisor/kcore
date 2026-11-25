use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
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
    // Generate Kubernetes CA
    let (ca_cert, ca_key) = generate_ca("kubernetes-ca", "kubernetes", 3650)?;

    // Generate etcd CA
    let (etcd_ca_cert, etcd_ca_key) = generate_ca("etcd-ca", "etcd", 3650)?;

    // Generate service account keys
    let (sa_key, sa_pub) = generate_key_pair()?;

    let mut pki = KubernetesPki {
        ca_cert,
        ca_key,
        sa_key,
        sa_pub,
        etcd_ca_cert,
        etcd_ca_key,
        apiserver_cert: String::new(),
        apiserver_key: String::new(),
        apiserver_kubelet_client_cert: String::new(),
        apiserver_kubelet_client_key: String::new(),
        apiserver_etcd_client_cert: String::new(),
        apiserver_etcd_client_key: String::new(),
        etcd_node_certs: HashMap::new(),
        etcd_node_peer_certs: HashMap::new(),
        admin_kubeconfig: String::new(),
    };
    renew_leaf_certificates(&mut pki, cluster_name, api_endpoint, service_cidr, nodes)?;
    Ok(pki)
}

/// Validity of every leaf certificate this module issues.
pub const LEAF_VALIDITY_DAYS: u32 = 365;

/// Issue every leaf certificate again from the stored CAs. The CAs and the
/// service account keys stay as they are, so nodes that still hold the old
/// leaves keep verifying against the same roots while a rollout is in flight.
/// Per-node kubelet client certificates are not stored and are not touched.
pub fn renew_leaf_certificates(
    pki: &mut KubernetesPki,
    cluster_name: &str,
    api_endpoint: &str,
    service_cidr: &str,
    nodes: &[NodeInfo],
) -> Result<()> {
    renew_leaf_certificates_for(
        pki,
        cluster_name,
        api_endpoint,
        service_cidr,
        nodes,
        LEAF_VALIDITY_DAYS,
    )
}

pub fn renew_leaf_certificates_for(
    pki: &mut KubernetesPki,
    cluster_name: &str,
    api_endpoint: &str,
    service_cidr: &str,
    nodes: &[NodeInfo],
    leaf_validity_days: u32,
) -> Result<()> {
    let api_vip = api_endpoint
        .split(':')
        .next()
        .context("invalid API endpoint")?;
    let service_cluster_ip = parse_first_ip(service_cidr)?;
    let days = if leaf_validity_days == 0 {
        LEAF_VALIDITY_DAYS
    } else {
        leaf_validity_days
    };

    let mut api_sans = vec![
        SanType::DnsName("kubernetes".try_into().unwrap()),
        SanType::DnsName("kubernetes.default".try_into().unwrap()),
        SanType::DnsName("kubernetes.default.svc".try_into().unwrap()),
        SanType::DnsName("kubernetes.default.svc.cluster.local".try_into().unwrap()),
        SanType::IpAddress(service_cluster_ip.parse().context("invalid service IP")?),
        SanType::IpAddress(api_vip.parse().context("invalid API VIP")?),
        SanType::IpAddress("127.0.0.1".parse().unwrap()),
    ];
    for node in nodes {
        api_sans.push(SanType::DnsName(node.hostname.as_str().try_into().unwrap()));
        api_sans.push(SanType::IpAddress(
            node.ip_address.parse().context("invalid node IP")?,
        ));
    }

    let (apiserver_cert, apiserver_key) = generate_server_cert(
        "kube-apiserver",
        "kubernetes",
        &api_sans,
        &pki.ca_cert,
        &pki.ca_key,
        days,
    )?;
    let (apiserver_kubelet_client_cert, apiserver_kubelet_client_key) = generate_client_cert(
        "kube-apiserver-kubelet-client",
        "system:masters",
        &pki.ca_cert,
        &pki.ca_key,
        days,
    )?;
    let (apiserver_etcd_client_cert, apiserver_etcd_client_key) = generate_client_cert(
        "kube-apiserver-etcd-client",
        "system:masters",
        &pki.etcd_ca_cert,
        &pki.etcd_ca_key,
        days,
    )?;

    let mut etcd_node_certs = HashMap::new();
    let mut etcd_node_peer_certs = HashMap::new();
    for node in nodes {
        let (server, peer) = etcd_certs_for_node(pki, node)?;
        etcd_node_certs.insert(node.node_id.clone(), server);
        etcd_node_peer_certs.insert(node.node_id.clone(), peer);
    }

    let (admin_cert, admin_key) = generate_client_cert(
        "kubernetes-admin",
        "system:masters",
        &pki.ca_cert,
        &pki.ca_key,
        days,
    )?;
    let admin_kubeconfig = generate_kubeconfig(
        cluster_name,
        &format!("https://{api_endpoint}"),
        &pki.ca_cert,
        &admin_cert,
        &admin_key,
        "kubernetes-admin",
    )?;

    pki.apiserver_cert = apiserver_cert;
    pki.apiserver_key = apiserver_key;
    pki.apiserver_kubelet_client_cert = apiserver_kubelet_client_cert;
    pki.apiserver_kubelet_client_key = apiserver_kubelet_client_key;
    pki.apiserver_etcd_client_cert = apiserver_etcd_client_cert;
    pki.apiserver_etcd_client_key = apiserver_etcd_client_key;
    pki.etcd_node_certs = etcd_node_certs;
    pki.etcd_node_peer_certs = etcd_node_peer_certs;
    pki.admin_kubeconfig = admin_kubeconfig;
    Ok(())
}

pub fn pem_not_after_public(pem_str: &str) -> Option<OffsetDateTime> {
    pem_not_after(pem_str)
}

fn pem_not_after(pem_str: &str) -> Option<OffsetDateTime> {
    use x509_parser::prelude::FromDer;
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem_str.as_bytes()).ok()?;
    let (_, cert) =
        x509_parser::certificate::X509Certificate::from_der(pem.contents.as_slice()).ok()?;
    OffsetDateTime::from_unix_timestamp(cert.validity().not_after.timestamp()).ok()
}

/// The soonest expiry among the leaf certificates stored in `pki`.
/// None when a certificate cannot be parsed or none is present.
pub fn earliest_leaf_not_after(pki: &KubernetesPki) -> Option<OffsetDateTime> {
    let mut pems: Vec<&str> = vec![
        &pki.apiserver_cert,
        &pki.apiserver_kubelet_client_cert,
        &pki.apiserver_etcd_client_cert,
    ];
    for (cert, _) in pki
        .etcd_node_certs
        .values()
        .chain(pki.etcd_node_peer_certs.values())
    {
        pems.push(cert);
    }
    let mut earliest: Option<OffsetDateTime> = None;
    for pem in pems {
        let not_after = pem_not_after(pem)?;
        earliest = Some(earliest.map_or(not_after, |e| e.min(not_after)));
    }
    earliest
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

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

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

    let ca_key = KeyPair::from_pem(ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key)?;

    let key_pair = KeyPair::generate()?;
    let cert = params.signed_by(&key_pair, &issuer)?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    Ok((cert_pem, key_pem))
}

fn generate_client_cert(
    cn: &str,
    org: &str,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    days: u32,
) -> Result<(String, String)> {
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

    let ca_key = KeyPair::from_pem(ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key)?;

    let key_pair = KeyPair::generate()?;
    let cert = params.signed_by(&key_pair, &issuer)?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    Ok((cert_pem, key_pem))
}

fn generate_key_pair() -> Result<(String, String)> {
    let key = KeyPair::generate()?;
    let private_pem = key.serialize_pem();
    let public_pem = key
        .public_key_pem()
        .lines()
        .skip(1)
        .take_while(|line: &&str| !line.starts_with("-----END"))
        .collect::<Vec<_>>()
        .join("\n");
    let public_pem = format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        public_pem
    );

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
    let ca_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        ca_cert.as_bytes(),
    );
    let cert_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_cert.as_bytes(),
    );
    let key_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_key.as_bytes(),
    );

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

/// Issue a kubelet client certificate for a node joining an existing cluster,
/// and the etcd server and peer certificates that node's Nix module expects.
/// The stored CA and admin kubeconfig are left as they are.
pub fn issue_node_certificates(
    pki: &mut KubernetesPki,
    cluster_name: &str,
    api_endpoint: &str,
    node: &NodeInfo,
) -> Result<String> {
    if !pki.etcd_node_certs.contains_key(&node.node_id)
        || !pki.etcd_node_peer_certs.contains_key(&node.node_id)
    {
        let (etcd_server, etcd_peer) = etcd_certs_for_node(pki, node)?;
        pki.etcd_node_certs
            .insert(node.node_id.clone(), etcd_server);
        pki.etcd_node_peer_certs
            .insert(node.node_id.clone(), etcd_peer);
    }
    let (cert, key) = generate_client_cert(
        &format!("system:node:{}", node.hostname),
        "system:nodes",
        &pki.ca_cert,
        &pki.ca_key,
        365,
    )?;
    let server = if api_endpoint.starts_with("https://") || api_endpoint.starts_with("http://") {
        api_endpoint.to_string()
    } else {
        format!("https://{api_endpoint}")
    };
    generate_kubeconfig(
        cluster_name,
        &server,
        &pki.ca_cert,
        &cert,
        &key,
        &format!("system:node:{}", node.hostname),
    )
}

fn etcd_certs_for_node(
    pki: &KubernetesPki,
    node: &NodeInfo,
) -> Result<((String, String), (String, String))> {
    let sans = vec![
        SanType::DnsName(node.hostname.as_str().try_into().unwrap()),
        SanType::DnsName("localhost".try_into().unwrap()),
        SanType::IpAddress(node.ip_address.parse().context("invalid node IP")?),
        SanType::IpAddress("127.0.0.1".parse().unwrap()),
    ];
    let server = generate_server_cert(
        &node.hostname,
        "etcd",
        &sans,
        &pki.etcd_ca_cert,
        &pki.etcd_ca_key,
        365,
    )?;
    let peer = generate_server_cert(
        &node.hostname,
        "etcd",
        &sans,
        &pki.etcd_ca_cert,
        &pki.etcd_ca_key,
        365,
    )?;
    Ok((server, peer))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes() -> Vec<NodeInfo> {
        vec![NodeInfo {
            node_id: "n1".to_string(),
            hostname: "node-1".to_string(),
            ip_address: "10.0.0.2".to_string(),
        }]
    }

    #[test]
    fn leaf_expiry_is_about_a_year_out() {
        let pki =
            generate_kubernetes_pki("lab", "10.0.0.1:6443", "10.96.0.0/12", &nodes()).expect("pki");
        let not_after = earliest_leaf_not_after(&pki).expect("expiry");
        let days = (not_after - OffsetDateTime::now_utc()).whole_days();
        assert!((363..=366).contains(&days), "got {days} days");
    }

    #[test]
    fn renewal_keeps_cas_and_replaces_leaves() {
        let mut pki =
            generate_kubernetes_pki("lab", "10.0.0.1:6443", "10.96.0.0/12", &nodes()).expect("pki");
        let before = pki.clone();
        renew_leaf_certificates(&mut pki, "lab", "10.0.0.1:6443", "10.96.0.0/12", &nodes())
            .expect("renew");
        assert_eq!(pki.ca_cert, before.ca_cert);
        assert_eq!(pki.ca_key, before.ca_key);
        assert_eq!(pki.etcd_ca_cert, before.etcd_ca_cert);
        assert_eq!(pki.sa_key, before.sa_key);
        assert_ne!(pki.apiserver_cert, before.apiserver_cert);
        assert_ne!(pki.apiserver_key, before.apiserver_key);
        assert_ne!(pki.admin_kubeconfig, before.admin_kubeconfig);
        assert_ne!(pki.etcd_node_certs["n1"].0, before.etcd_node_certs["n1"].0);
        assert!(earliest_leaf_not_after(&pki).is_some());
    }
}
