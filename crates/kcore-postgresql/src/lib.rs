//! Pure PostgreSQL cluster model: spec, diff, rollout order, Nix fragment,
//! Patroni request bodies and response parsers, and admin SQL.
//!
//! Nothing here touches the network or the filesystem. The controller and the
//! node agent both depend on this crate so the two sides agree on every name.

pub mod nixgen;
pub mod ops;
pub mod rollout;
pub mod spec;

pub use nixgen::{render_disabled, render_member, MemberFragment};
pub use rollout::{plan_rollout, rollout_invariants, MemberHealth, OpKind, RolloutRequest, Step};
pub use spec::{
    diff, normalize, parse_manifest, validate, ClusterSpec, Issue, PlanAction, PlanDiff,
};
