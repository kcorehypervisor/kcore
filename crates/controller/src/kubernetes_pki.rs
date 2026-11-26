use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use std::collections::HashMap;
use time::{Duration, OffsetDateTime};

/// Complete Kubernetes PKI for a cluster
#[derive(Debug, Clone, Default)]
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
    /// Retiring Kubernetes CA during a dual-CA rotation; empty when idle.
    pub previous_ca_cert: String,
    pub previous_ca_key: String,
    /// Retiring etcd CA during a dual-CA rotation; empty when idle or when
    /// the rotation leaves etcd alone.
    pub previous_etcd_ca_cert: String,
    pub previous_etcd_ca_key: String,
}

/// Where a cluster is in a dual-CA rotation, derived from the stored PKI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaRotationPhase {
    /// One CA, nothing in flight.
    Idle,
    /// BEGIN done: both CAs trusted, leaves still under the retiring CA.
    TrustDistributed,
    /// REISSUE done: both CAs trusted, leaves under the new CA.
    LeavesReissued,
}

impl CaRotationPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::TrustDistributed => "trust-distributed",
            Self::LeavesReissued => "leaves-reissued",
        }
    }
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
    let (ca_cert, ca_key) = generate_ca("kubernetes-ca", "kubernetes", CA_VALIDITY_DAYS)?;

    // Generate etcd CA
    let (etcd_ca_cert, etcd_ca_key) = generate_ca("etcd-ca", "etcd", CA_VALIDITY_DAYS)?;

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
        ..Default::default()
    };
    renew_leaf_certificates(&mut pki, cluster_name, api_endpoint, service_cidr, nodes)?;
    Ok(pki)
}

/// Validity of every leaf certificate this module issues.
pub const LEAF_VALIDITY_DAYS: u32 = 365;
/// Validity of the Kubernetes and etcd CAs, at create and at rotation.
pub const CA_VALIDITY_DAYS: u32 = 3650;

/// True between BEGIN and RETIRE of a dual-CA rotation.
pub fn dual_ca_active(pki: &KubernetesPki) -> bool {
    !pki.previous_ca_cert.trim().is_empty()
}

/// What every Kubernetes verifier trusts: the retiring CA then the active
/// one during a rotation, otherwise the active CA unchanged.
pub fn ca_trust_bundle(pki: &KubernetesPki) -> String {
    pem_bundle(&pki.previous_ca_cert, &pki.ca_cert)
}

/// The etcd counterpart of [`ca_trust_bundle`].
pub fn etcd_ca_trust_bundle(pki: &KubernetesPki) -> String {
    pem_bundle(&pki.previous_etcd_ca_cert, &pki.etcd_ca_cert)
}

fn pem_bundle(previous: &str, current: &str) -> String {
    if previous.trim().is_empty() {
        return current.to_string();
    }
    let mut out = previous.trim_end().to_string();
    out.push('\n');
    out.push_str(current);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn ca_rotation_phase(pki: &KubernetesPki) -> CaRotationPhase {
    if !dual_ca_active(pki) {
        CaRotationPhase::Idle
    } else if leaves_under_active_ca(pki) {
        CaRotationPhase::LeavesReissued
    } else {
        CaRotationPhase::TrustDistributed
    }
}

/// Every stored leaf verifies against the active CA it belongs under.
/// etcd leaves are only checked when the rotation replaced the etcd CA.
pub fn leaves_under_active_ca(pki: &KubernetesPki) -> bool {
    let kubernetes = [&pki.apiserver_cert, &pki.apiserver_kubelet_client_cert]
        .iter()
        .all(|leaf| signed_by(leaf, &pki.ca_cert));
    if !kubernetes {
        return false;
    }
    if pki.previous_etcd_ca_cert.trim().is_empty() {
        return true;
    }
    std::iter::once(&pki.apiserver_etcd_client_cert)
        .chain(pki.etcd_node_certs.values().map(|(cert, _)| cert))
        .chain(pki.etcd_node_peer_certs.values().map(|(cert, _)| cert))
        .all(|leaf| signed_by(leaf, &pki.etcd_ca_cert))
}

/// BEGIN: the active CAs become the retiring ones and new CAs take their
/// place. Leaves are left alone, so every node keeps presenting certificates
/// the old trust accepts while the dual bundle rolls out. The service account
/// keys are not CA material and stay as they are.
pub fn begin_ca_rotation(pki: &mut KubernetesPki, include_etcd: bool) -> Result<()> {
    if dual_ca_active(pki) {
        anyhow::bail!("a dual-CA rotation is already in progress");
    }
    let (ca_cert, ca_key) = generate_ca("kubernetes-ca", "kubernetes", CA_VALIDITY_DAYS)?;
    pki.previous_ca_cert = std::mem::replace(&mut pki.ca_cert, ca_cert);
    pki.previous_ca_key = std::mem::replace(&mut pki.ca_key, ca_key);
    if include_etcd {
        let (etcd_ca_cert, etcd_ca_key) = generate_ca("etcd-ca", "etcd", CA_VALIDITY_DAYS)?;
        pki.previous_etcd_ca_cert = std::mem::replace(&mut pki.etcd_ca_cert, etcd_ca_cert);
        pki.previous_etcd_ca_key = std::mem::replace(&mut pki.etcd_ca_key, etcd_ca_key);
    }
    pki.admin_kubeconfig = with_kubeconfig_ca(&pki.admin_kubeconfig, &ca_trust_bundle(pki));
    Ok(())
}

/// RETIRE: drop the retiring CAs once every leaf is under the new ones.
pub fn retire_previous_ca(pki: &mut KubernetesPki) -> Result<()> {
    if !dual_ca_active(pki) {
        anyhow::bail!("no dual-CA rotation is in progress");
    }
    if !leaves_under_active_ca(pki) {
        anyhow::bail!("leaf certificates are still signed by the retiring CA; reissue them first");
    }
    pki.previous_ca_cert.clear();
    pki.previous_ca_key.clear();
    pki.previous_etcd_ca_cert.clear();
    pki.previous_etcd_ca_key.clear();
    pki.admin_kubeconfig = with_kubeconfig_ca(&pki.admin_kubeconfig, &pki.ca_cert);
    Ok(())
}

/// The CA expiry worth warning about: the retiring CA while both are
/// trusted, otherwise the active one.
pub fn ca_not_after_for_notice(pki: &KubernetesPki) -> Option<OffsetDateTime> {
    if dual_ca_active(pki) {
        pem_not_after(&pki.previous_ca_cert)
    } else {
        pem_not_after(&pki.ca_cert)
    }
}

/// The Kubernetes CA that signs leaves issued outside a full reissue (a
/// joining node, a refreshed kubelet kubeconfig). Until REISSUE moves the
/// stored leaves, new ones stay under the retiring CA with them.
fn kubernetes_leaf_signer(pki: &KubernetesPki) -> (&str, &str) {
    if ca_rotation_phase(pki) == CaRotationPhase::TrustDistributed {
        (&pki.previous_ca_cert, &pki.previous_ca_key)
    } else {
        (&pki.ca_cert, &pki.ca_key)
    }
}

fn etcd_leaf_signer(pki: &KubernetesPki) -> (&str, &str) {
    if !pki.previous_etcd_ca_cert.trim().is_empty()
        && ca_rotation_phase(pki) == CaRotationPhase::TrustDistributed
    {
        (&pki.previous_etcd_ca_cert, &pki.previous_etcd_ca_key)
    } else {
        (&pki.etcd_ca_cert, &pki.etcd_ca_key)
    }
}

/// Swap `certificate-authority-data` in a kubeconfig this module wrote,
/// leaving the client credentials as they are.
fn with_kubeconfig_ca(kubeconfig: &str, ca_pem: &str) -> String {
    if kubeconfig.is_empty() {
        return String::new();
    }
    let ca_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        ca_pem.as_bytes(),
    );
    let mut out = String::with_capacity(kubeconfig.len() + ca_b64.len());
    for line in kubeconfig.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("certificate-authority-data:") {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str("certificate-authority-data: ");
            out.push_str(&ca_b64);
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

/// `cert_pem`'s signature verifies with the key in `ca_pem` (first PEM of each).
fn signed_by(cert_pem: &str, ca_pem: &str) -> bool {
    use x509_parser::prelude::FromDer;
    let Ok((_, leaf)) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()) else {
        return false;
    };
    let Ok((_, ca)) = x509_parser::pem::parse_x509_pem(ca_pem.as_bytes()) else {
        return false;
    };
    let Ok((_, leaf)) = x509_parser::certificate::X509Certificate::from_der(&leaf.contents) else {
        return false;
    };
    let Ok((_, ca)) = x509_parser::certificate::X509Certificate::from_der(&ca.contents) else {
        return false;
    };
    leaf.verify_signature(Some(ca.public_key())).is_ok()
}

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
        let (server, peer) = etcd_certs_for_node(&pki.etcd_ca_cert, &pki.etcd_ca_key, node)?;
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
        &ca_trust_bundle(pki),
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

/// Expiry of the first certificate in `pem_str`; later PEM blocks of a
/// trust bundle are ignored.
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
        let (etcd_ca_cert, etcd_ca_key) = etcd_leaf_signer(pki);
        let (etcd_server, etcd_peer) = etcd_certs_for_node(etcd_ca_cert, etcd_ca_key, node)?;
        pki.etcd_node_certs
            .insert(node.node_id.clone(), etcd_server);
        pki.etcd_node_peer_certs
            .insert(node.node_id.clone(), etcd_peer);
    }
    let (ca_cert, ca_key) = kubernetes_leaf_signer(pki);
    let (cert, key) = generate_client_cert(
        &format!("system:node:{}", node.hostname),
        "system:nodes",
        ca_cert,
        ca_key,
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
        &ca_trust_bundle(pki),
        &cert,
        &key,
        &format!("system:node:{}", node.hostname),
    )
}

fn etcd_certs_for_node(
    etcd_ca_cert: &str,
    etcd_ca_key: &str,
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
        etcd_ca_cert,
        etcd_ca_key,
        365,
    )?;
    let peer = generate_server_cert(
        &node.hostname,
        "etcd",
        &sans,
        etcd_ca_cert,
        etcd_ca_key,
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

    fn lab_pki() -> KubernetesPki {
        generate_kubernetes_pki("lab", "10.0.0.1:6443", "10.96.0.0/12", &nodes()).expect("pki")
    }

    fn kubeconfig_ca(kubeconfig: &str) -> String {
        let b64 = kubeconfig
            .lines()
            .find_map(|l| l.trim().strip_prefix("certificate-authority-data: "))
            .expect("ca data");
        String::from_utf8(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn idle_bundle_is_the_active_ca_unchanged() {
        let pki = lab_pki();
        assert_eq!(ca_trust_bundle(&pki), pki.ca_cert);
        assert_eq!(etcd_ca_trust_bundle(&pki), pki.etcd_ca_cert);
        assert_eq!(ca_rotation_phase(&pki), CaRotationPhase::Idle);
        assert!(leaves_under_active_ca(&pki));
    }

    #[test]
    fn begin_keeps_the_old_ca_trusted_and_the_leaves_untouched() {
        let mut pki = lab_pki();
        let before = pki.clone();
        begin_ca_rotation(&mut pki, true).expect("begin");

        assert_eq!(pki.previous_ca_cert, before.ca_cert);
        assert_eq!(pki.previous_ca_key, before.ca_key);
        assert_eq!(pki.previous_etcd_ca_cert, before.etcd_ca_cert);
        assert_eq!(pki.previous_etcd_ca_key, before.etcd_ca_key);
        assert_ne!(pki.ca_cert, before.ca_cert);
        assert_ne!(pki.etcd_ca_cert, before.etcd_ca_cert);
        assert_eq!(pki.apiserver_cert, before.apiserver_cert);
        assert_eq!(pki.etcd_node_certs, before.etcd_node_certs);
        assert_eq!(pki.sa_key, before.sa_key);
        assert_eq!(pki.sa_pub, before.sa_pub);

        let bundle = ca_trust_bundle(&pki);
        assert_eq!(bundle.matches("BEGIN CERTIFICATE").count(), 2);
        assert!(
            bundle.find(before.ca_cert.trim()).unwrap() < bundle.find(pki.ca_cert.trim()).unwrap()
        );
        assert_eq!(
            etcd_ca_trust_bundle(&pki)
                .matches("BEGIN CERTIFICATE")
                .count(),
            2
        );
        assert_eq!(kubeconfig_ca(&pki.admin_kubeconfig), bundle);
        assert_eq!(ca_rotation_phase(&pki), CaRotationPhase::TrustDistributed);
        assert!(
            begin_ca_rotation(&mut pki, true).is_err(),
            "no second begin"
        );
    }

    #[test]
    fn kubernetes_only_rotation_leaves_etcd_alone() {
        let mut pki = lab_pki();
        let before = pki.clone();
        begin_ca_rotation(&mut pki, false).expect("begin");
        assert_eq!(pki.etcd_ca_cert, before.etcd_ca_cert);
        assert!(pki.previous_etcd_ca_cert.is_empty());
        assert_eq!(etcd_ca_trust_bundle(&pki), before.etcd_ca_cert);
    }

    #[test]
    fn new_leaves_follow_the_stored_ones_until_reissue() {
        let mut pki = lab_pki();
        begin_ca_rotation(&mut pki, true).expect("begin");
        let joiner = NodeInfo {
            node_id: "n2".to_string(),
            hostname: "node-2".to_string(),
            ip_address: "10.0.0.3".to_string(),
        };
        let kubelet = issue_node_certificates(&mut pki, "lab", "10.0.0.1:6443", &joiner).unwrap();
        assert!(signed_by(
            &pki.etcd_node_certs["n2"].0,
            &pki.previous_etcd_ca_cert
        ));
        assert_eq!(kubeconfig_ca(&kubelet), ca_trust_bundle(&pki));
        let client_b64 = kubelet
            .lines()
            .find_map(|l| l.trim().strip_prefix("client-certificate-data: "))
            .unwrap();
        let client = String::from_utf8(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, client_b64).unwrap(),
        )
        .unwrap();
        assert!(signed_by(&client, &pki.previous_ca_cert));
        assert!(!signed_by(&client, &pki.ca_cert));
    }

    #[test]
    fn renew_after_begin_moves_every_leaf_to_the_new_ca() {
        let mut pki = lab_pki();
        begin_ca_rotation(&mut pki, true).expect("begin");
        let sa_key = pki.sa_key.clone();
        renew_leaf_certificates(&mut pki, "lab", "10.0.0.1:6443", "10.96.0.0/12", &nodes())
            .expect("reissue");
        assert!(signed_by(&pki.apiserver_cert, &pki.ca_cert));
        assert!(!signed_by(&pki.apiserver_cert, &pki.previous_ca_cert));
        assert!(signed_by(
            &pki.etcd_node_peer_certs["n1"].0,
            &pki.etcd_ca_cert
        ));
        assert_eq!(ca_rotation_phase(&pki), CaRotationPhase::LeavesReissued);
        assert_eq!(kubeconfig_ca(&pki.admin_kubeconfig), ca_trust_bundle(&pki));
        assert_eq!(pki.sa_key, sa_key);
    }

    #[test]
    fn retire_needs_reissued_leaves_then_drops_the_old_ca() {
        let mut pki = lab_pki();
        assert!(retire_previous_ca(&mut pki).is_err(), "nothing to retire");
        let original = pki.clone();
        begin_ca_rotation(&mut pki, true).expect("begin");
        assert!(retire_previous_ca(&mut pki).is_err(), "leaves still old");

        renew_leaf_certificates(&mut pki, "lab", "10.0.0.1:6443", "10.96.0.0/12", &nodes())
            .expect("reissue");
        let new_ca = pki.ca_cert.clone();
        retire_previous_ca(&mut pki).expect("retire");
        assert!(pki.previous_ca_cert.is_empty() && pki.previous_ca_key.is_empty());
        assert!(pki.previous_etcd_ca_cert.is_empty() && pki.previous_etcd_ca_key.is_empty());
        assert_eq!(ca_trust_bundle(&pki), new_ca);
        assert_eq!(kubeconfig_ca(&pki.admin_kubeconfig), new_ca);
        assert_eq!(ca_rotation_phase(&pki), CaRotationPhase::Idle);
        assert_eq!(pki.sa_key, original.sa_key);
        assert_eq!(pki.sa_pub, original.sa_pub);
    }

    #[test]
    fn notice_expiry_reads_the_retiring_ca_and_only_the_first_pem() {
        let mut pki = lab_pki();
        let old = pem_not_after(&pki.ca_cert).unwrap();
        begin_ca_rotation(&mut pki, false).expect("begin");
        assert_eq!(ca_not_after_for_notice(&pki), Some(old));
        assert_eq!(pem_not_after(&ca_trust_bundle(&pki)), Some(old));
        assert_eq!(
            pem_not_after(&format!("{}{}", pki.ca_cert, pki.previous_ca_cert)),
            pem_not_after(&pki.ca_cert)
        );
    }
}
