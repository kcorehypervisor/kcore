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
