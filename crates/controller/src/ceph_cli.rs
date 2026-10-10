//! Local `ceph` CLI helpers for reconcilers. Fail gracefully when the binary
//! is missing (developer laptops without a Ceph lab).

use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CephCliError {
    NotInstalled,
    Failed(String),
}

impl std::fmt::Display for CephCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CephCliError::NotInstalled => write!(f, "ceph CLI not found on PATH"),
            CephCliError::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

pub fn ceph_available() -> bool {
    Command::new("ceph")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn run_ceph(args: &[&str]) -> Result<String, CephCliError> {
    if !ceph_available() {
        return Err(CephCliError::NotInstalled);
    }
    let output = Command::new("ceph")
        .args(args)
        .output()
        .map_err(|e| CephCliError::Failed(e.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(CephCliError::Failed(if stderr.is_empty() {
            format!("ceph {} failed", args.join(" "))
        } else {
            stderr
        }))
    }
}

/// Parse `ceph --version` / `ceph version` output. Ceph 20 (Tentacle) and 19
/// (Squid) both use `ceph version X.Y.Z ...`.
pub fn parse_ceph_version_line(line: &str) -> Option<(u32, u32, u32)> {
    // Examples:
    //   ceph version 20.2.4 (....) tentacle (stable)
    //   ceph version 19.2.3 (....) squid (stable)
    let rest = line.trim().strip_prefix("ceph version ")?;
    let ver = rest.split_whitespace().next()?;
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

pub fn run_rados(args: &[&str]) -> Result<String, CephCliError> {
    if !Command::new("rados")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Err(CephCliError::NotInstalled);
    }
    let output = Command::new("rados")
        .args(args)
        .output()
        .map_err(|e| CephCliError::Failed(e.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(CephCliError::Failed(if stderr.is_empty() {
            format!("rados {} failed", args.join(" "))
        } else {
            stderr
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_ceph_version_line;

    #[test]
    fn parses_ceph_20_tentacle_and_19_squid() {
        assert_eq!(
            parse_ceph_version_line("ceph version 20.2.4 (abcdef) tentacle (stable)"),
            Some((20, 2, 4))
        );
        assert_eq!(
            parse_ceph_version_line("ceph version 19.2.3 (deadbeef) squid (stable)"),
            Some((19, 2, 3))
        );
        assert!(parse_ceph_version_line("not a version").is_none());
    }
}
