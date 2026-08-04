use anyhow::Result;

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

pub async fn list(
    info: &ConnectionInfo,
    limit: u32,
    action: Option<String>,
    since: Option<String>,
) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_audit_events(controller_proto::ListAuditEventsRequest {
            limit,
            since: since.unwrap_or_default(),
            action: action.unwrap_or_default(),
        })
        .await?
        .into_inner();

    if resp.events.is_empty() {
        println!("No audit events");
        return Ok(());
    }

    println!(
        "{:<6}  {:<26}  {:<16}  {:<28}  {:<36}",
        "ID", "TIME", "ACTOR", "ACTION", "RESOURCE"
    );
    for e in &resp.events {
        let detail = if e.detail.is_empty() {
            String::new()
        } else {
            format!("  {}", truncate(&e.detail, 40))
        };
        println!(
            "{:<6}  {:<26}  {:<16}  {:<28}  {:<36}{}",
            e.id,
            truncate(&e.created_at, 26),
            truncate(&e.actor, 16),
            truncate(&e.action, 28),
            truncate(&e.resource, 36),
            detail
        );
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
