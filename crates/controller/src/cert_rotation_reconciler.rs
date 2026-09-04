//! Proactive certificate rotation, CRL refresh and expiry warnings.
//!
//! One background loop, in the style of the other reconcilers
//! (`ceph_cluster_reconciler`, `disk_reconciler`), that on every tick:
//!
//! 1. refreshes the in-memory revoked-serial set used by the auth path,
//! 2. regenerates and republishes the CRL when it is due,
//! 3. warns about certificates approaching expiry,
//! 4. asks nodes whose certificate is inside the renewal window to rotate.
//!
//! The controller never signs a replacement behind the node's back: it calls
//! `NodeAdmin.RotateNodeCert`, and the node then generates a keypair, submits
//! a CSR (`Controller.SignNodeCsr`), installs the result and reloads. If any
//! of that fails the node keeps serving its existing certificate and the next
//! tick retries, so a failed rotation is never worse than no rotation.

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use time::{Duration, OffsetDateTime};
use tokio::time as tokio_time;
use tonic::Request;
use tracing::{info, warn};

use crate::config::{CertRotationConfig, PkiConfig};
use crate::db::{Database, IssuedCertRow};
use crate::grpc::SubCaState;
use crate::node_client::NodeClients;
use crate::node_proto;
use crate::pki::crl::{self, CrlCache};
use crate::pki::inventory::KIND_NODE;
use crate::pki::revocation::RevocationState;
use crate::pki::{format_ts, parse_ts};

/// Everything the loop needs, bundled so `main.rs` stays readable.
pub struct CertRotationContext {
    pub db: Database,
    pub clients: NodeClients,
    pub sub_ca: Arc<Mutex<SubCaState>>,
    pub crl_cache: CrlCache,
    pub revocation: RevocationState,
    pub rotation: CertRotationConfig,
    pub pki: PkiConfig,
}

/// Is this certificate inside its renewal window?
///
/// Due when **either** fewer than `renew_before_days` remain **or** less than
/// `lifetime_fraction` of the total lifetime remains. The second rule is what
/// makes short-lived certificates rotate sanely; the first is the absolute
/// floor operators think in.
pub fn is_due_for_renewal(
    not_before: OffsetDateTime,
    not_after: OffsetDateTime,
    now: OffsetDateTime,
    renew_before_days: i64,
    lifetime_fraction: f64,
) -> bool {
    if now >= not_after {
        return true;
    }
    let remaining = not_after - now;
    if remaining <= Duration::days(renew_before_days) {
        return true;
    }
    let lifetime = not_after - not_before;
    if lifetime <= Duration::ZERO {
        return true;
    }
    let remaining_fraction = remaining.as_seconds_f64() / lifetime.as_seconds_f64();
    remaining_fraction < lifetime_fraction
}

/// [`is_due_for_renewal`] for an inventory row. Rows with unparseable
/// timestamps are treated as not due — the rotation path needs a trustworthy
/// validity window, and a bad row is surfaced by the warning pass instead.
pub fn row_is_due(row: &IssuedCertRow, now: OffsetDateTime, cfg: &CertRotationConfig) -> bool {
    match (parse_ts(&row.not_before), parse_ts(&row.not_after)) {
        (Some(nb), Some(na)) => is_due_for_renewal(
            nb,
            na,
            now,
            cfg.renew_before_days,
            cfg.renew_at_lifetime_fraction,
        ),
        _ => false,
    }
}

pub fn spawn_cert_rotation_reconciler(ctx: CertRotationContext) {
    let interval = StdDuration::from_secs(ctx.rotation.check_interval_secs.max(1));
    tokio::spawn(async move {
        // Publish revocation data and a CRL before the first sleep so a
        // freshly started controller enforces revocation immediately.
        reconcile_once(&ctx).await;
        let mut ticker = tokio_time::interval(interval);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            reconcile_once(&ctx).await;
        }
    });
}

async fn reconcile_once(ctx: &CertRotationContext) {
    if let Err(error) = ctx.revocation.refresh(&ctx.db) {
        warn!(%error, "failed to refresh revoked serial set");
    }

    let sub_ca = match ctx.sub_ca.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => {
            warn!("sub-CA lock poisoned; skipping certificate reconcile tick");
            return;
        }
    };
    if let Err(error) = crl::ensure_current(
        &ctx.db,
        &sub_ca,
        &ctx.crl_cache,
        Duration::hours(ctx.pki.crl_validity_hours),
        Duration::hours(ctx.pki.crl_refresh_before_hours),
        false,
    ) {
        warn!(%error, "failed to regenerate CRL");
    }

    warn_about_expiring_certificates(&ctx.db, &ctx.rotation);

    if !ctx.rotation.enabled {
        return;
    }
    if !sub_ca.is_available() {
        warn!("certificate rotation is enabled but no sub-CA is configured; cannot renew node certificates");
        return;
    }

    for node_id in nodes_due_for_rotation(&ctx.db, &ctx.rotation) {
        match rotate_node(&ctx.db, &ctx.clients, &node_id, false).await {
            Ok(result) if result.skipped => {
                info!(node_id = %node_id, "node reports certificate is not due for rotation yet");
            }
            Ok(result) => {
                info!(
                    node_id = %node_id,
                    serial = %result.serial_hex,
                    days_until_expiry = result.days_until_expiry,
                    "node rotated its certificate"
                );
            }
            Err(error) => {
                warn!(
                    node_id = %node_id,
                    %error,
                    "node certificate rotation failed; the existing certificate stays in service and the next tick retries"
                );
            }
        }
    }
}

/// Log a warning for every active certificate inside the warning window, so
/// expiry is visible in the controller journal and not only via `kctl`.
fn warn_about_expiring_certificates(db: &Database, cfg: &CertRotationConfig) {
    let now = OffsetDateTime::now_utc();
    let threshold = format_ts(now + Duration::days(cfg.warn_before_days));
    let rows = match db.list_issued_certificates(crate::db::CERT_STATUS_ACTIVE, "", &threshold) {
        Ok(rows) => rows,
        Err(error) => {
            warn!(%error, "failed to list certificates for expiry warnings");
            return;
        }
    };
    for row in rows {
        let days = parse_ts(&row.not_after)
            .map(|na| crate::pki::days_until(na, now))
            .unwrap_or(i32::MIN);
        if days < 0 {
            warn!(
                serial = %row.serial_hex,
                subject = %row.subject_cn,
                node_id = %row.node_id,
                not_after = %row.not_after,
                "certificate has EXPIRED"
            );
        } else {
            warn!(
                serial = %row.serial_hex,
                subject = %row.subject_cn,
                node_id = %row.node_id,
                days_until_expiry = days,
                "certificate expires soon"
            );
        }
    }
}

/// Node ids whose certificate is due for renewal.
///
/// Two sources, because not every node has an inventory row: certificates
/// issued before this feature existed are only known through the expiry the
/// node reports at registration and on every heartbeat.
pub fn nodes_due_for_rotation(db: &Database, cfg: &CertRotationConfig) -> Vec<String> {
    let now = OffsetDateTime::now_utc();
    let mut due: Vec<String> = Vec::new();

    match db.list_issued_certificates(crate::db::CERT_STATUS_ACTIVE, "", "") {
        Ok(rows) => {
            for row in rows {
                if row.identity_kind != KIND_NODE || row.node_id.is_empty() {
                    continue;
                }
                if row_is_due(&row, now, cfg) && !due.contains(&row.node_id) {
                    due.push(row.node_id);
                }
            }
        }
        Err(error) => warn!(%error, "failed to list certificates for rotation"),
    }

    match db.list_nodes() {
        Ok(nodes) => {
            for node in nodes {
                if node.approval_status != "approved" || due.contains(&node.id) {
                    continue;
                }
                // Negative means "not reported"; only act on a real number.
                if node.cert_expiry_days >= 0
                    && i64::from(node.cert_expiry_days) <= cfg.renew_before_days
                {
                    match db.get_active_certificate_for_node(&node.id) {
                        // Already covered by the inventory pass above.
                        Ok(Some(_)) => {}
                        _ => due.push(node.id),
                    }
                }
            }
        }
        Err(error) => warn!(%error, "failed to list nodes for rotation"),
    }

    due
}

/// Ask one node to rotate its certificate.
pub async fn rotate_node(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
    force: bool,
) -> Result<node_proto::RotateNodeCertResponse, String> {
    let node = db
        .get_node(node_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("node '{node_id}' is not registered"))?;
    if node.approval_status != "approved" {
        return Err(format!(
            "node '{node_id}' is not approved (status: {})",
            node.approval_status
        ));
    }
    if clients.get_admin(&node.address).is_none() {
        clients
            .connect(&node.address)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut admin = clients
        .get_admin(&node.address)
        .ok_or_else(|| format!("no admin client for {}", node.address))?;

    let response = admin
        .rotate_node_cert(Request::new(node_proto::RotateNodeCertRequest {
            force,
            reason: "controller-initiated rotation".to_string(),
        }))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();

    if !response.success {
        return Err(response.message);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CERT_STATUS_ACTIVE, CERT_STATUS_REVOKED};
    use crate::pki::REASON_NONE;

    fn cfg(renew_before_days: i64, fraction: f64) -> CertRotationConfig {
        CertRotationConfig {
            enabled: true,
            check_interval_secs: 60,
            renew_before_days,
            renew_at_lifetime_fraction: fraction,
            warn_before_days: renew_before_days.max(45),
            cert_validity_days: 365,
        }
    }

    fn row(node_id: &str, not_before: OffsetDateTime, not_after: OffsetDateTime) -> IssuedCertRow {
        IssuedCertRow {
            serial_hex: format!("{node_id}-serial").to_uppercase(),
            subject_cn: format!("kcore-node-{node_id}"),
            identity_kind: KIND_NODE.to_string(),
            node_id: node_id.to_string(),
            issuer_cn: "test-sub-ca".to_string(),
            fingerprint_sha256: "ff".repeat(32),
            not_before: format_ts(not_before),
            not_after: format_ts(not_after),
            issued_at: format_ts(not_before),
            status: CERT_STATUS_ACTIVE.to_string(),
            revocation_reason: REASON_NONE,
            revoked_at: String::new(),
        }
    }

    #[test]
    fn not_due_outside_both_windows() {
        let now = OffsetDateTime::now_utc();
        // 365-day cert, 200 days left: 55% of lifetime remains.
        let not_before = now - Duration::days(165);
        let not_after = now + Duration::days(200);
        assert!(!is_due_for_renewal(not_before, not_after, now, 30, 0.25));
    }

    #[test]
    fn due_once_inside_the_fixed_day_window() {
        let now = OffsetDateTime::now_utc();
        let not_before = now - Duration::days(335);
        // 30 days is the threshold: 31 days out is not due, 29 is.
        assert!(!is_due_for_renewal(
            not_before,
            now + Duration::days(31),
            now,
            30,
            0.0
        ));
        assert!(is_due_for_renewal(
            not_before,
            now + Duration::days(29),
            now,
            30,
            0.0
        ));
    }

    #[test]
    fn due_once_inside_the_lifetime_fraction_window() {
        let now = OffsetDateTime::now_utc();
        // 100-day lifetime: 30 days left is 30% (not due at 25%), 20 days is 20% (due).
        let not_after_30 = now + Duration::days(30);
        let not_before_30 = not_after_30 - Duration::days(100);
        assert!(!is_due_for_renewal(
            not_before_30,
            not_after_30,
            now,
            0,
            0.25
        ));

        let not_after_20 = now + Duration::days(20);
        let not_before_20 = not_after_20 - Duration::days(100);
        assert!(is_due_for_renewal(
            not_before_20,
            not_after_20,
            now,
            0,
            0.25
        ));
    }

    #[test]
    fn short_lived_certificates_rotate_on_the_fraction_rule_alone() {
        let now = OffsetDateTime::now_utc();
        // A 2-hour certificate can never satisfy a 30-day window, so only the
        // fraction rule can decide. 90 minutes left of 2 hours is 75%.
        let not_after = now + Duration::minutes(90);
        let not_before = not_after - Duration::hours(2);
        assert!(!is_due_for_renewal(not_before, not_after, now, 0, 0.25));
        // 20 minutes left is 16.7%.
        let not_after = now + Duration::minutes(20);
        let not_before = not_after - Duration::hours(2);
        assert!(is_due_for_renewal(not_before, not_after, now, 0, 0.25));
    }

    #[test]
    fn already_expired_certificates_are_always_due() {
        let now = OffsetDateTime::now_utc();
        assert!(is_due_for_renewal(
            now - Duration::days(400),
            now - Duration::days(1),
            now,
            0,
            0.0
        ));
    }

    #[test]
    fn zero_length_validity_window_is_due() {
        let now = OffsetDateTime::now_utc();
        let t = now + Duration::seconds(5);
        assert!(is_due_for_renewal(t, t, now, 0, 0.0));
    }

    #[test]
    fn row_is_due_ignores_unparseable_timestamps() {
        let now = OffsetDateTime::now_utc();
        let mut r = row("node-a", now - Duration::days(360), now + Duration::days(1));
        assert!(row_is_due(&r, now, &cfg(30, 0.25)));
        r.not_after = "garbage".to_string();
        assert!(!row_is_due(&r, now, &cfg(30, 0.25)));
    }

    #[test]
    fn nodes_due_for_rotation_selects_only_certificates_inside_the_window() {
        let db = Database::open(":memory:").expect("db");
        let now = OffsetDateTime::now_utc();

        // Healthy: 200 of 365 days left.
        db.record_issued_certificate(&row(
            "node-fresh",
            now - Duration::days(165),
            now + Duration::days(200),
        ))
        .expect("record fresh");
        // Due: 10 days left.
        db.record_issued_certificate(&row(
            "node-expiring",
            now - Duration::days(355),
            now + Duration::days(10),
        ))
        .expect("record expiring");

        let due = nodes_due_for_rotation(&db, &cfg(30, 0.25));
        assert_eq!(due, vec!["node-expiring".to_string()]);
    }

    #[test]
    fn nodes_due_for_rotation_ignores_revoked_and_rotated_rows() {
        let db = Database::open(":memory:").expect("db");
        let now = OffsetDateTime::now_utc();
        let mut revoked = row(
            "node-revoked",
            now - Duration::days(355),
            now + Duration::days(5),
        );
        revoked.status = CERT_STATUS_REVOKED.to_string();
        db.record_issued_certificate(&revoked).expect("record");

        let mut rotated = row(
            "node-rotated",
            now - Duration::days(355),
            now + Duration::days(5),
        );
        rotated.status = crate::db::CERT_STATUS_ROTATED.to_string();
        db.record_issued_certificate(&rotated).expect("record");

        assert!(nodes_due_for_rotation(&db, &cfg(30, 0.25)).is_empty());
    }

    #[test]
    fn nodes_due_for_rotation_covers_nodes_without_an_inventory_row() {
        let db = Database::open(":memory:").expect("db");
        db.upsert_node(&crate::db::NodeRow {
            id: "legacy-node".into(),
            hostname: "legacy".into(),
            address: "10.0.9.9:9091".into(),
            cpu_cores: 4,
            memory_bytes: 1 << 30,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: "eno1".into(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            // Reported by heartbeat: inside the 30-day window.
            cert_expiry_days: 7,
            luks_method: String::new(),
            dc_id: "DC1".into(),
        })
        .expect("upsert node");

        let due = nodes_due_for_rotation(&db, &cfg(30, 0.25));
        assert_eq!(due, vec!["legacy-node".to_string()]);
    }

    #[test]
    fn nodes_due_for_rotation_skips_unapproved_and_unreported_nodes() {
        let db = Database::open(":memory:").expect("db");
        let base = crate::db::NodeRow {
            id: String::new(),
            hostname: "n".into(),
            address: "10.0.9.9:9091".into(),
            cpu_cores: 1,
            memory_bytes: 1 << 30,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: "eno1".into(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".into(),
        };
        db.upsert_node(&crate::db::NodeRow {
            id: "pending-node".into(),
            approval_status: "pending".into(),
            cert_expiry_days: 1,
            ..base.clone()
        })
        .expect("upsert pending");
        db.upsert_node(&crate::db::NodeRow {
            id: "unknown-expiry-node".into(),
            ..base
        })
        .expect("upsert unknown");

        assert!(nodes_due_for_rotation(&db, &cfg(30, 0.25)).is_empty());
    }
}
