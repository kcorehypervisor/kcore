//! Proactive certificate rotation, CRL refresh and expiry warnings.
//!
//! One background loop, in the style of the other reconcilers
//! (`ceph_cluster_reconciler`, `disk_reconciler`), that on every tick:
//!
//! 1. refreshes the in-memory revoked-serial set used by the auth path,
//! 2. regenerates and republishes the CRL when it is due,
//! 3. warns about certificates approaching expiry,
//! 4. asks nodes whose certificate is inside the renewal window to rotate,
//! 5. revokes node certificates that rotation replaced, once the replacement
//!    has been in use for `revokeSupersededAfterHours`.
//!
//! The controller never signs a replacement behind the node's back: it calls
//! `NodeAdmin.RotateNodeCert`, and the node then generates a keypair, submits
//! a CSR (`Controller.SignNodeCsr`), installs the result and reloads. If any
//! of that fails the node keeps serving its existing certificate and the next
//! tick retries, so a failed rotation is never worse than no rotation.

use std::collections::HashSet;
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
use crate::pki::{format_ts, parse_ts, REASON_SUPERSEDED};

/// Everything the loop needs, bundled so `main.rs` stays readable.
pub struct CertRotationContext {
    pub db: Database,
    pub clients: NodeClients,
    pub sub_ca: Arc<Mutex<SubCaState>>,
    pub crl_cache: CrlCache,
    pub revocation: RevocationState,
    pub rotation: CertRotationConfig,
    pub pki: PkiConfig,
    pub webhooks: crate::webhooks::Dispatcher,
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
        let mut cert_warnings = crate::webhooks::WarningDedup::default();
        reconcile_once(&ctx, &mut cert_warnings).await;
        let mut ticker = tokio_time::interval(interval);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            reconcile_once(&ctx, &mut cert_warnings).await;
        }
    });
}

async fn reconcile_once(
    ctx: &CertRotationContext,
    cert_warnings: &mut crate::webhooks::WarningDedup,
) {
    let now = OffsetDateTime::now_utc();
    record_seen_certificates(&ctx.db, &ctx.revocation, now);
    revoke_superseded_certificates(&ctx.db, &ctx.revocation, &ctx.rotation, now);

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

    warn_about_expiring_certificates(&ctx.db, &ctx.rotation, &ctx.webhooks, cert_warnings);

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

/// Write the serials peers presented since the last tick to the inventory.
fn record_seen_certificates(db: &Database, revocation: &RevocationState, now: OffsetDateTime) {
    let seen = revocation.take_seen();
    if seen.is_empty() {
        return;
    }
    if let Err(error) = db.mark_certificates_seen(&seen, &format_ts(now)) {
        warn!(%error, "failed to record which certificates were presented");
    }
}

/// Revoke node certificates that rotation replaced.
///
/// Rotation marks the old certificate `rotated` when the controller signs the
/// new one, before the node has installed it, so a `rotated` row alone is not
/// proof that the node moved on. The old certificate is revoked only after
/// the new one was presented to the controller and
/// `revoke_superseded_after_hours` have passed. A node that never installed
/// its new certificate keeps the old one working, and the next rotation tick
/// tries again.
///
/// Returns the revoked serials.
pub fn revoke_superseded_certificates(
    db: &Database,
    revocation: &RevocationState,
    cfg: &CertRotationConfig,
    now: OffsetDateTime,
) -> Vec<String> {
    if cfg.revoke_superseded_after_hours <= 0 {
        return Vec::new();
    }
    let seen_before = format_ts(now - Duration::hours(cfg.revoke_superseded_after_hours));
    let rows =
        match db.list_superseded_certificates_to_revoke(KIND_NODE, &seen_before, &format_ts(now)) {
            Ok(rows) => rows,
            Err(error) => {
                warn!(%error, "failed to list superseded certificates");
                return Vec::new();
            }
        };
    let mut revoked = Vec::new();
    for row in rows {
        match db.revoke_certificate_by_serial(&row.serial_hex, REASON_SUPERSEDED, &format_ts(now)) {
            Ok(Some(_)) => {
                revocation.insert_revoked(&row.serial_hex);
                info!(
                    serial = %row.serial_hex,
                    subject = %row.subject_cn,
                    node_id = %row.node_id,
                    "revoked a node certificate that rotation replaced"
                );
                revoked.push(row.serial_hex);
            }
            Ok(None) => {}
            Err(error) => warn!(
                serial = %row.serial_hex,
                %error,
                "failed to revoke a superseded certificate"
            ),
        }
    }
    revoked
}

/// Log a warning for every active certificate inside the warning window, so
/// expiry is visible in the controller journal and not only via `kctl`.
///
/// The same pass emits `cert.expiry.warning` once per certificate (or once per
/// node, when the only signal is the expiry the node reports on heartbeat)
/// until that certificate leaves the window.
fn warn_about_expiring_certificates(
    db: &Database,
    cfg: &CertRotationConfig,
    webhooks: &crate::webhooks::Dispatcher,
    dedup: &mut crate::webhooks::WarningDedup,
) {
    let now = OffsetDateTime::now_utc();
    let threshold = format_ts(now + Duration::days(cfg.warn_before_days));
    let rows = match db.list_issued_certificates(crate::db::CERT_STATUS_ACTIVE, "", &threshold) {
        Ok(rows) => rows,
        Err(error) => {
            warn!(%error, "failed to list certificates for expiry warnings");
            return;
        }
    };
    let mut covered_nodes = HashSet::new();
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
        if !row.node_id.is_empty() {
            covered_nodes.insert(row.node_id.clone());
        }
        let slot = format!("cert/{}", row.serial_hex);
        let fingerprint = format!("{}|{}", row.serial_hex, row.not_after);
        if dedup.claim(&slot, &fingerprint) {
            webhooks.emit(
                crate::webhooks::EVENT_CERT_EXPIRY_WARNING,
                format!("cert/{}", row.serial_hex),
                serde_json::json!({
                    "serial": row.serial_hex,
                    "subject": row.subject_cn,
                    "nodeId": row.node_id,
                    "notAfter": row.not_after,
                    "daysUntilExpiry": days,
                    "expired": days < 0,
                }),
            );
        }
    }

    // Nodes enrolled before certificate inventory existed only report a day
    // count on heartbeat. Skip any node the inventory pass already covered,
    // and skip nodes whose inventory cert is still outside the window.
    if let Ok(nodes) = db.list_nodes() {
        for node in nodes {
            if node.cert_expiry_days < 0
                || i64::from(node.cert_expiry_days) > cfg.warn_before_days
                || covered_nodes.contains(&node.id)
            {
                continue;
            }
            if matches!(db.get_active_certificate_for_node(&node.id), Ok(Some(_))) {
                continue;
            }
            let slot = format!("node/{}/cert-reported", node.id);
            if dedup.claim(&slot, "in-window") {
                webhooks.emit(
                    crate::webhooks::EVENT_CERT_EXPIRY_WARNING,
                    format!("node/{}", node.id),
                    serde_json::json!({
                        "serial": "",
                        "subject": "",
                        "nodeId": node.id,
                        "notAfter": "",
                        "daysUntilExpiry": node.cert_expiry_days,
                        "expired": false,
                        "source": "node-reported",
                    }),
                );
            }
        }
    }
    dedup.finish_pass();
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
            revoke_superseded_after_hours: 24,
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

    fn superseded_pair(
        db: &Database,
        now: OffsetDateTime,
        kind: &str,
        old_not_after: OffsetDateTime,
    ) -> (String, String) {
        let mut old = row("n1", now - Duration::days(300), old_not_after);
        old.serial_hex = "0A01".into();
        old.identity_kind = kind.into();
        old.status = crate::db::CERT_STATUS_ROTATED.into();
        let mut new = row("n1", now - Duration::days(2), now + Duration::days(363));
        new.serial_hex = "0B02".into();
        new.identity_kind = kind.into();
        db.record_issued_certificate(&old).unwrap();
        db.record_issued_certificate(&new).unwrap();
        (old.serial_hex, new.serial_hex)
    }

    fn revocation() -> RevocationState {
        RevocationState::new(
            crate::pki::revocation::FailMode::SoftFail,
            Duration::hours(1),
        )
    }

    #[test]
    fn a_superseded_certificate_waits_for_proof_the_replacement_is_in_use() {
        let db = Database::open(":memory:").expect("db");
        let now = OffsetDateTime::now_utc();
        let (_, new) = superseded_pair(&db, now, KIND_NODE, now + Duration::days(60));
        let state = revocation();
        let cfg = cfg(30, 0.25);

        assert!(revoke_superseded_certificates(&db, &state, &cfg, now).is_empty());

        db.mark_certificates_seen(
            std::slice::from_ref(&new),
            &format_ts(now - Duration::hours(2)),
        )
        .unwrap();
        assert!(
            revoke_superseded_certificates(&db, &state, &cfg, now).is_empty(),
            "the replacement has been used for less than the grace period"
        );

        db.mark_certificates_seen(
            std::slice::from_ref(&new),
            &format_ts(now - Duration::hours(30)),
        )
        .unwrap();
        assert!(
            revoke_superseded_certificates(&db, &state, &cfg, now).is_empty(),
            "a first-seen time is never overwritten"
        );
    }

    #[test]
    fn the_old_certificate_is_revoked_once_the_replacement_has_been_in_use() {
        let db = Database::open(":memory:").expect("db");
        let now = OffsetDateTime::now_utc();
        let (old, new) = superseded_pair(&db, now, KIND_NODE, now + Duration::days(60));
        let state = revocation();
        let cfg = cfg(30, 0.25);
        db.mark_certificates_seen(
            std::slice::from_ref(&new),
            &format_ts(now - Duration::hours(30)),
        )
        .unwrap();

        assert_eq!(
            revoke_superseded_certificates(&db, &state, &cfg, now),
            vec![old.clone()]
        );
        let row = db.get_issued_certificate(&old).unwrap().unwrap();
        assert_eq!(row.status, crate::db::CERT_STATUS_REVOKED);
        assert_eq!(row.revocation_reason, REASON_SUPERSEDED);
        assert!(matches!(
            state.check(&old),
            crate::pki::revocation::Decision::Revoked { .. }
        ));
        let current = db.get_issued_certificate(&new).unwrap().unwrap();
        assert_eq!(current.status, crate::db::CERT_STATUS_ACTIVE);
        assert!(revoke_superseded_certificates(&db, &state, &cfg, now).is_empty());
    }

    #[test]
    fn automatic_revocation_can_be_turned_off_and_skips_other_identities() {
        let now = OffsetDateTime::now_utc();
        let state = revocation();

        let db = Database::open(":memory:").expect("db");
        let (_, new) = superseded_pair(&db, now, KIND_NODE, now + Duration::days(60));
        db.mark_certificates_seen(&[new], &format_ts(now - Duration::hours(30)))
            .unwrap();
        let mut off = cfg(30, 0.25);
        off.revoke_superseded_after_hours = 0;
        assert!(revoke_superseded_certificates(&db, &state, &off, now).is_empty());

        let db = Database::open(":memory:").expect("db");
        let (_, new) = superseded_pair(
            &db,
            now,
            crate::pki::inventory::KIND_OPERATOR,
            now + Duration::days(60),
        );
        db.mark_certificates_seen(&[new], &format_ts(now - Duration::hours(30)))
            .unwrap();
        assert!(
            revoke_superseded_certificates(&db, &state, &cfg(30, 0.25), now).is_empty(),
            "an operator may use the same identity from two machines"
        );

        let db = Database::open(":memory:").expect("db");
        let (_, new) = superseded_pair(&db, now, KIND_NODE, now - Duration::days(1));
        db.mark_certificates_seen(&[new], &format_ts(now - Duration::hours(30)))
            .unwrap();
        assert!(
            revoke_superseded_certificates(&db, &state, &cfg(30, 0.25), now).is_empty(),
            "an expired certificate needs no CRL entry"
        );
    }

    #[test]
    fn presented_certificates_reach_the_inventory_only_when_enforcement_is_on() {
        let db = Database::open(":memory:").expect("db");
        let now = OffsetDateTime::now_utc();
        let (_, new) = superseded_pair(&db, now, KIND_NODE, now + Duration::days(60));

        let off = RevocationState::disabled();
        off.note_seen(&new);
        record_seen_certificates(&db, &off, now);
        assert!(
            revoke_superseded_certificates(&db, &off, &cfg(30, 0.25), now + Duration::days(2))
                .is_empty()
        );

        let on = revocation();
        assert!(on.check_status(&new, "kcore-node-n1").is_ok());
        record_seen_certificates(&db, &on, now - Duration::hours(30));
        assert!(on.take_seen().is_empty());
        assert_eq!(
            revoke_superseded_certificates(&db, &on, &cfg(30, 0.25), now).len(),
            1
        );
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
