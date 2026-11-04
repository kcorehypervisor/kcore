//! `kctl agent`: start the local chat UI (`kcore-agent`) with this context's credentials.
//!
//! The agent is a separate Node process. `kctl` resolves the connection the same way every
//! other command does, writes a one-context file under the runtime directory (mode 0600),
//! hands the agent that path plus a random session token, and removes the file on exit.
//! The browser only ever sees the token; the private key stays with the agent process.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use clap::Args;
use serde_yaml::{Mapping, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::client::{self, controller_proto as proto};
use crate::config::{self, ConnectionInfo};
use crate::pki;

#[derive(Args, Debug)]
pub struct AgentArgs {
    /// Port for the local UI (0 picks a free one)
    #[arg(long, default_value_t = 0)]
    pub port: u16,

    /// Serve the UI on this LAN address instead of loopback. The page is plain HTTP and the
    /// session token travels in the link, so anyone who can read that traffic gets this
    /// context's access. Pair it with `--role` and a trusted network.
    #[arg(long, value_name = "IPV4")]
    pub listen: Option<std::net::Ipv4Addr>,

    /// Print the URL but do not open a browser
    #[arg(long)]
    pub no_browser: bool,

    /// Path to the kcore-agent executable (or its dist/index.js)
    #[arg(long, value_name = "PATH")]
    pub agent_bin: Option<PathBuf>,

    /// kcore-provisioning address (host:port) to give the agent for provisioning RPCs
    #[arg(long, value_name = "HOST:PORT")]
    pub provisioning: Option<String>,

    /// Run as a dedicated `agent-<user>` operator limited to this role, created and certified on the first run.
    /// Needs a cluster-admin connection and mTLS. Without it the agent uses this context's identity.
    #[arg(long, value_enum, value_name = "ROLE")]
    pub role: Option<AgentRole>,
}

/// Roles `--role` will give an agent. `cluster-admin` is left out on purpose: use `--as <operator>`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRole {
    #[value(name = "read-only")]
    ReadOnly,
    #[value(name = "vm-admin")]
    VmAdmin,
}

impl AgentRole {
    fn kind(self) -> proto::OperatorRoleKind {
        match self {
            Self::ReadOnly => proto::OperatorRoleKind::ReadOnly,
            Self::VmAdmin => proto::OperatorRoleKind::VmAdmin,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::VmAdmin => "vm-admin",
        }
    }
}

fn rank(kind: proto::OperatorRoleKind) -> u8 {
    match kind {
        proto::OperatorRoleKind::Unspecified => 0,
        proto::OperatorRoleKind::ReadOnly => 1,
        proto::OperatorRoleKind::VmAdmin => 2,
        proto::OperatorRoleKind::ClusterAdmin => 3,
    }
}

/// `agent-<login>`, restricted to what the controller accepts as an operator name.
fn agent_operator_name(login: &str) -> Result<String> {
    let cleaned: String = login
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        bail!("cannot name the agent operator: no usable login name (set USER)");
    }
    let name = format!("agent-{cleaned}");
    Ok(name.chars().take(64).collect())
}

#[derive(Debug, PartialEq, Eq)]
struct Provision {
    create: bool,
    grant: bool,
}

/// What to do so the agent operator holds `role` and nothing stronger.
/// An operator that already holds a stronger role is refused rather than quietly trimmed.
fn plan_provision(
    name: &str,
    existing: Option<&[proto::OperatorRoleKind]>,
    role: AgentRole,
) -> Result<Provision> {
    let Some(held) = existing else {
        return Ok(Provision {
            create: true,
            grant: true,
        });
    };
    if let Some(stronger) = held.iter().find(|r| rank(**r) > rank(role.kind())) {
        bail!(
            "operator '{name}' already holds {stronger:?}, which is stronger than --role {}. \
             Revoke it first with `kctl operator revoke-role {name} --role <role>`, or pick another role.",
            role.name()
        );
    }
    Ok(Provision {
        create: false,
        grant: !held.contains(&role.kind()),
    })
}

/// Make sure `agent-<login>` exists with the role and a certificate on disk, and return a
/// connection that presents that certificate instead of the invoker's.
async fn provision_agent_identity(
    info: &ConnectionInfo,
    role: AgentRole,
) -> Result<ConnectionInfo> {
    if info.insecure {
        bail!("--role needs mTLS; it cannot be combined with -k/--insecure");
    }
    if info.cert_pem.is_some() || info.key_pem.is_some() {
        bail!(
            "--role cannot be used with inline cert-data/key-data in the context; \
             use certificate files so the agent certificate can replace them"
        );
    }
    let login = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let name = agent_operator_name(&login)?;

    let mut controller = client::controller_client(info).await?;
    let listed = controller
        .list_operators(proto::ListOperatorsRequest {})
        .await
        .context("listing operators (--role needs a cluster-admin connection)")?
        .into_inner();
    let existing: Option<Vec<proto::OperatorRoleKind>> = listed
        .operators
        .iter()
        .find(|op| op.name == name)
        .map(|op| {
            op.roles
                .iter()
                .map(|r| {
                    proto::OperatorRoleKind::try_from(*r)
                        .unwrap_or(proto::OperatorRoleKind::Unspecified)
                })
                .collect()
        });
    let plan = plan_provision(&name, existing.as_deref(), role)?;

    if plan.create {
        controller
            .create_operator(proto::CreateOperatorRequest { name: name.clone() })
            .await
            .with_context(|| format!("creating operator '{name}'"))?;
        println!("created operator '{name}'");
    }
    if plan.grant {
        controller
            .grant_operator_role(proto::GrantOperatorRoleRequest {
                operator_name: name.clone(),
                role: role.kind() as i32,
            })
            .await
            .with_context(|| format!("granting {} to '{name}'", role.name()))?;
        println!("granted {} to '{name}'", role.name());
    }

    let dir = config::default_kcore_dir().join("operators").join(&name);
    if !(dir.join("operator.crt").is_file() && dir.join("operator.key").is_file()) {
        let issued = controller
            .issue_operator_cert(proto::IssueOperatorCertRequest {
                operator_name: name.clone(),
            })
            .await
            .with_context(|| format!("issuing a certificate for '{name}'"))?
            .into_inner();
        if !issued.success {
            bail!(
                "issuing a certificate for '{name}' failed: {}",
                issued.message
            );
        }
        let written = pki::write_operator_tls_material(&name, &issued.cert_pem, &issued.key_pem)
            .map_err(|e| anyhow!(e))?;
        println!(
            "wrote the certificate for '{name}' under {}",
            written.display()
        );
    }

    let mut scoped = info.clone();
    config::apply_operator_client_cert(&mut scoped, &name).map_err(|e| anyhow!(e))?;
    println!(
        "the agent runs as operator '{name}' with role {}",
        role.name()
    );
    Ok(scoped)
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(info: &ConnectionInfo, args: &AgentArgs) -> Result<()> {
    let agent = resolve_agent(args.agent_bin.as_deref())?;
    let scoped;
    let info = match args.role {
        Some(role) => {
            scoped = provision_agent_identity(info, role).await?;
            &scoped
        }
        None => info,
    };
    let context = context_yaml(info, args.provisioning.as_deref())?;
    let host = listen_host(args.listen)?;

    let runtime = RuntimeDir::create()?;
    let config_path = runtime.path().join("config");
    write_private(&config_path, context.as_bytes())?;

    let token = random_token()?;

    let mut command = Command::new(&agent.program);
    command
        .args(&agent.args)
        .env("KCORE_CONFIG", &config_path)
        .env("KCORE_AGENT_TOKEN", &token)
        .env("KCORE_AGENT_PORT", args.port.to_string())
        .env("KCORE_AGENT_BIND", &host)
        .env(
            "KCORE_AGENT_ALLOW_REMOTE",
            if host == LOOPBACK { "0" } else { "1" },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", agent.describe()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("agent stdout was not captured"))?;
    let mut lines = BufReader::new(stdout).lines();

    let port = tokio::time::timeout(STARTUP_TIMEOUT, async {
        while let Some(line) = lines.next_line().await? {
            if let Some(port) = parse_listening_line(&line, &host) {
                return Ok(port);
            }
            println!("{line}");
        }
        Err(anyhow!("the agent exited before it started listening"))
    })
    .await
    .map_err(|_| anyhow!("the agent did not start listening within {STARTUP_TIMEOUT:?}"))??;

    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            println!("{line}");
        }
    });

    let url = agent_url(&host, port, &token);
    println!("kcore agent is running at http://{host}:{port}/");
    if host != LOOPBACK {
        eprintln!(
            "warning: listening on {host} over plain HTTP. The session token is in the link; \
             anyone on this network who gets it can use this agent's access."
        );
    }
    println!("Open this link once; it carries the session token: {url}");
    println!("Press Ctrl-C to stop.");
    if !args.no_browser && host == LOOPBACK {
        open_browser(&url);
    }

    tokio::select! {
        status = child.wait() => {
            let status = status?;
            drop(runtime);
            if !status.success() {
                bail!("the agent exited with {status}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            stop(&mut child).await;
            drop(runtime);
        }
    }
    Ok(())
}

struct AgentCommand {
    program: PathBuf,
    args: Vec<PathBuf>,
}

impl AgentCommand {
    fn describe(&self) -> String {
        let mut parts = vec![self.program.display().to_string()];
        parts.extend(self.args.iter().map(|a| a.display().to_string()));
        parts.join(" ")
    }
}

fn resolve_agent(flag: Option<&Path>) -> Result<AgentCommand> {
    let env_value = std::env::var_os("KCORE_AGENT_BIN").map(PathBuf::from);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let cwd = std::env::current_dir().ok();
    let path_var = std::env::var_os("PATH");
    resolve_agent_from(
        flag,
        env_value.as_deref(),
        exe_dir.as_deref(),
        cwd.as_deref(),
        path_var.as_deref(),
    )
}

fn resolve_agent_from(
    flag: Option<&Path>,
    env_value: Option<&Path>,
    exe_dir: Option<&Path>,
    cwd: Option<&Path>,
    path_var: Option<&std::ffi::OsStr>,
) -> Result<AgentCommand> {
    let mut tried: Vec<String> = Vec::new();

    for (label, candidate) in [("--agent-bin", flag), ("KCORE_AGENT_BIN", env_value)] {
        if let Some(path) = candidate {
            if path.is_file() {
                return Ok(command_for(path));
            }
            bail!("{label} points at {}, which is not a file", path.display());
        }
    }

    if let Some(dir) = exe_dir {
        let sibling = dir.join("kcore-agent");
        if sibling.is_file() {
            return Ok(command_for(&sibling));
        }
        tried.push(sibling.display().to_string());
    }

    if let Some(found) = path_var.and_then(|paths| {
        std::env::split_paths(paths)
            .map(|dir| dir.join("kcore-agent"))
            .find(|candidate| candidate.is_file())
    }) {
        return Ok(command_for(&found));
    }
    tried.push("kcore-agent on PATH".to_string());

    for dir in exe_dir.into_iter().chain(cwd) {
        for ancestor in dir.ancestors().take(6) {
            let built = ancestor.join("kcore-mcp/packages/agent/dist/index.js");
            if built.is_file() {
                return Ok(command_for(&built));
            }
        }
        tried.push(format!(
            "kcore-mcp/packages/agent/dist/index.js above {}",
            dir.display()
        ));
    }

    bail!(
        "could not find kcore-agent. Tried: {}. Install it with `npm install -g kcore-agent`, \
         build it with `npm run build` in kcore-mcp, or pass --agent-bin / set KCORE_AGENT_BIN.",
        tried.join(", ")
    )
}

fn command_for(path: &Path) -> AgentCommand {
    if path
        .extension()
        .is_some_and(|ext| ext == "js" || ext == "mjs")
    {
        AgentCommand {
            program: PathBuf::from("node"),
            args: vec![path.to_path_buf()],
        }
    } else {
        AgentCommand {
            program: path.to_path_buf(),
            args: Vec::new(),
        }
    }
}

/// One-context config the agent reads through the same loader the MCP server uses.
fn context_yaml(info: &ConnectionInfo, provisioning: Option<&str>) -> Result<String> {
    let mut ctx = Mapping::new();
    let key = |s: &str| Value::String(s.to_string());

    ctx.insert(key("controller"), Value::String(info.address.clone()));
    ctx.insert(
        key("controllers"),
        Value::Sequence(
            info.addresses
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>(),
        ),
    );
    if let Some(name) = &info.tls_server_name {
        ctx.insert(key("tls-server-name"), Value::String(name.clone()));
    }
    if let Some(addr) = provisioning.map(str::trim).filter(|s| !s.is_empty()) {
        ctx.insert(key("provisioning"), Value::String(addr.to_string()));
    }

    if info.insecure {
        ctx.insert(key("insecure"), Value::Bool(true));
    } else {
        for (field, pem, path) in [
            ("ca-data", &info.ca_pem, &info.ca),
            ("cert-data", &info.cert_pem, &info.cert),
            ("key-data", &info.key_pem, &info.key),
        ] {
            let bytes = pem_bytes(field, pem.as_deref(), path.as_deref())?;
            ctx.insert(key(field), Value::String(BASE64.encode(bytes)));
        }
    }

    let mut contexts = Mapping::new();
    contexts.insert(key("agent"), Value::Mapping(ctx));
    let mut root = Mapping::new();
    root.insert(key("current-context"), Value::String("agent".to_string()));
    root.insert(key("contexts"), Value::Mapping(contexts));
    Ok(serde_yaml::to_string(&Value::Mapping(root))?)
}

fn pem_bytes(field: &str, inline: Option<&str>, path: Option<&str>) -> Result<Vec<u8>> {
    if let Some(pem) = inline.filter(|s| !s.trim().is_empty()) {
        return Ok(pem.as_bytes().to_vec());
    }
    if let Some(path) = path.filter(|s| !s.trim().is_empty()) {
        return fs::read(path).with_context(|| format!("reading {field} from {path}"));
    }
    bail!("no {field} in the selected context; `kctl agent` needs the same credentials as other kctl commands (or pass -k for plaintext)")
}

const LOOPBACK: &str = "127.0.0.1";

/// The address the agent binds. A wildcard would expose every interface, so it is refused.
fn listen_host(listen: Option<std::net::Ipv4Addr>) -> Result<String> {
    match listen {
        None => Ok(LOOPBACK.to_string()),
        Some(ip) if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() => {
            bail!("--listen needs one specific address of this machine, not {ip}")
        }
        Some(ip) => Ok(ip.to_string()),
    }
}

fn parse_listening_line(line: &str, host: &str) -> Option<u16> {
    line.trim()
        .strip_prefix(&format!("listening http://{host}:"))?
        .parse()
        .ok()
}

fn agent_url(host: &str, port: u16, token: &str) -> String {
    format!("http://{host}:{port}/#token={token}")
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .context("reading /dev/urandom for the session token")?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

async fn stop(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: plain signal delivery to a child process this function owns.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(STOP_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

/// `$XDG_RUNTIME_DIR/kcore-agent/<pid>` (or the temp dir), mode 0700, removed on drop.
struct RuntimeDir {
    path: PathBuf,
}

impl RuntimeDir {
    fn create() -> Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir)
            .join("kcore-agent");
        Self::create_in(&base)
    }

    fn create_in(base: &Path) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(base)
            .with_context(|| format!("creating {}", base.display()))?;
        fs::set_permissions(base, fs::Permissions::from_mode(0o700))?;
        let path = base.join(std::process::id().to_string());
        let _ = fs::remove_dir_all(&path);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RuntimeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::os::unix::fs::PermissionsExt;

    fn info() -> ConnectionInfo {
        ConnectionInfo {
            address: "10.0.0.1:9090".into(),
            addresses: vec!["10.0.0.1:9090".into(), "10.0.0.2:9090".into()],
            insecure: false,
            tls_server_name: Some("kcore-controller".into()),
            cert_pem: Some("CERT".into()),
            key_pem: Some("KEY".into()),
            ca_pem: Some("CA".into()),
            cert: None,
            key: None,
            ca: None,
        }
    }

    #[test]
    fn context_round_trips_through_the_kctl_config_loader() {
        let yaml = context_yaml(&info(), Some(" 10.0.0.1:50051 ")).unwrap();
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(cfg.current_context.as_deref(), Some("agent"));
        let ctx = &cfg.contexts["agent"];
        assert_eq!(ctx.controller, "10.0.0.1:9090");
        assert_eq!(ctx.controllers.len(), 2);
        assert_eq!(ctx.tls_server_name.as_deref(), Some("kcore-controller"));
        assert_eq!(
            BASE64.decode(ctx.key_data.as_deref().unwrap()).unwrap(),
            b"KEY"
        );
        assert!(yaml.contains("provisioning: 10.0.0.1:50051"));
        assert!(!yaml.contains("operator"));
    }

    #[test]
    fn insecure_context_carries_no_key_material() {
        let mut i = info();
        i.insecure = true;
        let yaml = context_yaml(&i, None).unwrap();
        assert!(yaml.contains("insecure: true"));
        assert!(!yaml.contains("key-data"));
    }

    #[test]
    fn key_files_are_read_when_the_context_uses_paths() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("k.pem");
        fs::write(&key, "FILEKEY").unwrap();
        let mut i = info();
        i.key_pem = None;
        i.key = Some(key.display().to_string());
        let yaml = context_yaml(&i, None).unwrap();
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            BASE64
                .decode(cfg.contexts["agent"].key_data.as_deref().unwrap())
                .unwrap(),
            b"FILEKEY"
        );
    }

    #[test]
    fn missing_credentials_are_a_clear_error() {
        let mut i = info();
        i.cert_pem = None;
        let err = context_yaml(&i, None).unwrap_err().to_string();
        assert!(err.contains("cert-data"), "{err}");
    }

    use proto::OperatorRoleKind::{ClusterAdmin, ReadOnly, VmAdmin};

    #[test]
    fn agent_operator_names_are_controller_safe() {
        assert_eq!(agent_operator_name("alice").unwrap(), "agent-alice");
        assert_eq!(agent_operator_name("a.b c").unwrap(), "agent-a-b-c");
        assert_eq!(agent_operator_name("..x..").unwrap(), "agent-x");
        assert!(agent_operator_name("").is_err());
        assert!(agent_operator_name("...").is_err());
        let long = agent_operator_name(&"x".repeat(200)).unwrap();
        assert_eq!(long.len(), 64);
    }

    #[test]
    fn a_new_agent_operator_is_created_and_granted() {
        let plan = plan_provision("agent-a", None, AgentRole::VmAdmin).unwrap();
        assert_eq!(
            plan,
            Provision {
                create: true,
                grant: true
            }
        );
    }

    #[test]
    fn an_existing_agent_operator_is_only_granted_what_it_lacks() {
        let none = plan_provision("agent-a", Some(&[]), AgentRole::ReadOnly).unwrap();
        assert_eq!(
            none,
            Provision {
                create: false,
                grant: true
            }
        );
        let same = plan_provision("agent-a", Some(&[VmAdmin]), AgentRole::VmAdmin).unwrap();
        assert_eq!(
            same,
            Provision {
                create: false,
                grant: false
            }
        );
        let up = plan_provision("agent-a", Some(&[ReadOnly]), AgentRole::VmAdmin).unwrap();
        assert_eq!(
            up,
            Provision {
                create: false,
                grant: true
            }
        );
    }

    #[test]
    fn an_agent_operator_with_a_stronger_role_is_refused() {
        let err = plan_provision("agent-a", Some(&[ClusterAdmin]), AgentRole::VmAdmin)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ClusterAdmin") && err.contains("revoke-role"),
            "{err}"
        );
        assert!(plan_provision("agent-a", Some(&[VmAdmin]), AgentRole::ReadOnly).is_err());
    }

    #[tokio::test]
    async fn role_refuses_plaintext_and_inline_keys_before_touching_the_controller() {
        let mut plain = info();
        plain.insecure = true;
        let err = provision_agent_identity(&plain, AgentRole::ReadOnly)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("mTLS"), "{err}");
        let err = provision_agent_identity(&info(), AgentRole::ReadOnly)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("inline"), "{err}");
    }

    #[test]
    fn listening_line_is_parsed_only_for_loopback() {
        assert_eq!(
            parse_listening_line("listening http://127.0.0.1:41233", LOOPBACK),
            Some(41233)
        );
        assert_eq!(
            parse_listening_line("listening http://0.0.0.0:41233", LOOPBACK),
            None
        );
        assert_eq!(parse_listening_line("something else", LOOPBACK), None);
    }

    #[test]
    fn listening_line_must_name_the_requested_address() {
        assert_eq!(
            parse_listening_line("listening http://192.168.40.10:9000", "192.168.40.10"),
            Some(9000)
        );
        assert_eq!(
            parse_listening_line("listening http://127.0.0.1:9000", "192.168.40.10"),
            None
        );
    }

    #[test]
    fn listen_refuses_wildcards_and_defaults_to_loopback() {
        assert_eq!(listen_host(None).unwrap(), "127.0.0.1");
        assert_eq!(
            listen_host(Some("192.168.40.10".parse().unwrap())).unwrap(),
            "192.168.40.10"
        );
        assert!(listen_host(Some("0.0.0.0".parse().unwrap())).is_err());
        assert!(listen_host(Some("255.255.255.255".parse().unwrap())).is_err());
    }

    #[test]
    fn the_token_goes_in_the_fragment_not_the_query() {
        let url = agent_url("127.0.0.1", 8123, "abc");
        assert_eq!(url, "http://127.0.0.1:8123/#token=abc");
    }

    #[test]
    fn tokens_are_64_hex_chars_and_differ() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn runtime_dir_is_private_and_removed_on_drop() {
        let base = tempfile::tempdir().unwrap();
        let runtime = RuntimeDir::create_in(&base.path().join("kcore-agent")).unwrap();
        let path = runtime.path().to_path_buf();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let file = path.join("config");
        write_private(&file, b"x").unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_private(&file, b"y").is_err());
        drop(runtime);
        assert!(!path.exists());
    }

    #[test]
    fn agent_lookup_prefers_flag_then_env_then_sibling_then_repo_build() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("flag-agent");
        let sibling_dir = dir.path().join("bin");
        fs::create_dir_all(&sibling_dir).unwrap();
        fs::write(&flag, "").unwrap();
        fs::write(sibling_dir.join("kcore-agent"), "").unwrap();

        let by_flag =
            resolve_agent_from(Some(&flag), None, Some(&sibling_dir), None, None).unwrap();
        assert_eq!(by_flag.program, flag);
        let by_sibling = resolve_agent_from(None, None, Some(&sibling_dir), None, None).unwrap();
        assert_eq!(by_sibling.program, sibling_dir.join("kcore-agent"));
        assert!(
            resolve_agent_from(Some(&dir.path().join("nope")), None, None, None, None).is_err()
        );

        let repo = dir.path().join("repo");
        let built = repo.join("kcore-mcp/packages/agent/dist");
        fs::create_dir_all(&built).unwrap();
        fs::write(built.join("index.js"), "").unwrap();
        let target = repo.join("target/debug");
        fs::create_dir_all(&target).unwrap();
        let by_repo = resolve_agent_from(None, None, Some(&target), None, None).unwrap();
        assert_eq!(by_repo.program, PathBuf::from("node"));
        assert_eq!(by_repo.args, vec![built.join("index.js")]);

        let empty = dir.path().join("empty/bin");
        fs::create_dir_all(&empty).unwrap();
        let from_cwd =
            resolve_agent_from(None, None, None, Some(&repo.join("crates/kctl")), None).unwrap();
        assert_eq!(from_cwd.args, vec![built.join("index.js")]);

        let on_path = dir.path().join("npm-bin");
        fs::create_dir_all(&on_path).unwrap();
        fs::write(on_path.join("kcore-agent"), "").unwrap();
        let search = std::env::join_paths([dir.path().join("elsewhere"), on_path.clone()]).unwrap();
        let by_path =
            resolve_agent_from(None, None, Some(&empty), None, Some(search.as_os_str())).unwrap();
        assert_eq!(by_path.program, on_path.join("kcore-agent"));

        let err = resolve_agent_from(None, None, Some(&empty), None, None)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("could not find kcore-agent"), "{err}");
    }
}
