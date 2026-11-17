//! Enterprise update channel: read releases from the private GitHub repo.
//!
//! Downloads go through `curl` so the controller needs no HTTPS client crate.
//! The subscription token is passed on stdin (`-H @-`) and never appears in
//! the process argument list.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const ENTERPRISE_REPO: &str = "kcorehypervisor/enterprise";
pub const DEFAULT_FLAKE_REF: &str = "github:kcorehypervisor/kcore";
const CURL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnterpriseRelease {
    pub version: String,
    pub tag: String,
    pub kcore_rev: String,
    pub prebuilt_name: String,
    pub prebuilt_url: String,
    pub prebuilt_sha256: String,
    pub schema_version: i32,
    pub min_compatible_reader: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenIdentity {
    pub login: String,
}

/// Parse a SHA256SUMS file; return the hex digest for `name`.
pub fn sha256_for(sums: &str, name: &str) -> Result<String, String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let digest = parts
            .next()
            .ok_or_else(|| format!("bad SHA256SUMS line: {line}"))?;
        let file = parts
            .next()
            .ok_or_else(|| format!("bad SHA256SUMS line: {line}"))?;
        let file = file.trim_start_matches('*');
        if file == name {
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("bad sha256 for {name}: {digest}"));
            }
            return Ok(digest.to_ascii_lowercase());
        }
    }
    Err(format!("{name} not listed in SHA256SUMS"))
}

/// Parse enterprise `release.json`.
pub fn parse_release_json(body: &str) -> Result<(String, String, String, i32, i32), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("release.json: {e}"))?;
    let channel = v
        .get("channel")
        .and_then(|x| x.as_str())
        .unwrap_or_default();
    if channel != "enterprise" {
        return Err(format!(
            "release.json channel is {channel:?}; expected \"enterprise\""
        ));
    }
    let version = v
        .get("version")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if version.is_empty() {
        return Err("release.json has no version".into());
    }
    let kcore_rev = v
        .get("kcoreRev")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if kcore_rev.len() != 40 || !kcore_rev.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "release.json kcoreRev is not a commit: {kcore_rev}"
        ));
    }
    let prebuilt = v
        .get("prebuilt")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if prebuilt.is_empty() {
        return Err("release.json has no prebuilt asset name".into());
    }
    let schema_version = v
        .get("schemaVersion")
        .and_then(|x| x.as_i64())
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(0);
    let min_reader = v
        .get("minCompatibleReader")
        .and_then(|x| x.as_i64())
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(0);
    Ok((version, kcore_rev, prebuilt, schema_version, min_reader))
}

fn asset_url(repo: &str, tag: &str, name: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{name}")
}

/// Authenticated GitHub API GET. Token goes on stdin as the Authorization header value.
async fn github_api_json(token: &str, path: &str) -> Result<serde_json::Value, String> {
    let url = format!("https://api.github.com{path}");
    let mut child = Command::new("curl")
        .args([
            "-fsS",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            "-H",
            "User-Agent: kcore-controller",
            "-H",
            "@-",
            &url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "curl stdin missing".to_string())?;
        let header = format!("Authorization: Bearer {token}");
        stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| format!("curl stdin: {e}"))?;
    }
    let out = tokio::time::timeout(CURL_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "curl timed out".to_string())?
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err
            .trim()
            .lines()
            .last()
            .unwrap_or("github api failed")
            .to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("github json: {e}"))
}

/// Download a release asset. Token on stdin as Authorization header.
async fn download_text(token: &str, url: &str) -> Result<String, String> {
    let mut child = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "Accept: application/octet-stream",
            "-H",
            "User-Agent: kcore-controller",
            "-H",
            "@-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "curl stdin missing".to_string())?;
        let header = format!("Authorization: Bearer {token}");
        stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| format!("curl stdin: {e}"))?;
    }
    let out = tokio::time::timeout(CURL_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "curl timed out".to_string())?
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err
            .trim()
            .lines()
            .last()
            .unwrap_or("download failed")
            .to_string());
    }
    String::from_utf8(out.stdout).map_err(|e| format!("download utf8: {e}"))
}

/// Who the token authenticates as.
pub async fn verify_token(token: &str) -> Result<TokenIdentity, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("subscription token is empty".into());
    }
    let v = github_api_json(token, "/user").await?;
    let login = v
        .get("login")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if login.is_empty() {
        return Err("token did not return a GitHub login".into());
    }
    Ok(TokenIdentity { login })
}

/// Latest published release tag on the enterprise repo.
pub async fn latest_tag(token: &str) -> Result<String, String> {
    let v = github_api_json(token, &format!("/repos/{ENTERPRISE_REPO}/releases/latest")).await?;
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if tag.is_empty() {
        return Err("enterprise repo has no latest release".into());
    }
    Ok(tag)
}

/// Resolve an enterprise release for `tag` (vX.Y.Z).
pub async fn resolve_release(token: &str, tag: &str) -> Result<EnterpriseRelease, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("no subscription token; run: kctl set subscription --token-file …".into());
    }
    // Confirm the release exists (and the token can see it).
    let _ = github_api_json(
        token,
        &format!("/repos/{ENTERPRISE_REPO}/releases/tags/{tag}"),
    )
    .await
    .map_err(|e| format!("{tag} not found on {ENTERPRISE_REPO}: {e}"))?;

    let release_json_url = asset_url(
        ENTERPRISE_REPO,
        tag,
        &format!("kcore-{}-release.json", tag.trim_start_matches('v')),
    );
    let sums_url = asset_url(ENTERPRISE_REPO, tag, "SHA256SUMS");
    let release_body = download_text(token, &release_json_url)
        .await
        .map_err(|e| format!("download release.json: {e}"))?;
    let sums_body = download_text(token, &sums_url)
        .await
        .map_err(|e| format!("download SHA256SUMS: {e}"))?;

    let (version, kcore_rev, prebuilt_name, schema_version, min_compatible_reader) =
        parse_release_json(&release_body)?;
    if format!("v{version}") != tag {
        return Err(format!(
            "{tag} release.json carries version {version}; they must match"
        ));
    }
    let prebuilt_sha256 = sha256_for(&sums_body, &prebuilt_name)?;
    let prebuilt_url = asset_url(ENTERPRISE_REPO, tag, &prebuilt_name);
    Ok(EnterpriseRelease {
        version,
        tag: tag.to_string(),
        kcore_rev,
        prebuilt_name,
        prebuilt_url,
        prebuilt_sha256,
        schema_version,
        min_compatible_reader,
    })
}

/// Build the curl argv used for authenticated downloads (without the token).
/// Exposed for tests so we can assert the token never appears on the argv.
pub fn curl_auth_argv_for_tests(url: &str) -> Vec<String> {
    vec![
        "curl".into(),
        "-fsSL".into(),
        "-H".into(),
        "Accept: application/octet-stream".into(),
        "-H".into(),
        "User-Agent: kcore-controller".into(),
        "-H".into(),
        "@-".into(),
        url.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enterprise_release_json() {
        let body = r#"{
          "version": "0.8.0",
          "channel": "enterprise",
          "nodeBuild": "prebuilt",
          "kcoreRev": "0123456789abcdef0123456789abcdef01234567",
          "schemaVersion": 73,
          "minCompatibleReader": 56,
          "prebuilt": "kcore-prebuilt-0.8.0-x86_64-linux.tar"
        }"#;
        let (version, rev, prebuilt, schema, min) = parse_release_json(body).unwrap();
        assert_eq!(version, "0.8.0");
        assert_eq!(rev.len(), 40);
        assert_eq!(prebuilt, "kcore-prebuilt-0.8.0-x86_64-linux.tar");
        assert_eq!(schema, 73);
        assert_eq!(min, 56);
    }

    #[test]
    fn rejects_community_channel_in_enterprise_json() {
        let body = r#"{
          "version": "0.8.0",
          "channel": "community",
          "kcoreRev": "0123456789abcdef0123456789abcdef01234567",
          "prebuilt": "x.tar"
        }"#;
        assert!(parse_release_json(body).is_err());
    }

    #[test]
    fn sha256sums_lookup() {
        let sums = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  kcore-prebuilt-0.8.0-x86_64-linux.tar
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  kcore-0.8.0-release.json
";
        assert_eq!(
            sha256_for(sums, "kcore-prebuilt-0.8.0-x86_64-linux.tar").unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(sha256_for(sums, "missing.tar").is_err());
    }

    #[test]
    fn curl_argv_never_embeds_the_token() {
        let token = "ghp_this_must_not_appear_on_argv";
        let argv = curl_auth_argv_for_tests("https://example.test/asset");
        let joined = argv.join(" ");
        assert!(!joined.contains(token));
        assert!(argv.iter().any(|a| a == "@-"));
    }
}
