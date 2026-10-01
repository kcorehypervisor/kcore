// Compliance report is allowed for read-only operators (enforced on the controller).

use anyhow::{Context, Result};

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;
use crate::output;

pub async fn report(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_compliance_report(controller_proto::GetComplianceReportRequest {})
        .await?
        .into_inner();

    output::print_compliance_report(&resp);
    Ok(())
}

pub async fn crypto_config(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_crypto_config(controller_proto::GetCryptoConfigRequest {})
        .await?
        .into_inner();
    output::print_crypto_config(&resp);
    Ok(())
}

pub async fn export_sbom(
    info: &ConnectionInfo,
    kind: &str,
    output_path: Option<&str>,
) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .export_sbom(controller_proto::ExportSbomRequest {
            kind: kind.to_string(),
        })
        .await?
        .into_inner();
    if let Some(path) = output_path {
        std::fs::write(path, &resp.document).with_context(|| format!("writing SBOM to {path}"))?;
    }
    output::print_sbom_summary(&resp, output_path);
    Ok(())
}
