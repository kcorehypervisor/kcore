//! Pure ClickHouse cluster model: spec, diff, rollout order, Nix fragment, SQL.

pub mod nixgen;
pub mod ops;
pub mod rollout;
pub mod spec;

pub use nixgen::{
    cluster_root, keeper_data_path, render_disabled, render_fragment, render_node, secrets_dir,
    server_data_path, tls_dir, user_password_env, Fragment, KeeperFragment, ServerFragment,
};
pub use rollout::{
    plan_rollout, rollout_invariants, safe_to_take_down, OpKind, RolloutRequest, Step,
};
pub use spec::{
    diff, normalize, parse_manifest, split_issues, validate, ClusterSpec, PlanAction, PlanDiff,
};
