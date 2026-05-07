use anyhow::Result;

use crate::client::{self, controller_proto as proto};
use crate::config::ConnectionInfo;
use crate::pki;
use crate::OperatorRoleArg;

pub fn role_kind_from_cli(r: OperatorRoleArg) -> proto::OperatorRoleKind {
    match r {
        OperatorRoleArg::ReadOnly => proto::OperatorRoleKind::ReadOnly,
        OperatorRoleArg::Admin => proto::OperatorRoleKind::Admin,
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
