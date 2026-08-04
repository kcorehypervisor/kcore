// Compliance report is allowed for read-only operators (enforced on the controller).

use anyhow::Result;

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
