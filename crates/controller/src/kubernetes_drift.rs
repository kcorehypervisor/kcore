//! Drift detection for rendered node Nix.
//!
//! The orchestrator stores the hash of the Nix it last applied to a node. A
//! different hash for the same module version means the render changed under
//! the node, for example after a controller upgrade.

use aws_lc_rs::digest;

pub fn rendered_hash(nix: &str, module_version: &str) -> String {
    let input = format!("{module_version}\n{nix}");
    digest::digest(&digest::SHA256, input.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An empty stored hash means nothing was applied yet, which is not drift.
pub fn drift_detected(stored: &str, current: &str) -> bool {
    !stored.is_empty() && stored != current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_sha256_hex() {
        let hash = rendered_hash("", "");
        assert_eq!(hash.len(), 64);
        assert_eq!(
            hash,
            "01ba4719c80b6fe911b091a7c05124b64eeece964e09c058ef8f9805daca546b"
        );
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(rendered_hash("{ }", "v1"), rendered_hash("{ }", "v1"));
    }

    #[test]
    fn nix_change_changes_hash() {
        assert_ne!(
            rendered_hash("{ a = 1; }", "v1"),
            rendered_hash("{ a = 2; }", "v1")
        );
    }

    #[test]
    fn module_version_changes_hash() {
        assert_ne!(rendered_hash("{ }", "v1"), rendered_hash("{ }", "v2"));
    }

    #[test]
    fn equal_hashes_are_not_drift() {
        let h = rendered_hash("{ }", "v1");
        assert!(!drift_detected(&h, &h));
    }

    #[test]
    fn different_hashes_are_drift() {
        assert!(drift_detected(
            &rendered_hash("{ }", "v1"),
            &rendered_hash("{ a = 1; }", "v1")
        ));
    }

    #[test]
    fn missing_stored_hash_is_not_drift() {
        assert!(!drift_detected("", &rendered_hash("{ }", "v1")));
    }
}
