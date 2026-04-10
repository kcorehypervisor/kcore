use anyhow::{bail, Result};

use crate::client::{self, controller_proto as proto};
use crate::config::ConnectionInfo;

pub async fn list(info: &ConnectionInfo, limit: i32) -> Result<()> {
    let mut client = client::controller_admin_client(info).await?;
    let resp = client
        .list_replication_conflicts(proto::ListReplicationConflictsRequest { limit })
        .await?
        .into_inner();

    if resp.conflicts.is_empty() {
        println!("No unresolved replication conflicts");
        return Ok(());
    }

    println!(
        "{:<6}  {:<20}  {:<12}  {:<12}  {:<24}",
        "ID", "RESOURCE", "INCUMBENT", "CHALLENGER", "REASON"
    );
    for c in &resp.conflicts {
        println!(
            "{:<6}  {:<20}  {:<12}  {:<12}  {:<24}",
            c.id,
            c.resource_key,
            c.incumbent_controller_id,
            c.challenger_controller_id,
            truncate(&c.reason, 24)
        );
    }
    Ok(())
}

pub async fn resolve(info: &ConnectionInfo, id: i64) -> Result<()> {
    if id <= 0 {
        bail!("conflict id must be > 0");
    }

    let mut client = client::controller_admin_client(info).await?;
    let resp = client
        .resolve_replication_conflict(proto::ResolveReplicationConflictRequest { id })
        .await?
        .into_inner();
    if !resp.success {
        bail!("resolve conflict request returned unsuccessful status");
    }
    println!("Resolved replication conflict {id}");
    Ok(())
}

pub async fn describe(info: &ConnectionInfo, id: i64) -> Result<()> {
    if id <= 0 {
        bail!("conflict id must be > 0");
    }
    let mut client = client::controller_admin_client(info).await?;
    let resp = client
        .list_replication_conflicts(proto::ListReplicationConflictsRequest { limit: 1000 })
        .await?
        .into_inner();
    let conflict = resp
        .conflicts
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| anyhow::anyhow!("conflict id {id} not found in unresolved conflicts"))?;

    println!("ID:                    {}", conflict.id);
    println!("Resource key:          {}", conflict.resource_key);
    println!("Reason:                {}", conflict.reason);
    println!("Incumbent operation:   {}", conflict.incumbent_op_id);
    println!("Incumbent controller:  {}", conflict.incumbent_controller_id);
    println!("Challenger operation:  {}", conflict.challenger_op_id);
    println!("Challenger controller: {}", conflict.challenger_controller_id);
    Ok(())
}

pub async fn status(info: &ConnectionInfo, require_healthy: bool) -> Result<()> {
    let mut client = client::controller_admin_client(info).await?;
    let resp = client
        .get_replication_status(proto::GetReplicationStatusRequest {})
        .await?
        .into_inner();

    println!("zero_manual_slo_healthy: {}", resp.zero_manual_slo_healthy);
    println!("outbox_head_event_id: {}", resp.outbox_head_event_id);
    println!("outbox_size: {}", resp.outbox_size);
    println!("unresolved_conflicts: {}", resp.unresolved_conflicts);
    println!(
        "oldest_unresolved_conflict_age_seconds: {}",
        resp.oldest_unresolved_conflict_age_seconds
    );
    println!(
        "pending_compensation_jobs: {}",
        resp.pending_compensation_jobs
    );
    println!("failed_compensation_jobs: {}", resp.failed_compensation_jobs);
    println!("materialization_backlog: {}", resp.materialization_backlog);
    println!("failed_reservations: {}", resp.failed_reservations);
    println!(
        "failed_retryable_reservations: {}",
        resp.failed_retryable_reservations
    );
    println!(
        "failed_non_retryable_reservations: {}",
        resp.failed_non_retryable_reservations
    );
    println!(
        "retry_exhausted_reservations: {}",
        resp.retry_exhausted_reservations
    );
    if !resp.zero_manual_slo_violations.is_empty() {
        println!("violations:");
        for reason in resp.zero_manual_slo_violations {
            println!("  - {reason}");
        }
    }

    if require_healthy && !resp.zero_manual_slo_healthy {
        bail!("replication zero-manual SLO is unhealthy");
    }
    Ok(())
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        input.to_string()
    } else {
        let keep = max.saturating_sub(3);
        let mut out = input.chars().take(keep).collect::<String>();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_keeps_short_values() {
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn truncate_shrinks_long_values() {
        let v = truncate("0123456789abcdef", 8);
        assert_eq!(v.chars().count(), 8);
    }
}
