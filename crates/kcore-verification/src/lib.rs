//! Design model of path checks, placement, replication, and compensation.
//!
//! These functions are not the product implementations in `kcore-sanitize`,
//! `kcore-disko-types`, or the controller. A passing test here does not prove
//! the matching product function.

pub mod compensation;
pub mod disk_layout;
pub mod nix_sanitize;
pub mod path_validator;
pub mod replication;
pub mod scheduler;
pub mod security_group;

#[cfg(kani)]
mod proofs;
