//! Controller-side GuestOps helpers: collect node pubkeys and drive probe/grow.

use crate::db::Database;
use crate::node_client::NodeClients;
use crate::node_proto;

/// Refresh GuestOps public keys from approved nodes into `guest_ops_node_keys`.
pub async fn sync_guest_ops_keys(db: &Database, clients: &NodeClients) -> Result<usize, String> {
    let nodes = db.list_nodes().map_err(|e| e.to_string())?;
    let mut n = 0usize;
    for node in nodes
        .into_iter()
        .filter(|n| n.approval_status == "approved")
    {
        if clients.get_admin(&node.address).is_none() {
            let _ = clients.connect(&node.address).await;
        }
        let Some(mut admin) = clients.get_admin(&node.address) else {
            continue;
        };
        let resp = match admin
            .ensure_guest_ops_key(node_proto::EnsureGuestOpsKeyRequest {})
            .await
        {
            Ok(r) => r.into_inner(),
            Err(e) => {
                tracing::debug!(node = %node.id, error = %e, "EnsureGuestOpsKey failed");
                continue;
            }
        };
        if resp.success && !resp.public_key.is_empty() {
            db.upsert_guest_ops_key(&node.id, &resp.public_key)
                .map_err(|e| e.to_string())?;
            n += 1;
        }
    }
    Ok(n)
}

/// All GuestOps public keys to inject into cloud-init.
pub fn all_guest_ops_keys(db: &Database) -> Vec<String> {
    db.list_guest_ops_keys()
        .unwrap_or_default()
        .into_iter()
        .map(|(_, pk)| pk)
        .filter(|pk| !pk.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn all_guest_ops_keys_empty_db() {
        let db = Database::open(":memory:").expect("db");
        assert!(all_guest_ops_keys(&db).is_empty());
    }
}
