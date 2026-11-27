use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Deserialize;

use crate::client::{self, controller_proto};
use crate::commands::cluster_update::{self, CanaryArgs};
use crate::commands::node;
use crate::config::{self, ConnectionInfo, Context};
use crate::pki;
use anyhow::{bail, Context as _, Result};

const CONTROLLER_WAIT: Duration = Duration::from_secs(600);
const NODE_WAIT: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterManifest {
    #[serde(default)]
    kind: Option<String>,
    metadata: ClusterMetadata,
    spec: ClusterSpec,
}

#[derive(Debug, Deserialize)]
struct ClusterMetadata {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClusterSpec {
    /// Optional when `nodes` names a controller. Kept for PKI-only manifests.
    controller: Option<String>,
    #[serde(alias = "certs_dir")]
    certs_dir: Option<String>,
    /// Only for `kctl create cluster --force`. Refused on a declarative apply.
    #[serde(default)]
    force: bool,
    version: Option<String>,
    #[serde(default)]
    update: ClusterUpdatePolicy,
    #[serde(default)]
    nodes: Vec<ClusterNodeSpec>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClusterUpdatePolicy {
    #[serde(default)]
    reboot: bool,
    #[serde(default)]
    drain_vms: bool,
    soak: Option<String>,
    #[serde(alias = "canary_node")]
    canary_node: Option<String>,
    #[serde(default, alias = "no_canary")]
    no_canary: bool,
    channel: Option<String>,
    #[serde(alias = "flake_ref")]
    flake_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClusterNodeSpec {
    name: String,
    address: String,
    #[serde(alias = "os_disk")]
    os_disk: String,
    #[serde(default, alias = "data_disks")]
    data_disks: Vec<String>,
    #[serde(default)]
    controller: bool,
    #[serde(alias = "storage_backend")]
    storage_backend: Option<String>,
    #[serde(default, alias = "disable_vxlan")]
    disable_vxlan: bool,
    #[serde(default = "default_dc_id", alias = "dc_id")]
    dc_id: String,
    hostname: Option<String>,
    #[serde(default = "default_true")]
    insecure: bool,
}

fn default_dc_id() -> String {
    "DC1".to_string()
}
fn default_true() -> bool {
    true
}

/// PKI-only apply used by older manifests and tests. Prefer [`apply_from_manifest`].
pub fn create_from_manifest(file: &str, config_path: &Path) -> Result<()> {
    let content = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest: ClusterManifest = serde_yaml::from_str(&content)
        .with_context(|| format!("parsing Cluster manifest {file}"))?;
    if !manifest.spec.nodes.is_empty() || manifest.spec.version.is_some() {
        bail!(
            "this Cluster manifest has nodes or a version; use `kctl apply -f` (async apply path)"
        );
    }
    let controller = manifest
        .spec
        .controller
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("spec.controller is required for a PKI-only Cluster"))?;
    let context_name = manifest.metadata.name;
    let certs_dir = resolve_certs_dir(&manifest.spec, &context_name, config_path);
    create(
        config_path,
        controller,
        &certs_dir,
        &context_name,
        manifest.spec.force,
    )
}

/// Idempotent Cluster apply: ensure PKI, install missing live-ISO nodes, set the version.
pub async fn apply_from_manifest(file: &str, config_path: &Path, dry_run: bool) -> Result<()> {
    let content = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest: ClusterManifest = serde_yaml::from_str(&content)
        .with_context(|| format!("parsing Cluster manifest {file}"))?;
    if manifest.spec.force {
        bail!("spec.force is refused on a declarative Cluster apply; use `kctl create cluster --force` to rotate the CA");
    }

    let context_name = manifest.metadata.name.clone();
    let certs_dir = resolve_certs_dir(&manifest.spec, &context_name, config_path);
    let controller_addr = resolve_controller_addr(&manifest.spec)?;

    if dry_run {
        return print_dry_run(&manifest, &controller_addr, &certs_dir);
    }

    ensure_pki(config_path, &controller_addr, &certs_dir, &context_name)?;

    let mut controller_nodes: Vec<&ClusterNodeSpec> = manifest
        .spec
        .nodes
        .iter()
        .filter(|n| n.controller)
        .collect();
    let agent_nodes: Vec<&ClusterNodeSpec> = manifest
        .spec
        .nodes
        .iter()
        .filter(|n| !n.controller)
        .collect();
    controller_nodes.sort_by(|a, b| a.name.cmp(&b.name));

    for (index, node) in controller_nodes.iter().enumerate() {
        apply_node(
            config_path,
            &certs_dir,
            &context_name,
            node,
            &controller_join_addrs(&manifest.spec.nodes, &node.name),
            index == 0,
        )
        .await?;
    }
    for node in agent_nodes {
        apply_node(
            config_path,
            &certs_dir,
            &context_name,
            node,
            &controller_join_addrs(&manifest.spec.nodes, ""),
            false,
        )
        .await?;
    }

    if let Some(version) = manifest.spec.version.as_deref() {
        let info = config::resolve_controller(config_path, &[], false, None, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let policy = &manifest.spec.update;
        cluster_update::set_version(
            &info,
            version,
            policy.flake_ref.as_deref().unwrap_or(""),
            policy.channel.as_deref().unwrap_or(""),
            policy.drain_vms,
            false,
            CanaryArgs {
                soak: policy.soak.as_deref(),
                node: policy.canary_node.as_deref(),
                disabled: policy.no_canary,
            },
            policy.reboot,
        )
        .await?;
    }
    Ok(())
}

fn print_dry_run(manifest: &ClusterManifest, controller: &str, certs_dir: &Path) -> Result<()> {
    println!("--- dry run ---");
    println!("cluster: {}", manifest.metadata.name);
    println!("controller: {controller}");
    println!("certsDir: {}", certs_dir.display());
    if certs_complete(certs_dir) {
        println!("pki: unchanged (already present)");
    } else {
        println!("pki: would create");
    }
    for node in &manifest.spec.nodes {
        let role = if node.controller {
            "controller"
        } else {
            "agent"
        };
        println!(
            "node {}: {} disk {} ({role}) — would probe then install or skip",
            node.name, node.address, node.os_disk
        );
    }
    if let Some(version) = &manifest.spec.version {
        println!(
            "version: would set {version} (reboot={}, drainVms={})",
            manifest.spec.update.reboot, manifest.spec.update.drain_vms
        );
    }
    println!("--- end ---");
    Ok(())
}

fn resolve_certs_dir(spec: &ClusterSpec, context_name: &str, config_path: &Path) -> PathBuf {
    spec.certs_dir
        .as_ref()
        .map(|p| {
            if let Some(rest) = p.strip_prefix("~/") {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(rest)
            } else {
                PathBuf::from(p)
            }
        })
        .unwrap_or_else(|| default_cluster_certs_dir_with_fallback(context_name, config_path))
}

fn resolve_controller_addr(spec: &ClusterSpec) -> Result<String> {
    if let Some(c) = spec.controller.as_ref().filter(|s| !s.trim().is_empty()) {
        return Ok(normalize_controller(c));
    }
    let first = spec.nodes.iter().find(|n| n.controller).ok_or_else(|| {
        anyhow::anyhow!("spec.controller or a node with controller: true is required")
    })?;
    Ok(format!("{}:9090", host_only(&first.address)?))
}

fn controller_join_addrs(nodes: &[ClusterNodeSpec], exclude_name: &str) -> Vec<String> {
    nodes
        .iter()
        .filter(|n| n.controller && n.name != exclude_name)
        .map(|n| {
            host_only(&n.address)
                .map(|h| format!("{h}:9090"))
                .unwrap_or_else(|_| n.address.clone())
        })
        .collect()
}

fn host_only(address: &str) -> Result<String> {
    pki::host_from_address(address).map_err(|e| anyhow::anyhow!("{e}"))
}

fn normalize_controller(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.contains(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:9090")
    }
}

fn normalize_node_address(address: &str) -> String {
    let trimmed = address.trim();
    if trimmed.contains(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:9091")
    }
}

fn default_cluster_certs_dir_with_fallback(context_name: &str, config_path: &Path) -> PathBuf {
    let preferred = config::default_cluster_certs_dir(context_name);
    if std::fs::create_dir_all(&preferred).is_ok() {
        return preferred;
    }

    let fallback_root = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kcore");
    fallback_root.join(context_name)
}

fn certs_complete(certs_dir: &Path) -> bool {
    [
        "ca.crt",
        "ca.key",
        "sub-ca.crt",
        "sub-ca.key",
        "kctl.crt",
        "kctl.key",
    ]
    .iter()
    .all(|name| certs_dir.join(name).is_file())
}

/// Load existing PKI when complete; create when missing. Never regenerates.
fn ensure_pki(
    config_path: &Path,
    controller: &str,
    certs_dir: &Path,
    context_name: &str,
) -> Result<()> {
    if certs_complete(certs_dir) {
        ensure_context(config_path, controller, certs_dir, context_name)?;
        println!("Cluster PKI unchanged in {}", certs_dir.display());
        return Ok(());
    }
    let partial = ["ca.crt", "ca.key", "kctl.crt", "kctl.key"]
        .iter()
        .any(|name| certs_dir.join(name).exists());
    if partial {
        bail!(
            "incomplete certificates in {}; refuse to generate a new CA. Fix or remove the directory",
            certs_dir.display()
        );
    }
    create(config_path, controller, certs_dir, context_name, false)
}

fn ensure_context(
    config_path: &Path,
    controller: &str,
    certs_dir: &Path,
    context_name: &str,
) -> Result<()> {
    let ca_pem = std::fs::read_to_string(certs_dir.join("ca.crt"))?;
    let cert_pem = std::fs::read_to_string(certs_dir.join("kctl.crt"))?;
    let key_pem = std::fs::read_to_string(certs_dir.join("kctl.key"))?;
    let mut cfg = config::load_config(config_path).with_context(|| {
        format!(
            "loading existing config at {} (refusing to overwrite a config that failed to parse)",
            config_path.display()
        )
    })?;
    if let Some(ctx) = cfg.contexts.get(context_name) {
        if let Some(existing) = ctx.ca_data.as_deref() {
            if let Ok(decoded) = BASE64.decode(existing.trim()) {
                if decoded.as_slice() != ca_pem.as_bytes() {
                    bail!(
                        "context '{context_name}' has a different CA than {}; refuse to overwrite",
                        certs_dir.display()
                    );
                }
            }
        }
        // Keep controllers, operator, tls_server_name. Refresh inline PEMs and
        // the primary controller address only when empty.
        let ctx = cfg.contexts.get_mut(context_name).unwrap();
        if ctx.controller.trim().is_empty() {
            ctx.controller = controller.to_string();
        }
        if ctx.controllers.is_empty() {
            ctx.controllers = vec![controller.to_string()];
        }
        ctx.ca_data = Some(BASE64.encode(ca_pem.as_bytes()));
        ctx.cert_data = Some(BASE64.encode(cert_pem.as_bytes()));
        ctx.key_data = Some(BASE64.encode(key_pem.as_bytes()));
    } else {
        cfg.contexts.insert(
            context_name.to_string(),
            Context {
                controller: controller.to_string(),
                controllers: vec![controller.to_string()],
                insecure: false,
                tls_server_name: None,
                ca_data: Some(BASE64.encode(ca_pem.as_bytes())),
                cert_data: Some(BASE64.encode(cert_pem.as_bytes())),
                key_data: Some(BASE64.encode(key_pem.as_bytes())),
                cert: None,
                key: None,
                ca: None,
                operator: None,
            },
        );
    }
    if cfg.current_context.is_none() {
        cfg.current_context = Some(context_name.to_string());
    }
    config::save_config(config_path, &cfg)?;
    Ok(())
}

async fn apply_node(
    config_path: &Path,
    certs_dir: &Path,
    context_name: &str,
    node: &ClusterNodeSpec,
    join_controllers: &[String],
    bootstrap: bool,
) -> Result<()> {
    let address = normalize_node_address(&node.address);
    if node_registered(config_path, &node.name, &address).await? {
        println!("node {}: already registered; skip", node.name);
        return Ok(());
    }
    if !live_iso_reachable(&address, node.insecure).await? {
        bail!(
            "node {} at {address} is not registered and the live ISO agent did not answer; boot it from the kcore ISO",
            node.name
        );
    }
    println!("node {}: installing from live ISO", node.name);
    let node_info = ConnectionInfo {
        address: address.clone(),
        addresses: vec![address.clone()],
        insecure: node.insecure,
        tls_server_name: None,
        cert_pem: None,
        key_pem: None,
        ca_pem: None,
        cert: None,
        key: None,
        ca: None,
    };
    let controller_info = config::resolve_controller(config_path, &[], false, None, None).ok();
    let joins = if bootstrap {
        Vec::new()
    } else {
        join_controllers.to_vec()
    };
    // Point current context at this cluster so install can append controllers.
    {
        let mut cfg = config::load_config(config_path)?;
        cfg.current_context = Some(context_name.to_string());
        config::save_config(config_path, &cfg)?;
    }
    node::install(
        &node_info,
        controller_info.as_ref(),
        &node.os_disk,
        node.data_disks.clone(),
        &joins,
        node.controller,
        "filesystem",
        node.storage_backend.as_deref(),
        None,
        None,
        None,
        None,
        certs_dir,
        node.disable_vxlan,
        &node.dc_id,
        node.hostname.as_deref(),
        Some(&node.name),
        config_path,
    )
    .await?;
    if node.controller {
        wait_for_controller(config_path, &address).await?;
    } else {
        wait_for_registration(config_path, &node.name, &address).await?;
    }
    Ok(())
}

async fn node_registered(config_path: &Path, name: &str, address: &str) -> Result<bool> {
    let Ok(info) = config::resolve_controller(config_path, &[], false, None, None) else {
        return Ok(false);
    };
    let Ok(mut client) = client::controller_client(&info).await else {
        return Ok(false);
    };
    let Ok(resp) = client
        .list_nodes(controller_proto::ListNodesRequest {})
        .await
    else {
        return Ok(false);
    };
    let host = host_only(address).unwrap_or_default();
    Ok(resp.into_inner().nodes.iter().any(|n| {
        n.node_id == name
            || n.hostname == name
            || n.address == address
            || n.address.starts_with(&format!("{host}:"))
    }))
}

async fn live_iso_reachable(address: &str, insecure: bool) -> Result<bool> {
    let info = ConnectionInfo {
        address: address.to_string(),
        addresses: vec![address.to_string()],
        insecure,
        tls_server_name: None,
        cert_pem: None,
        key_pem: None,
        ca_pem: None,
        cert: None,
        key: None,
        ca: None,
    };
    match tokio::time::timeout(Duration::from_secs(8), async {
        let mut client = client::node_admin_client(&info).await?;
        client
            .list_disks(crate::client::node_proto::ListDisksRequest {})
            .await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    {
        Ok(Ok(())) => Ok(true),
        _ => Ok(false),
    }
}

async fn wait_for_controller(config_path: &Path, node_address: &str) -> Result<()> {
    let host = host_only(node_address)?;
    let controller = format!("{host}:9090");
    let deadline = tokio::time::Instant::now() + CONTROLLER_WAIT;
    println!("waiting for controller at {controller}");
    loop {
        if tokio::time::Instant::now() > deadline {
            bail!("controller at {controller} did not become ready within {CONTROLLER_WAIT:?}");
        }
        if let Ok(info) = config::resolve_controller(
            config_path,
            std::slice::from_ref(&controller),
            false,
            None,
            None,
        ) {
            if client::controller_client(&info).await.is_ok() {
                if let Ok(mut c) = client::controller_client(&info).await {
                    if c.get_cluster_health(controller_proto::GetClusterHealthRequest {})
                        .await
                        .is_ok()
                    {
                        println!("controller {controller} is ready");
                        return Ok(());
                    }
                }
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn wait_for_registration(config_path: &Path, name: &str, address: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + NODE_WAIT;
    println!("waiting for node {name} to register");
    loop {
        if tokio::time::Instant::now() > deadline {
            bail!("node {name} did not register within {NODE_WAIT:?}");
        }
        if node_registered(config_path, name, address).await? {
            println!("node {name} registered");
            return Ok(());
        }
        tokio::time::sleep(POLL).await;
    }
}

pub fn create(
    config_path: &Path,
    controller: &str,
    certs_dir: &Path,
    context_name: &str,
    force: bool,
) -> Result<()> {
    if !force {
        if let Ok(existing_cfg) = config::load_config(config_path) {
            if let Some(ctx) = existing_cfg.contexts.get(context_name) {
                let has_creds = ctx.ca_data.is_some()
                    || ctx.ca.is_some()
                    || ctx.cert_data.is_some()
                    || ctx.cert.is_some()
                    || ctx.key_data.is_some()
                    || ctx.key.is_some();
                if has_creds {
                    anyhow::bail!(
                        "context '{}' already has TLS credentials in {}. \
                         Re-running `create cluster` would generate a new CA and \
                         break connections to any controllers using the current trust root. \
                         Use --force to overwrite deliberately.",
                        context_name,
                        config_path.display()
                    );
                }
            }
        }
    }

    let controller_host = pki::host_from_address(controller)
        .map_err(|e| anyhow::anyhow!("invalid controller: {e}"))?;
    let pki_paths = pki::create_cluster_pki(certs_dir, &controller_host, force)
        .map_err(|e| anyhow::anyhow!("creating cluster PKI: {e}"))?;

    let ca_pem = std::fs::read_to_string(&pki_paths.ca_cert)?;
    let cert_pem = std::fs::read_to_string(&pki_paths.kctl_cert)?;
    let key_pem = std::fs::read_to_string(&pki_paths.kctl_key)?;

    let mut cfg = config::load_config(config_path).with_context(|| {
        format!(
            "loading existing config at {} (refusing to overwrite a config that failed to parse)",
            config_path.display()
        )
    })?;
    cfg.contexts.insert(
        context_name.to_string(),
        Context {
            controller: controller.to_string(),
            controllers: vec![controller.to_string()],
            insecure: false,
            tls_server_name: None,
            ca_data: Some(BASE64.encode(ca_pem.as_bytes())),
            cert_data: Some(BASE64.encode(cert_pem.as_bytes())),
            key_data: Some(BASE64.encode(key_pem.as_bytes())),
            cert: None,
            key: None,
            ca: None,
            operator: None,
        },
    );
    cfg.current_context = Some(context_name.to_string());
    config::save_config(config_path, &cfg)?;

    println!("Cluster PKI generated in {}", pki_paths.certs_dir.display());
    println!(
        "Context '{}' saved to {} (cert data embedded inline)",
        context_name,
        config_path.display()
    );
    println!(
        "Controller certificate: {}",
        pki_paths.controller_cert.display()
    );
    println!("Controller key: {}", pki_paths.controller_key.display());
    println!("CA certificate: {}", pki_paths.ca_cert.display());
    println!("mTLS is now the default for this context.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_writes_context_with_inline_data() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let certs_dir = temp.path().join("certs");

        create(&config_path, "127.0.0.1:9090", &certs_dir, "test", false).expect("create cluster");

        let cfg = config::load_config(&config_path).expect("load config");
        let ctx = cfg.contexts.get("test").expect("context");
        assert_eq!(ctx.controller, "127.0.0.1:9090");

        assert!(ctx.ca_data.is_some(), "ca-data must be embedded inline");
        assert!(ctx.cert_data.is_some(), "cert-data must be embedded inline");
        assert!(ctx.key_data.is_some(), "key-data must be embedded inline");

        assert!(
            ctx.ca.is_none(),
            "file path should not be set when inline data is used"
        );
        assert!(ctx.cert.is_none());
        assert!(ctx.key.is_none());

        let ca_decoded = BASE64
            .decode(ctx.ca_data.as_ref().unwrap().trim())
            .expect("decode ca");
        let ca_str = String::from_utf8(ca_decoded).expect("utf8");
        assert!(
            ca_str.contains("BEGIN CERTIFICATE"),
            "decoded ca must be PEM"
        );

        assert!(certs_dir.join("controller.crt").exists());
        assert!(certs_dir.join("controller.key").exists());
    }

    #[test]
    fn create_refuses_overwrite_without_force() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let certs_dir = temp.path().join("certs");

        create(&config_path, "127.0.0.1:9090", &certs_dir, "test", false).expect("first create");
        let err = create(&config_path, "127.0.0.1:9090", &certs_dir, "test", false)
            .expect_err("should refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already has TLS credentials"),
            "should warn about existing trust root: {msg}"
        );
    }

    #[test]
    fn create_refuses_overwrite_of_context_with_inline_data() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let certs_dir_1 = temp.path().join("certs1");
        let certs_dir_2 = temp.path().join("certs2");

        create(&config_path, "127.0.0.1:9090", &certs_dir_1, "prod", false).expect("first create");

        let err = create(&config_path, "10.0.0.1:9090", &certs_dir_2, "prod", false)
            .expect_err("should refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already has TLS credentials") && msg.contains("--force"),
            "should mention --force to override: {msg}"
        );
    }

    #[test]
    fn create_with_force_overwrites_context() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let certs_dir_1 = temp.path().join("certs1");
        let certs_dir_2 = temp.path().join("certs2");

        create(&config_path, "127.0.0.1:9090", &certs_dir_1, "prod", false).expect("first create");
        create(&config_path, "10.0.0.1:9090", &certs_dir_2, "prod", true)
            .expect("force overwrite should succeed");

        let cfg = config::load_config(&config_path).expect("load config");
        let ctx = cfg.contexts.get("prod").expect("context");
        assert_eq!(ctx.controller, "10.0.0.1:9090");
    }

    #[test]
    fn create_from_manifest_parses_yaml() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let manifest_path = temp.path().join("cluster.yaml");
        let certs_dir = temp.path().join("my-certs");

        std::fs::write(
            &manifest_path,
            format!(
                "kind: Cluster\nmetadata:\n  name: staging\nspec:\n  controller: 127.0.0.1:9090\n  certsDir: {}\n",
                certs_dir.display()
            ),
        )
        .expect("write manifest");

        create_from_manifest(manifest_path.to_str().unwrap(), &config_path)
            .expect("create_from_manifest");

        let cfg = config::load_config(&config_path).expect("load config");
        let ctx = cfg.contexts.get("staging").expect("context");
        assert_eq!(ctx.controller, "127.0.0.1:9090");
        assert!(ctx.ca_data.is_some());
        assert!(certs_dir.join("controller.crt").exists());
    }

    #[test]
    fn create_from_manifest_defaults_certs_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let manifest_path = temp.path().join("cluster.yaml");

        std::fs::write(
            &manifest_path,
            "kind: Cluster\nmetadata:\n  name: testctx\nspec:\n  controller: 127.0.0.1:9090\n  force: true\n",
        )
        .expect("write manifest");

        create_from_manifest(manifest_path.to_str().unwrap(), &config_path)
            .expect("create_from_manifest");

        let cfg = config::load_config(&config_path).expect("load config");
        assert!(cfg.contexts.contains_key("testctx"));
    }

    #[test]
    fn create_refuses_when_existing_config_is_unparseable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let certs_dir = temp.path().join("certs");

        std::fs::write(&config_path, "this: is: not valid: yaml: at all\n  - [\n")
            .expect("write broken config");

        let err = create(&config_path, "127.0.0.1:9090", &certs_dir, "fresh", true)
            .expect_err("must refuse to overwrite a corrupt config");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("loading existing config"),
            "should mention the load failure: {msg}"
        );

        let on_disk = std::fs::read_to_string(&config_path).expect("file must still exist");
        assert!(
            on_disk.contains("not valid"),
            "the corrupt config must be left untouched, got: {on_disk}"
        );
    }

    #[test]
    fn create_from_manifest_supports_snake_case() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let manifest_path = temp.path().join("cluster.yaml");
        let certs_dir = temp.path().join("snake-certs");

        std::fs::write(
            &manifest_path,
            format!(
                "kind: Cluster\nmetadata:\n  name: snaketest\nspec:\n  controller: 127.0.0.1:9090\n  certs_dir: {}\n",
                certs_dir.display()
            ),
        )
        .expect("write manifest");

        create_from_manifest(manifest_path.to_str().unwrap(), &config_path)
            .expect("create_from_manifest with snake_case");

        let cfg = config::load_config(&config_path).expect("load config");
        assert!(cfg.contexts.contains_key("snaketest"));
    }

    #[test]
    fn declarative_manifest_parses_version_and_nodes() {
        let yaml = r#"
kind: Cluster
metadata:
  name: prod
spec:
  version: v0.9.0
  update:
    reboot: true
    drainVms: true
    soak: 1h
    canaryNode: node-105
  nodes:
    - name: node-107
      address: 192.168.40.107
      osDisk: /dev/sda
      controller: true
    - name: node-105
      address: 192.168.40.105
      osDisk: /dev/sda
"#;
        let manifest: ClusterManifest = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(manifest.spec.version.as_deref(), Some("v0.9.0"));
        assert!(manifest.spec.update.reboot);
        assert!(manifest.spec.update.drain_vms);
        assert_eq!(manifest.spec.nodes.len(), 2);
        assert!(manifest.spec.nodes[0].controller);
        assert_eq!(
            resolve_controller_addr(&manifest.spec).unwrap(),
            "192.168.40.107:9090"
        );
    }

    #[test]
    fn declarative_manifest_rejects_typos() {
        let yaml = r#"
kind: Cluster
metadata:
  name: prod
spec:
  version: v0.9.0
  update:
    rebot: true
"#;
        let err = serde_yaml::from_str::<ClusterManifest>(yaml).expect_err("typo");
        assert!(format!("{err}").contains("rebot") || format!("{err}").contains("unknown"));
    }

    #[test]
    fn ensure_pki_is_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        let certs_dir = temp.path().join("certs");
        create(&config_path, "127.0.0.1:9090", &certs_dir, "prod", false).expect("create");
        let ca_before = std::fs::read_to_string(certs_dir.join("ca.crt")).unwrap();
        ensure_pki(&config_path, "127.0.0.1:9090", &certs_dir, "prod").expect("ensure");
        let ca_after = std::fs::read_to_string(certs_dir.join("ca.crt")).unwrap();
        assert_eq!(ca_before, ca_after);
    }

    #[test]
    fn shipped_examples_parse() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/cluster");
        let mut found = 0usize;
        for entry in std::fs::read_dir(&dir).expect("examples/cluster") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read");
            serde_yaml::from_str::<ClusterManifest>(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            found += 1;
        }
        assert!(found > 0, "expected at least one cluster example");
    }
}
