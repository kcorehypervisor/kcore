use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::{Request, Status};

use crate::db::Database;

pub const CN_KCTL: &str = "kctl";
pub const CN_NODE_PREFIX: &str = "kcore-node-";
pub const CN_CONTROLLER_PREFIX: &str = "kcore-controller-";
pub const CN_KCTL_PREFIX: &str = "kctl:";

/// Minimum operator capability for management RPCs (human operators and bootstrap).
///
/// Roadmap roles: `read-only` < `vm-admin` < `cluster-admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperatorRole {
    ReadOnly = 1,
    VmAdmin = 2,
    ClusterAdmin = 3,
}

impl OperatorRole {
    pub fn satisfies(self, required: OperatorRole) -> bool {
        self >= required
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            OperatorRole::ReadOnly => "read-only",
            OperatorRole::VmAdmin => "vm-admin",
            OperatorRole::ClusterAdmin => "cluster-admin",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(OperatorRole::ReadOnly),
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
    /// Legacy single-operator cert; bootstrap-only when operators exist unless `bootstrap_kctl` is set.
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

/// Extract the Common Name from the peer's TLS client certificate.
/// Returns `None` when TLS is not in use or no client cert was presented.
pub fn peer_cn<T>(request: &Request<T>) -> Option<String> {
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
            let n = db
                .count_operators()
                .map_err(|e| Status::internal(format!("operator CountOperators db error: {e}")))?;
            if n == 0 || bootstrap_kctl {
                Ok(OperatorRole::ClusterAdmin)
            } else {
                Err(Status::permission_denied(
                    "legacy CN=kctl client is disabled after operators are configured; use a per-operator certificate (CN=kctl:<name>)",
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
    let cn = match peer_cn(request) {
        Some(cn) => cn,
        None => {
            if !tls_active {
                return Ok(());
            }
            // TLS is configured but no client cert was presented — allow only when
            // the transport layer already accepted the connection (rare).
            return Ok(());
        }
    };

    let peer = classify_management_peer(&cn)?;
    let effective = effective_operator_role(db, &peer, bootstrap_kctl)?;

    if effective.satisfies(required) {
        Ok(())
    } else {
        Err(Status::permission_denied(format!(
            "identity '{cn}' lacks required role {required:?} (effective {effective:?})"
        )))
    }
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

    let effective = effective_operator_role(db, &peer, bootstrap_kctl)?;

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
    }
}
