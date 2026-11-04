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

use crate::config::ConnectionInfo;

#[derive(Args, Debug)]
pub struct AgentArgs {
    /// Port for the local UI (0 picks a free one)
    #[arg(long, default_value_t = 0)]
    pub port: u16,

    /// Print the URL but do not open a browser
    #[arg(long)]
    pub no_browser: bool,

    /// Path to the kcore-agent executable (or its dist/index.js)
    #[arg(long, value_name = "PATH")]
    pub agent_bin: Option<PathBuf>,

    /// kcore-provisioning address (host:port) to give the agent for provisioning RPCs
    #[arg(long, value_name = "HOST:PORT")]
    pub provisioning: Option<String>,
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(info: &ConnectionInfo, args: &AgentArgs) -> Result<()> {
    let agent = resolve_agent(args.agent_bin.as_deref())?;
    let context = context_yaml(info, args.provisioning.as_deref())?;

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
        .env_remove("KCORE_AGENT_BIND")
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
            if let Some(port) = parse_listening_line(&line) {
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

    let url = agent_url(port, &token);
    println!("kcore agent is running at http://127.0.0.1:{port}/");
    println!("Open this link once; it carries the session token: {url}");
    println!("Press Ctrl-C to stop.");
    if !args.no_browser {
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
    resolve_agent_from(
        flag,
        env_value.as_deref(),
        exe_dir.as_deref(),
        cwd.as_deref(),
    )
}

fn resolve_agent_from(
    flag: Option<&Path>,
    env_value: Option<&Path>,
    exe_dir: Option<&Path>,
    cwd: Option<&Path>,
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
        "could not find kcore-agent. Tried: {}. Build it with `npm run build` in kcore-mcp, \
         or pass --agent-bin / set KCORE_AGENT_BIN.",
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

fn parse_listening_line(line: &str) -> Option<u16> {
    line.trim()
        .strip_prefix("listening http://127.0.0.1:")?
        .parse()
        .ok()
}

fn agent_url(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/#token={token}")
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

    #[test]
    fn listening_line_is_parsed_only_for_loopback() {
        assert_eq!(
            parse_listening_line("listening http://127.0.0.1:41233"),
            Some(41233)
        );
        assert_eq!(parse_listening_line("listening http://0.0.0.0:41233"), None);
        assert_eq!(parse_listening_line("something else"), None);
    }

    #[test]
    fn the_token_goes_in_the_fragment_not_the_query() {
        let url = agent_url(8123, "abc");
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

        let by_flag = resolve_agent_from(Some(&flag), None, Some(&sibling_dir), None).unwrap();
        assert_eq!(by_flag.program, flag);
        let by_sibling = resolve_agent_from(None, None, Some(&sibling_dir), None).unwrap();
        assert_eq!(by_sibling.program, sibling_dir.join("kcore-agent"));
        assert!(resolve_agent_from(Some(&dir.path().join("nope")), None, None, None).is_err());

        let repo = dir.path().join("repo");
        let built = repo.join("kcore-mcp/packages/agent/dist");
        fs::create_dir_all(&built).unwrap();
        fs::write(built.join("index.js"), "").unwrap();
        let target = repo.join("target/debug");
        fs::create_dir_all(&target).unwrap();
        let by_repo = resolve_agent_from(None, None, Some(&target), None).unwrap();
        assert_eq!(by_repo.program, PathBuf::from("node"));
        assert_eq!(by_repo.args, vec![built.join("index.js")]);

        let empty = dir.path().join("empty/bin");
        fs::create_dir_all(&empty).unwrap();
        let from_cwd =
            resolve_agent_from(None, None, None, Some(&repo.join("crates/kctl"))).unwrap();
        assert_eq!(from_cwd.args, vec![built.join("index.js")]);

        let err = resolve_agent_from(None, None, Some(&empty), None)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("could not find kcore-agent"), "{err}");
    }
}
