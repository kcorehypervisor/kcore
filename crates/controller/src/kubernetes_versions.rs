//! Kubernetes versions a `KubernetesClusterSpec.version` may ask for.
//!
//! `modules/kubernetes-releases.json` is read here and by the Nix module, so
//! the controller cannot accept a version the nodes cannot build. Empty means
//! the host nixpkgs pin, which the file records as `hostPin`.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

const RELEASES_JSON: &str = include_str!("../../../modules/kubernetes-releases.json");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Releases {
    host_pin: String,
    releases: BTreeMap<String, BTreeMap<String, String>>,
}

fn releases() -> &'static Releases {
    static RELEASES: OnceLock<Releases> = OnceLock::new();
    RELEASES.get_or_init(|| {
        serde_json::from_str(RELEASES_JSON).expect("modules/kubernetes-releases.json is valid")
    })
}

/// Version of `pkgs.kubernetes` in the host nixpkgs pin.
pub fn host_pin() -> &'static str {
    &releases().host_pin
}

pub fn normalize(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}

/// The version a node runs for `version`: empty is the host pin.
pub fn effective(version: &str) -> String {
    let v = normalize(version);
    if v.is_empty() {
        host_pin().to_string()
    } else {
        v
    }
}

/// Every version an apply may set, oldest first.
pub fn allowed() -> Vec<String> {
    let mut all: Vec<String> = releases().releases.keys().cloned().collect();
    all.push(host_pin().to_string());
    all.sort_by(|a, b| compare(a, b));
    all.dedup();
    all
}

pub fn validate(version: &str) -> Result<(), String> {
    let v = effective(version);
    if v == host_pin() || releases().releases.contains_key(&v) {
        return Ok(());
    }
    Err(format!(
        "kubernetes version {v} is not available ({}); leave version empty to use the host pin {}",
        allowed().join(", "),
        host_pin()
    ))
}

fn parts(version: &str) -> Option<(u32, u32, u32)> {
    let v = normalize(version);
    let mut it = v.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn compare(a: &str, b: &str) -> Ordering {
    parts(&effective(a)).cmp(&parts(&effective(b)))
}

/// Kubernetes supports moving one minor at a time and never downgrades a
/// control plane. `force` lifts both rules; a major change is always refused.
pub fn check_upgrade(from: &str, to: &str, force: bool) -> Result<(), String> {
    let from_v = effective(from);
    let to_v = effective(to);
    if from_v == to_v {
        return Ok(());
    }
    validate(&to_v)?;
    let (Some((from_major, from_minor, _)), Some((to_major, to_minor, _))) =
        (parts(&from_v), parts(&to_v))
    else {
        return Err(format!("cannot compare Kubernetes {from_v} with {to_v}"));
    };
    if from_major != to_major {
        return Err(format!("refusing major version change {from_v} -> {to_v}"));
    }
    if force {
        return Ok(());
    }
    if compare(&to_v, &from_v) == Ordering::Less {
        return Err(format!(
            "refusing downgrade {from_v} -> {to_v}; pass --force to override"
        ));
    }
    if to_minor > from_minor + 1 {
        return Err(format!(
            "refusing to skip a minor version {from_v} -> {to_v}; upgrade to {to_major}.{} first, or pass --force",
            from_minor + 1
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_the_host_pin_and_the_pin_is_allowed() {
        assert_eq!(effective(""), host_pin());
        assert_eq!(effective(" v1.36.6 "), "1.36.6");
        assert!(validate("").is_ok());
        assert!(validate(host_pin()).is_ok());
        assert!(validate("9.9.9").is_err());
        assert!(allowed().contains(&host_pin().to_string()));
    }

    #[test]
    fn every_release_lists_every_binary() {
        let value: serde_json::Value = serde_json::from_str(RELEASES_JSON).unwrap();
        let binaries: Vec<&str> = value["binaries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap())
            .collect();
        for (version, hashes) in &releases().releases {
            assert!(parts(version).is_some(), "{version}");
            for bin in &binaries {
                let hash = hashes
                    .get(*bin)
                    .unwrap_or_else(|| panic!("{version} {bin}"));
                assert!(hash.starts_with("sha256-"), "{version} {bin}");
            }
        }
    }

    #[test]
    fn allowed_is_sorted_by_version_not_text() {
        let all = allowed();
        let mut sorted = all.clone();
        sorted.sort_by(|a, b| compare(a, b));
        assert_eq!(all, sorted);
        assert_eq!(compare("1.35.10", "1.35.2"), Ordering::Greater);
    }

    #[test]
    fn one_minor_up_is_allowed_and_a_skip_needs_force() {
        assert!(check_upgrade("", "1.36.6", false).is_ok());
        assert!(check_upgrade("1.35.2", "1.35.10", false).is_ok());
        assert!(check_upgrade("", "1.37.2", false).is_err());
        assert!(check_upgrade("", "1.37.2", true).is_ok());
    }

    #[test]
    fn any_downgrade_needs_force() {
        assert!(check_upgrade("1.36.6", "", false).is_err());
        assert!(check_upgrade("1.35.10", "1.35.2", false).is_err());
        assert!(check_upgrade("1.37.2", "1.36.6", false).is_err());
        assert!(check_upgrade("1.37.2", "1.36.6", true).is_ok());
    }

    #[test]
    fn empty_and_the_pin_are_the_same_version() {
        assert!(check_upgrade("", host_pin(), false).is_ok());
        assert_eq!(compare("", host_pin()), Ordering::Equal);
    }
}
