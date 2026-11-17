//! Constants for the enterprise update channel.
//!
//! The signing key's public half is fixed here so prepare can pass it as
//! `--extra-trusted-public-keys` when substituting from a release's prebuilt
//! file cache. The private key lives only on the release machine
//! (`KCORE_NIX_SIGNING_KEY_FILE`); it is never shipped in any package.

/// Nix binary-cache public key for signed enterprise prebuilt closures.
pub const NIX_PUBLIC_KEY: &str = "kcore-enterprise-1:yyQ6GouFaVTtgz43+pgHz/QRIg6quZPyJzAFosfJP8Y=";

/// GitHub repository that publishes enterprise releases (artifacts only).
pub const ENTERPRISE_GITHUB_REPO: &str = "kcorehypervisor/enterprise";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_key_has_name_and_base64() {
        let (name, key) = NIX_PUBLIC_KEY.split_once(':').expect("name:base64 form");
        assert_eq!(name, "kcore-enterprise-1");
        assert!(!key.is_empty());
        assert!(key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }
}
