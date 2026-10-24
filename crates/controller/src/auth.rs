use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::{Request, Status};

use crate::db::Database;

pub const CN_KCTL: &str = "kctl";
pub const CN_NODE_PREFIX: &str = "kcore-node-";
pub const CN_CONTROLLER_PREFIX: &str = "kcore-controller-";
pub const CN_KCTL_PREFIX: &str = "kctl:";

/// Minimum operator capability for management RPCs (human operators and bootstrap).
///
/// Roadmap roles: `read-only` < `backup-operator` < `vm-admin` < `cluster-admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperatorRole {
    ReadOnly = 1,
    BackupOperator = 2,
    VmAdmin = 3,
    ClusterAdmin = 4,
}

impl OperatorRole {
    pub fn satisfies(self, required: OperatorRole) -> bool {
        self >= required
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            OperatorRole::ReadOnly => "read-only",
            OperatorRole::BackupOperator => "backup-operator",
            OperatorRole::VmAdmin => "vm-admin",
            OperatorRole::ClusterAdmin => "cluster-admin",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(OperatorRole::ReadOnly),
            "backup-operator" => Some(OperatorRole::BackupOperator),
            // Canonical name; accept legacy "admin" from earlier branch drafts.
            "vm-admin" | "admin" => Some(OperatorRole::VmAdmin),
            "cluster-admin" => Some(OperatorRole::ClusterAdmin),
            _ => None,
        }
    }

    pub fn as_compliance_label(self) -> &'static str {
        self.as_db_str()
    }
}

#[derive(Debug, Clone)]
pub enum ManagementPeer {
    /// Legacy single-operator cert; retired once a cluster-admin operator has a cert, unless `bootstrap_kctl` is set.
    LegacyKctl,
    /// Per-operator cert `CN=kctl:<name>`.
    NamedOperator(String),
    /// Another controller (`CN=kcore-controller-*`); full cluster-admin on shared Controller RPCs only.
    PeerController,
}

fn classify_management_peer(cn: &str) -> Result<ManagementPeer, Status> {
    if cn.starts_with(CN_NODE_PREFIX) {
        return Err(Status::permission_denied(
            "node certificates cannot call this RPC",
        ));
    }
    if cn.starts_with(CN_CONTROLLER_PREFIX) {
        return Ok(ManagementPeer::PeerController);
    }
    if cn == CN_KCTL {
        return Ok(ManagementPeer::LegacyKctl);
    }
    let Some(rest) = cn.strip_prefix(CN_KCTL_PREFIX) else {
        return Err(Status::permission_denied(format!(
            "unrecognized client identity '{cn}'"
        )));
    };
    validate_operator_name(rest)?;
    Ok(ManagementPeer::NamedOperator(rest.to_string()))
}

/// Validate operator name segment used in `CN=kctl:<name>`.
pub fn validate_operator_name(name: &str) -> Result<(), Status> {
    if name.is_empty() {
        return Err(Status::invalid_argument("operator name cannot be empty"));
    }
    if name.len() > 64 {
        return Err(Status::invalid_argument(
            "operator name must be at most 64 characters",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Status::invalid_argument(
            "operator name must be ASCII alphanumeric, '_' or '-'",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebTokenKind {
    /// Password checked, second factor still due.
    Pending,
    Session,
    Api,
}

impl WebTokenKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            WebTokenKind::Pending => "pending",
            WebTokenKind::Session => "session",
            WebTokenKind::Api => "api",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(WebTokenKind::Pending),
            "session" => Some(WebTokenKind::Session),
            "api" => Some(WebTokenKind::Api),
            _ => None,
        }
    }
}

/// Request extension set by the web listener after a bearer token checks out.
/// The mTLS listener never sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebIdentity {
    pub operator: String,
    pub token_id: String,
    pub kind: WebTokenKind,
    /// API tokens may be capped below the operator's own role.
    pub max_role: Option<OperatorRole>,
    pub expires_at: i64,
}

pub fn web_identity<T>(request: &Request<T>) -> Option<&WebIdentity> {
    request.extensions().get::<WebIdentity>()
}

/// Peer address recorded by the web listener, which has no tonic connect info.
#[derive(Debug, Clone, Copy)]
pub struct WebPeer {
    pub remote_addr: std::net::SocketAddr,
}

/// Remote address of the caller on either listener.
pub fn remote_addr<T>(request: &Request<T>) -> Option<std::net::SocketAddr> {
    if let Some(peer) = request.extensions().get::<WebPeer>() {
        return Some(peer.remote_addr);
    }
    if let Some(tls) = request.extensions().get::<TlsConnectInfo<TcpConnectInfo>>() {
        return tls.get_ref().remote_addr();
    }
    request
        .extensions()
        .get::<TcpConnectInfo>()
        .and_then(TcpConnectInfo::remote_addr)
}

/// Caller identity in `CN` form. A web session or API token is the named
/// operator `kctl:<name>`, so every role check treats it like that operator's
/// certificate. A pending token maps to a name no check accepts.
pub fn peer_cn<T>(request: &Request<T>) -> Option<String> {
    if let Some(web) = web_identity(request) {
        return Some(match web.kind {
            WebTokenKind::Pending => format!("web-pending:{}", web.operator),
            WebTokenKind::Session | WebTokenKind::Api => {
                format!("{CN_KCTL_PREFIX}{}", web.operator)
            }
        });
    }
    cert_cn(request)
}

fn apply_token_cap<T>(request: &Request<T>, role: OperatorRole) -> OperatorRole {
    match web_identity(request).and_then(|web| web.max_role) {
        Some(cap) => role.min(cap),
        None => role,
    }
}

/// Extract the Common Name from the peer's TLS client certificate.
/// Returns `None` when TLS is not in use or no client cert was presented.
fn cert_cn<T>(request: &Request<T>) -> Option<String> {
    let tls_info = request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()?;
    let certs = tls_info.peer_certs()?;
    let cert_der = certs.first()?;

    use x509_parser::prelude::FromDer;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(cert_der.as_ref()).ok()?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()?
        .as_str()
        .ok()
        .map(String::from);
    cn
}

fn effective_operator_role(
    db: &Database,
    peer: &ManagementPeer,
    bootstrap_kctl: bool,
) -> Result<OperatorRole, Status> {
    match peer {
        ManagementPeer::PeerController => Ok(OperatorRole::ClusterAdmin),
        ManagementPeer::LegacyKctl => {
            let retired = db
                .has_certified_cluster_admin()
                .map_err(|e| Status::internal(format!("operator lookup db error: {e}")))?;
            if !retired || bootstrap_kctl {
                Ok(OperatorRole::ClusterAdmin)
            } else {
                Err(Status::permission_denied(
                    "legacy CN=kctl client is disabled once a cluster-admin operator has a certificate; use a per-operator certificate (CN=kctl:<name>)",
                ))
            }
        }
        ManagementPeer::NamedOperator(name) => {
            let roles = db
                .list_operator_role_strings(name)
                .map_err(|e| Status::internal(format!("list operator roles: {e}")))?;
            let mut max_role = OperatorRole::ReadOnly;
            let mut any = false;
            for r in roles {
                if let Some(parsed) = OperatorRole::from_db_str(&r) {
                    any = true;
                    if parsed > max_role {
                        max_role = parsed;
                    }
                }
            }
            if !any {
                return Err(Status::permission_denied(format!(
                    "operator '{name}' has no roles assigned"
                )));
            }
            Ok(max_role)
        }
    }
}

/// Authorize a management RPC on the main `Controller` service (`kctl` or `kcore-controller-*`).
#[allow(clippy::result_large_err)]
pub fn require_controller_operator<T>(
    request: &Request<T>,
    db: &Database,
    tls_active: bool,
    bootstrap_kctl: bool,
    required: OperatorRole,
) -> Result<(), Status> {
    if peer_cn(request).is_none() {
        return Ok(());
    }
    let effective = controller_operator_role(request, db, tls_active, bootstrap_kctl)?;
    if effective.satisfies(required) {
        Ok(())
    } else {
        let cn = peer_cn(request).unwrap_or_default();
        Err(Status::permission_denied(format!(
            "identity '{cn}' lacks required role {required:?} (effective {effective:?})"
        )))
    }
}

/// Effective operator role for this request. Without TLS, management RPCs are
/// open and the caller is treated as cluster-admin.
#[allow(clippy::result_large_err)]
pub fn controller_operator_role<T>(
    request: &Request<T>,
    db: &Database,
    tls_active: bool,
    bootstrap_kctl: bool,
) -> Result<OperatorRole, Status> {
    let cn = match peer_cn(request) {
        Some(cn) => cn,
        None => {
            if !tls_active {
                return Ok(OperatorRole::ClusterAdmin);
            }
            return Ok(OperatorRole::ClusterAdmin);
        }
    };
    let peer = classify_management_peer(&cn)?;
    let role = effective_operator_role(db, &peer, bootstrap_kctl)?;
    Ok(apply_token_cap(request, role))
}

/// Authorize `ControllerAdmin` RPCs (historically CN=kctl only — never peer controllers).
#[allow(clippy::result_large_err)]
pub fn require_admin_operator<T>(
    request: &Request<T>,
    db: &Database,
    tls_active: bool,
    bootstrap_kctl: bool,
    required: OperatorRole,
) -> Result<(), Status> {
    let cn = match peer_cn(request) {
        Some(cn) => cn,
        None => {
            if !tls_active {
                return Ok(());
            }
            return Ok(());
        }
    };

    if cn.starts_with(CN_NODE_PREFIX) {
        return Err(Status::permission_denied(
            "node certificates cannot call controller admin RPCs",
        ));
    }
    if cn.starts_with(CN_CONTROLLER_PREFIX) {
        return Err(Status::permission_denied(
            "controller peer certificates cannot call controller admin RPCs",
        ));
    }

    let peer = if cn == CN_KCTL {
        ManagementPeer::LegacyKctl
    } else if let Some(rest) = cn.strip_prefix(CN_KCTL_PREFIX) {
        validate_operator_name(rest)?;
        ManagementPeer::NamedOperator(rest.to_string())
    } else {
        return Err(Status::permission_denied(format!(
            "unrecognized client identity '{cn}'"
        )));
    };

    let effective = apply_token_cap(request, effective_operator_role(db, &peer, bootstrap_kctl)?);

    if effective.satisfies(required) {
        Ok(())
    } else {
        Err(Status::permission_denied(format!(
            "identity '{cn}' lacks required role {required:?} (effective {effective:?})"
        )))
    }
}

/// Require that the peer's certificate CN matches one of the allowed patterns.
///
/// Patterns ending with `-` are treated as prefixes (for node certs like
/// `kcore-node-10.0.0.1`). All other patterns require an exact match.
///
/// When TLS is not in use (insecure mode), authorization is skipped — the
/// startup-time `--allow-insecure` enforcement is the primary control.
#[allow(clippy::result_large_err)]
pub fn require_peer<T>(request: &Request<T>, allowed: &[&str]) -> Result<(), Status> {
    let cn = match peer_cn(request) {
        Some(cn) => cn,
        None => return Ok(()),
    };

    if is_authorized(&cn, allowed) {
        Ok(())
    } else {
        Err(Status::permission_denied(format!(
            "peer '{cn}' is not authorized for this operation"
        )))
    }
}

fn is_authorized(cn: &str, allowed: &[&str]) -> bool {
    allowed.iter().any(|pattern| {
        if pattern.ends_with('-') {
            cn.starts_with(pattern)
        } else {
            cn == *pattern
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_prefix_matching() {
        assert!(is_authorized("kcore-node-10.0.0.1", &[CN_NODE_PREFIX]));
        assert!(is_authorized("kcore-node-192.168.1.1", &[CN_NODE_PREFIX]));
        assert!(!is_authorized("kcore-controller", &[CN_NODE_PREFIX]));
        assert!(!is_authorized("kctl", &[CN_NODE_PREFIX]));
    }

    #[test]
    fn exact_matching() {
        assert!(is_authorized("kctl", &[CN_KCTL]));
        assert!(!is_authorized("kctl-evil", &[CN_KCTL]));
        assert!(!is_authorized("kcore-controller", &[CN_KCTL]));
    }

    #[test]
    fn multiple_allowed_patterns() {
        let allowed = &[CN_KCTL, CN_NODE_PREFIX];
        assert!(is_authorized("kctl", allowed));
        assert!(is_authorized("kcore-node-10.0.0.1", allowed));
        assert!(!is_authorized("kcore-controller", allowed));
    }

    #[test]
    fn require_peer_allows_missing_tls_info() {
        let request = Request::new(());
        assert!(require_peer(&request, &[CN_KCTL]).is_ok());
    }

    #[test]
    fn operator_name_validation() {
        assert!(validate_operator_name("alice").is_ok());
        assert!(validate_operator_name("op-1").is_ok());
        assert!(validate_operator_name("").is_err());
        assert!(validate_operator_name("bad:name").is_err());
    }

    #[test]
    fn role_lattice() {
        assert!(OperatorRole::ClusterAdmin.satisfies(OperatorRole::ReadOnly));
        assert!(OperatorRole::VmAdmin.satisfies(OperatorRole::ReadOnly));
        assert!(OperatorRole::BackupOperator.satisfies(OperatorRole::ReadOnly));
        assert!(!OperatorRole::ReadOnly.satisfies(OperatorRole::BackupOperator));
        assert!(!OperatorRole::BackupOperator.satisfies(OperatorRole::VmAdmin));
        assert!(OperatorRole::VmAdmin.satisfies(OperatorRole::BackupOperator));
        assert!(!OperatorRole::ReadOnly.satisfies(OperatorRole::VmAdmin));
        assert!(OperatorRole::ClusterAdmin.satisfies(OperatorRole::VmAdmin));
        assert!(!OperatorRole::VmAdmin.satisfies(OperatorRole::ClusterAdmin));
    }

    #[test]
    fn role_db_strings() {
        assert_eq!(OperatorRole::VmAdmin.as_db_str(), "vm-admin");
        assert_eq!(
            OperatorRole::from_db_str("vm-admin"),
            Some(OperatorRole::VmAdmin)
        );
        assert_eq!(
            OperatorRole::from_db_str("admin"),
            Some(OperatorRole::VmAdmin)
        );
        assert_eq!(OperatorRole::BackupOperator.as_db_str(), "backup-operator");
        assert_eq!(
            OperatorRole::from_db_str("backup-operator"),
            Some(OperatorRole::BackupOperator)
        );
    }

    #[test]
    fn legacy_kctl_survives_bootstrap_until_an_admin_has_a_cert() {
        let db = Database::open(":memory:").expect("db");
        let legacy = || effective_operator_role(&db, &ManagementPeer::LegacyKctl, false);
        assert_eq!(legacy().unwrap(), OperatorRole::ClusterAdmin);

        db.create_operator("alice").unwrap();
        assert_eq!(legacy().unwrap(), OperatorRole::ClusterAdmin);
        db.grant_operator_role_str("alice", "cluster-admin")
            .unwrap();
        assert_eq!(legacy().unwrap(), OperatorRole::ClusterAdmin);

        db.create_operator("bob").unwrap();
        db.grant_operator_role_str("bob", "read-only").unwrap();
        db.set_operator_cert_serial("bob", "0b").unwrap();
        assert_eq!(legacy().unwrap(), OperatorRole::ClusterAdmin);

        db.set_operator_cert_serial("alice", "0a").unwrap();
        assert!(legacy().is_err());
        assert_eq!(
            effective_operator_role(&db, &ManagementPeer::LegacyKctl, true).unwrap(),
            OperatorRole::ClusterAdmin
        );
    }
}
