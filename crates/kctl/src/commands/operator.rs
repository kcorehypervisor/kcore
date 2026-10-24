use anyhow::Result;

use crate::client::{self, controller_proto as proto};
use crate::config::ConnectionInfo;
use crate::pki;
use crate::OperatorRoleArg;

pub fn role_kind_from_cli(r: OperatorRoleArg) -> proto::OperatorRoleKind {
    match r {
        OperatorRoleArg::ReadOnly => proto::OperatorRoleKind::ReadOnly,
        OperatorRoleArg::BackupOperator => proto::OperatorRoleKind::BackupOperator,
        OperatorRoleArg::VmAdmin => proto::OperatorRoleKind::VmAdmin,
        OperatorRoleArg::ClusterAdmin => proto::OperatorRoleKind::ClusterAdmin,
    }
}

pub async fn create(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_operator(proto::CreateOperatorRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    if let Some(op) = resp.operator {
        println!("operator '{}' created", op.name);
    }
    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_operator(proto::DeleteOperatorRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    if resp.success {
        println!("operator '{name}' deleted");
    } else {
        println!("operator '{name}' was not found");
    }
    Ok(())
}

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_operators(proto::ListOperatorsRequest {})
        .await?
        .into_inner();
    if resp.operators.is_empty() {
        println!("No operators defined");
        return Ok(());
    }
    println!("{:<20}  {:<30}  ROLES", "NAME", "CERT SERIAL");
    for op in &resp.operators {
        let roles: Vec<_> = op
            .roles
            .iter()
            .map(|r| {
                format!(
                    "{:?}",
                    proto::OperatorRoleKind::try_from(*r)
                        .unwrap_or(proto::OperatorRoleKind::Unspecified)
                )
            })
            .collect();
        println!(
            "{:<20}  {:<30}  {}",
            op.name,
            op.cert_serial,
            roles.join(",")
        );
    }
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_operator(proto::GetOperatorRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    let Some(op) = resp.operator else {
        anyhow::bail!("empty response");
    };
    println!("name:         {}", op.name);
    println!("cert_serial:  {}", op.cert_serial);
    println!(
        "roles:        {}",
        op.roles
            .iter()
            .map(|r| {
                format!(
                    "{:?}",
                    proto::OperatorRoleKind::try_from(*r)
                        .unwrap_or(proto::OperatorRoleKind::Unspecified)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

pub async fn issue_cert(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .issue_operator_cert(proto::IssueOperatorCertRequest {
            operator_name: name.to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        anyhow::bail!("issue_operator_cert failed: {}", resp.message);
    }
    let dir = pki::write_operator_tls_material(name, &resp.cert_pem, &resp.key_pem)
        .map_err(|e| anyhow::anyhow!(e))?;
    println!("wrote operator TLS material under {}", dir.display());
    println!("{}", resp.message);
    Ok(())
}

pub async fn grant_role(info: &ConnectionInfo, name: &str, role: OperatorRoleArg) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let kind = role_kind_from_cli(role);
    let resp = client
        .grant_operator_role(proto::GrantOperatorRoleRequest {
            operator_name: name.to_string(),
            role: kind as i32,
        })
        .await?
        .into_inner();
    if let Some(op) = resp.operator {
        println!("granted role {:?} to '{}'", kind, op.name);
    }
    Ok(())
}

pub async fn revoke_role(info: &ConnectionInfo, name: &str, role: OperatorRoleArg) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let kind = role_kind_from_cli(role);
    let resp = client
        .revoke_operator_role(proto::RevokeOperatorRoleRequest {
            operator_name: name.to_string(),
            role: kind as i32,
        })
        .await?
        .into_inner();
    if resp.operator.is_some() {
        println!("revoked role {:?} from '{}'", kind, name);
    }
    Ok(())
}

struct EchoOff {
    fd: i32,
    original: libc::termios,
}

impl EchoOff {
    fn enter() -> Option<Self> {
        let fd = libc::STDIN_FILENO;
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return None;
        }
        let mut quiet = original;
        quiet.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            return None;
        }
        Some(Self { fd, original })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

fn read_line_quiet(prompt: &str) -> Result<String> {
    use std::io::Write;
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let guard = EchoOff::enter();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if guard.is_some() {
        eprintln!();
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn read_new_password(name: &str) -> Result<String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_string());
    }
    let first = read_line_quiet(&format!("New dashboard password for '{name}': "))?;
    let second = read_line_quiet("Repeat it: ")?;
    if first != second {
        anyhow::bail!("the passwords do not match");
    }
    Ok(first)
}

pub async fn set_password(info: &ConnectionInfo, name: &str) -> Result<()> {
    let password = read_new_password(name)?;
    let mut client = client::auth_client(info).await?;
    let resp = client
        .set_operator_password(proto::SetOperatorPasswordRequest {
            name: name.to_string(),
            password,
        })
        .await?
        .into_inner();
    println!(
        "password set for '{name}' ({} web session(s) ended)",
        resp.sessions_revoked
    );
    Ok(())
}

pub async fn reset_second_factor(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::auth_client(info).await?;
    let resp = client
        .reset_operator_second_factor(proto::ResetOperatorSecondFactorRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    println!(
        "TOTP cleared for '{name}' ({} web session(s) ended); the next dashboard login enrolls again",
        resp.sessions_revoked
    );
    Ok(())
}
