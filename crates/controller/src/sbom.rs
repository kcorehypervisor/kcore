//! SBOM documents served by `ExportSbom`.
//!
//! Two sources, in order:
//!
//! 1. A file configured on the controller (`sbom.cratesFile` or
//!    `sbom.isoClosureFile`). That is how an installed release SBOM — the one
//!    covered by the Sigstore signature over `SHA256SUMS` — is handed to an
//!    auditor without leaving the cluster API.
//! 2. For `crates` only, the CycloneDX document `build.rs` generated from
//!    `Cargo.lock` and compiled into the binary. That graph is always
//!    available. It is not the signed release asset, and the response says so.

use std::path::Path;

use tonic::Status;

use crate::config::SbomConfig;
use crate::crypto_profile::SIGNING_SCHEME;

const EMBEDDED_CRATES_SBOM: &str = include_str!(concat!(env!("OUT_DIR"), "/crates.cdx.json"));

/// Refuse to pull an unbounded file into an RPC response.
const MAX_DOCUMENT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbomDocument {
    pub format: String,
    pub spec_version: String,
    pub filename: String,
    pub sha256: String,
    pub document: Vec<u8>,
    pub source: String,
    pub generator: String,
    pub signature_present: bool,
    pub signing_scheme: String,
}

pub fn load(kind: &str, cfg: &SbomConfig) -> Result<SbomDocument, Status> {
    let kind = kind.trim();
    match kind {
        "" | "crates" | "crate" => load_crates(cfg),
        "iso-closure" | "iso" => load_iso(cfg),
        other => Err(Status::invalid_argument(format!(
            "unknown SBOM kind '{other}' (expected 'crates' or 'iso-closure')"
        ))),
    }
}

fn load_crates(cfg: &SbomConfig) -> Result<SbomDocument, Status> {
    let path = cfg.crates_file.trim();
    if !path.is_empty() {
        return load_file(path, "crates");
    }
    let bytes = EMBEDDED_CRATES_SBOM.as_bytes();
    Ok(SbomDocument {
        format: "CycloneDX".into(),
        spec_version: spec_version_of(EMBEDDED_CRATES_SBOM),
        filename: format!("kcore-{}-crates.cdx.json", env!("CARGO_PKG_VERSION")),
        sha256: sha256_hex(bytes),
        document: bytes.to_vec(),
        source: "embedded".into(),
        generator: "kcore-lockfile".into(),
        signature_present: false,
        signing_scheme: SIGNING_SCHEME.into(),
    })
}

fn load_iso(cfg: &SbomConfig) -> Result<SbomDocument, Status> {
    let path = cfg.iso_closure_file.trim();
    if path.is_empty() {
        return Err(Status::not_found(
            "iso-closure SBOM is not installed on this controller (set sbom.isoClosureFile)",
        ));
    }
    load_file(path, "iso-closure")
}

fn load_file(path: &str, label: &str) -> Result<SbomDocument, Status> {
    crate::path_safety::assert_safe_path(path, label)
        .map_err(|e| Status::invalid_argument(e.to_string()))?;
    let meta = std::fs::metadata(path)
        .map_err(|e| Status::not_found(format!("reading {label} SBOM '{path}': {e}")))?;
    if meta.len() > MAX_DOCUMENT_BYTES {
        return Err(Status::resource_exhausted(format!(
            "{label} SBOM '{path}' is {} bytes, over the {} byte limit",
            meta.len(),
            MAX_DOCUMENT_BYTES
        )));
    }
    let document = std::fs::read(path)
        .map_err(|e| Status::internal(format!("reading {label} SBOM '{path}': {e}")))?;
    let text = String::from_utf8_lossy(&document);
    let filename = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sbom.json")
        .to_string();
    let format = if filename.ends_with(".spdx.json") || text.contains("SPDXID") {
        "SPDX"
    } else {
        "CycloneDX"
    };
    Ok(SbomDocument {
        format: format.into(),
        spec_version: spec_version_of(&text),
        filename,
        sha256: sha256_hex(&document),
        signature_present: signature_beside(path),
        document,
        source: "file".into(),
        generator: "installed-file".into(),
        signing_scheme: SIGNING_SCHEME.into(),
    })
}

fn signature_beside(path: &str) -> bool {
    let file = Path::new(path);
    if file.with_extension("sigstore.json").is_file() {
        return true;
    }
    let sibling = format!("{path}.sigstore.json");
    if Path::new(&sibling).is_file() {
        return true;
    }
    file.parent()
        .map(|dir| dir.join("SHA256SUMS.sigstore.json").is_file())
        .unwrap_or(false)
}

fn spec_version_of(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.get("specVersion")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".into())
}

fn sha256_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let dig = crate::pki::sha256(data);
    let mut s = String::with_capacity(dig.len() * 2);
    for b in dig {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

pub fn embedded_component_count() -> usize {
    serde_json::from_str::<serde_json::Value>(EMBEDDED_CRATES_SBOM)
        .ok()
        .and_then(|v| {
            v.get("components")
                .and_then(|c| c.as_array())
                .map(|a| a.len())
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_crates_sbom_is_cyclonedx_with_a_real_graph() {
        let doc = load("crates", &SbomConfig::default()).expect("embedded");
        assert_eq!(doc.source, "embedded");
        assert_eq!(doc.format, "CycloneDX");
        assert_eq!(doc.spec_version, "1.5");
        assert!(!doc.signature_present);
        assert!(doc.sha256.len() == 64);
        assert!(
            embedded_component_count() >= 100,
            "component count {}",
            embedded_component_count()
        );
        let parsed: serde_json::Value =
            serde_json::from_slice(&doc.document).expect("embedded json");
        assert_eq!(parsed["bomFormat"], "CycloneDX");
        assert_eq!(parsed["metadata"]["component"]["name"], "kcore-controller");
    }

    #[test]
    fn installed_file_is_preferred_and_a_bundle_marks_the_signature() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sbom = dir.path().join("kcore-crates.cdx.json");
        std::fs::write(&sbom, br#"{"bomFormat":"CycloneDX","specVersion":"1.5"}"#).expect("sbom");
        std::fs::write(dir.path().join("SHA256SUMS.sigstore.json"), b"{}").expect("sig");
        let cfg = SbomConfig {
            crates_file: sbom.display().to_string(),
            iso_closure_file: String::new(),
        };
        let doc = load("crates", &cfg).expect("file");
        assert_eq!(doc.source, "file");
        assert!(doc.signature_present);
        assert_eq!(doc.filename, "kcore-crates.cdx.json");
        assert_ne!(
            doc.sha256,
            load("crates", &SbomConfig::default())
                .expect("embedded")
                .sha256
        );
    }

    #[test]
    fn iso_closure_without_a_file_is_not_found() {
        let err = load("iso-closure", &SbomConfig::default()).expect_err("missing");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let err = load("spdx", &SbomConfig::default()).expect_err("kind");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
