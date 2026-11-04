//! `kctl agent`: a local chat UI whose model drives the kcore tools with this context's certificate.
//!
//! Everything runs inside this process: the HTTP server on 127.0.0.1, the model providers, and
//! the tools. The private key never leaves it and the browser only ever sees a random session token.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Args;

use crate::agent::auth::{AuthOptions, AuthStore};
use crate::agent::http::{self, AppOptions};
use crate::agent::summary::{cluster_summary, default_home};
use crate::agent::tools::rpc::Endpoints;
use crate::agent::tools::Toolbox;
use crate::agent::util::random_token;
use crate::client::{self, controller_proto as proto};
use crate::config::{self, ConnectionInfo};
use crate::pki;

const RPC_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Args, Debug)]
pub struct AgentArgs {
    /// Port for the local UI (0 picks a free one)
    #[arg(long, default_value_t = 0)]
    pub port: u16,

    /// Print the URL but do not open a browser
    #[arg(long)]
    pub no_browser: bool,

    /// kcore-provisioning address (host:port) for provisioning RPCs (or set KCORE_PROVISIONING)
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

fn agent_url(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/#token={token}")
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

fn provisioning_address(flag: Option<&str>) -> Option<String> {
    flag.map(str::to_string)
        .or_else(|| std::env::var("KCORE_PROVISIONING").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub async fn run(info: &ConnectionInfo, args: &AgentArgs, config_path: &Path) -> Result<()> {
    let scoped;
    let info = match args.role {
        Some(role) => {
            scoped = provision_agent_identity(info, role).await?;
            &scoped
        }
        None => info,
    };

    let home = default_home();
    let token = random_token(32).context("creating the session token")?;
    let toolbox = Arc::new(Toolbox::new(
        Endpoints {
            controller: info.clone(),
            provisioning: provisioning_address(args.provisioning.as_deref()),
            provisioning_tls: false,
            timeout: RPC_TIMEOUT,
        },
        config_path.to_path_buf(),
    ));
    let running = http::start(
        AppOptions {
            token: token.clone(),
            auth: Arc::new(AuthStore::new(home.clone(), AuthOptions::from_process())),
            toolbox,
            log_dir: Some(home.join("sessions")),
            summary: cluster_summary(info),
        },
        args.port,
    )
    .await?;

    let url = agent_url(running.port, &token);
    println!(
        "kcore agent is running at http://127.0.0.1:{}/",
        running.port
    );
    println!("Open this link once; it carries the session token: {url}");
    println!("Press Ctrl-C to stop.");
    if !args.no_browser {
        open_browser(&url);
    }

    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl-C")?;
    running.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_token_goes_in_the_fragment_not_the_query() {
        assert_eq!(agent_url(8123, "abc"), "http://127.0.0.1:8123/#token=abc");
    }

    #[test]
    fn the_provisioning_flag_is_trimmed() {
        assert_eq!(
            provisioning_address(Some(" 10.0.0.1:50051 ")).as_deref(),
            Some("10.0.0.1:50051")
        );
    }
}
