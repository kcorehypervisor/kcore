//! Static RBAC tables for `GetComplianceReport`, docs parity, and tests.
//!
//! Handler enforcement lives in `controller.rs` / `admin.rs`; when adding an RPC,
//! update the slices here and wire `require_*_operator` in the handler.

use crate::auth::{OperatorRole, CN_NODE_PREFIX};
use crate::controller_proto;

/// Compliance label for identities allowed on operator-management RPCs on `Controller`.
pub(crate) const ACL_OPERATOR_AND_PEER_CTRL: &str = "kctl|kctl:*|kcore-controller-*";

/// `ControllerAdmin::ApplyNixConfig` — human operators only (see `require_admin_operator`).
pub(crate) const ACL_HUMAN_OPERATOR: &str = "kctl|kctl:*";

pub(crate) fn acl_entry(
    method: &str,
    identity: &str,
    required_operator_role: &str,
) -> controller_proto::AccessControlEntry {
    controller_proto::AccessControlEntry {
        rpc_method: method.to_string(),
        allowed_identities: if identity.ends_with('-') {
            format!("{identity}*")
        } else {
            identity.to_string()
        },
        required_operator_role: required_operator_role.to_string(),
    }
}

/// Operator / peer-controller RPCs on [`Controller`](crate::grpc::ControllerService)
/// **before** the node-agent `RenewNodeCert` row in the compliance report (deterministic order).
pub(crate) static CONTROLLER_OPERATOR_RPC_BEFORE_RENEW: &[(&str, &str)] = &[
    ("CreateVm", "admin"),
    ("UpdateVm", "admin"),
    ("DeleteVm", "admin"),
    ("SetVmDesiredState", "admin"),
    ("GetVm", "read-only"),
    ("ListVms", "read-only"),
    ("CreateWorkload", "admin"),
    ("DeleteWorkload", "admin"),
    ("SetWorkloadDesiredState", "admin"),
    ("GetWorkload", "read-only"),
    ("ListWorkloads", "read-only"),
    ("CreateNetwork", "admin"),
    ("DeleteNetwork", "admin"),
    ("ListNetworks", "read-only"),
    ("CreateSecurityGroup", "admin"),
    ("GetSecurityGroup", "read-only"),
    ("ListSecurityGroups", "read-only"),
    ("DeleteSecurityGroup", "admin"),
    ("AttachSecurityGroup", "admin"),
    ("DetachSecurityGroup", "admin"),
    ("ListNodes", "read-only"),
    ("GetNode", "read-only"),
    ("CreateSshKey", "admin"),
    ("DeleteSshKey", "admin"),
    ("ListSshKeys", "read-only"),
    ("GetSshKey", "read-only"),
    ("DrainNode", "cluster-admin"),
    ("ApproveNode", "cluster-admin"),
    ("RejectNode", "cluster-admin"),
];

/// Operator / peer-controller RPCs **after** `RenewNodeCert` in the compliance report.
pub(crate) static CONTROLLER_OPERATOR_RPC_AFTER_RENEW: &[(&str, &str)] = &[
    ("IssueNodeBootstrapCert", "cluster-admin"),
    ("RotateSubCa", "cluster-admin"),
    ("ReloadTls", "cluster-admin"),
    ("GetComplianceReport", "read-only"),
    ("ListAuditEvents", "read-only"),
    ("GetNetworkOverview", "read-only"),
    ("GetStorageOverview", "read-only"),
    ("ListVolumes", "read-only"),
    ("CreateDiskLayout", "cluster-admin"),
    ("GetDiskLayout", "read-only"),
    ("ListDiskLayouts", "read-only"),
    ("DeleteDiskLayout", "cluster-admin"),
    ("ClassifyDiskLayout", "read-only"),
    ("CreateClusterUpdate", "cluster-admin"),
    ("GetClusterUpdate", "read-only"),
    ("ListClusterUpdates", "read-only"),
    ("PlanClusterUpdate", "read-only"),
    ("ApproveClusterUpdate", "cluster-admin"),
    ("CancelClusterUpdate", "cluster-admin"),
    ("RollbackClusterUpdate", "cluster-admin"),
    ("CreateOperator", "cluster-admin"),
    ("DeleteOperator", "cluster-admin"),
    ("ListOperators", "cluster-admin"),
    ("GetOperator", "cluster-admin"),
    ("GrantOperatorRole", "cluster-admin"),
    ("RevokeOperatorRole", "cluster-admin"),
    ("IssueOperatorCert", "cluster-admin"),
];

/// `ControllerAdmin` service RPCs (separate tonic service; listed at end of compliance output).
pub(crate) static CONTROLLER_ADMIN_RPC_ACL: &[(&str, &str, &str)] = &[
    ("ApplyNixConfig", ACL_HUMAN_OPERATOR, "cluster-admin"),
    (
        "GetReplicationEvents",
        ACL_OPERATOR_AND_PEER_CTRL,
        "cluster-admin",
    ),
    (
        "AckReplicationEvents",
        ACL_OPERATOR_AND_PEER_CTRL,
        "cluster-admin",
    ),
    (
        "GetReplicationStatus",
        ACL_OPERATOR_AND_PEER_CTRL,
        "cluster-admin",
    ),
    (
        "ListReplicationConflicts",
        ACL_OPERATOR_AND_PEER_CTRL,
        "cluster-admin",
    ),
    (
        "ResolveReplicationConflict",
        ACL_OPERATOR_AND_PEER_CTRL,
        "cluster-admin",
    ),
];

/// Full access-control table returned by `GetComplianceReport`.
pub(crate) fn compliance_access_control_entries() -> Vec<controller_proto::AccessControlEntry> {
    let mut v = vec![
        acl_entry("RegisterNode", CN_NODE_PREFIX, ""),
        acl_entry("Heartbeat", CN_NODE_PREFIX, ""),
        acl_entry("SyncVmState", CN_NODE_PREFIX, ""),
        acl_entry("SyncWorkloadState", CN_NODE_PREFIX, ""),
    ];
    v.extend(
        CONTROLLER_OPERATOR_RPC_BEFORE_RENEW
            .iter()
            .map(|(m, r)| acl_entry(m, ACL_OPERATOR_AND_PEER_CTRL, r)),
    );
    v.push(acl_entry("RenewNodeCert", CN_NODE_PREFIX, ""));
    v.extend(
        CONTROLLER_OPERATOR_RPC_AFTER_RENEW
            .iter()
            .map(|(m, r)| acl_entry(m, ACL_OPERATOR_AND_PEER_CTRL, r)),
    );
    v.extend(
        CONTROLLER_ADMIN_RPC_ACL
            .iter()
            .map(|(m, id, r)| acl_entry(m, id, r)),
    );
    v
}

/// Parsed `(rpc_method, min_role)` for all operator-visible `Controller` RPCs (both slices).
pub(crate) fn controller_operator_rbac_pairs() -> Vec<(&'static str, OperatorRole)> {
    CONTROLLER_OPERATOR_RPC_BEFORE_RENEW
        .iter()
        .chain(CONTROLLER_OPERATOR_RPC_AFTER_RENEW.iter())
        .map(|(m, r)| {
            (
                *m,
                OperatorRole::from_db_str(r).unwrap_or_else(|| {
                    panic!("invalid role str {r} for RPC {m}");
                }),
            )
        })
        .collect()
}

/// `(rpc, identity_label, min_role)` for `ControllerAdmin`.
pub(crate) fn controller_admin_rbac_pairs() -> Vec<(&'static str, &'static str, OperatorRole)> {
    CONTROLLER_ADMIN_RPC_ACL
        .iter()
        .map(|(m, id, r)| {
            (
                *m,
                *id,
                OperatorRole::from_db_str(r).unwrap_or_else(|| {
                    panic!("invalid role str {r} for admin RPC {m}");
                }),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn controller_operator_rpc_names_unique() {
        let mut names: Vec<_> = CONTROLLER_OPERATOR_RPC_BEFORE_RENEW
            .iter()
            .chain(CONTROLLER_OPERATOR_RPC_AFTER_RENEW.iter())
            .map(|(m, _)| *m)
            .collect();
        let orig = names.clone();
        names.sort();
        names.dedup();
        assert_eq!(
            orig.len(),
            names.len(),
            "duplicate RPC name in controller operator ACL slices"
        );
    }

    #[test]
    fn compliance_acl_has_expected_entry_count() {
        let v = compliance_access_control_entries();
        assert!(
            v.len() >= 60,
            "compliance ACL unexpectedly short (got {})",
            v.len()
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, .. ProptestConfig::default() })]

        /// Random effective role vs every declared minimum: decision matches the role lattice.
        #[test]
        fn operator_rpc_role_matrix_matches_partial_order(
            eff in prop::sample::select(vec![
                OperatorRole::ReadOnly,
                OperatorRole::Admin,
                OperatorRole::ClusterAdmin,
            ])
        ) {
            for (_method, required) in controller_operator_rbac_pairs() {
                let allowed = eff.satisfies(required);
                prop_assert_eq!(allowed, eff >= required);
            }
            for (_method, _id, required) in controller_admin_rbac_pairs() {
                let allowed = eff.satisfies(required);
                prop_assert_eq!(allowed, eff >= required);
            }
        }
    }
}
