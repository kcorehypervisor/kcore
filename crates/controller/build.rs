fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("..").join("..").join("proto");
    let controller_proto = proto_dir.join("controller.proto");
    let node_proto = proto_dir.join("node.proto");

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[controller_proto], std::slice::from_ref(&proto_dir))?;

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&[node_proto], &[proto_dir])?;

    let lock_path = manifest_dir.join("..").join("..").join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    let lock = std::fs::read_to_string(&lock_path)
        .map_err(|e| format!("reading {}: {e}", lock_path.display()))?;
    let document = cyclonedx_from_lockfile(&lock)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out_dir.join("crates.cdx.json"), document)?;

    Ok(())
}

/// CycloneDX 1.5 document of the resolved `Cargo.lock` graph.
///
/// This is the graph compiled into the controller so `ExportSbom` can answer
/// without a side file. It is not the Sigstore-signed release SBOM; that one
/// is produced by `scripts/sbom.sh` and served when `sbom.cratesFile` is set.
fn cyclonedx_from_lockfile(lock: &str) -> Result<String, String> {
    let pkgs = parse_lockfile(lock)?;
    if pkgs.len() < 100 {
        return Err(format!(
            "Cargo.lock parsed to only {} packages; the parser is probably wrong",
            pkgs.len()
        ));
    }

    let mut by_name: std::collections::HashMap<&str, Vec<&Pkg>> = std::collections::HashMap::new();
    for pkg in &pkgs {
        by_name.entry(pkg.name.as_str()).or_default().push(pkg);
    }

    let root = pkgs
        .iter()
        .find(|p| p.name == "kcore-controller")
        .ok_or("Cargo.lock has no kcore-controller package")?;
    let root_ref = bom_ref(&root.name, &root.version);

    let mut seen = std::collections::HashSet::new();
    let mut components = String::new();
    let mut dependencies = String::new();
    let mut first_component = true;
    let mut first_dep = true;

    for pkg in &pkgs {
        let pkg_ref = bom_ref(&pkg.name, &pkg.version);
        if !seen.insert(pkg_ref.clone()) {
            return Err(format!("duplicate package {pkg_ref} in Cargo.lock"));
        }
        let mut depends_on = Vec::new();
        for spec in &pkg.deps {
            let dep = resolve(&by_name, spec)?;
            depends_on.push(bom_ref(&dep.name, &dep.version));
        }
        if !first_dep {
            dependencies.push(',');
        }
        first_dep = false;
        dependencies.push_str(&format!(
            "{{\"ref\":\"{}\",\"dependsOn\":[{}]}}",
            json_escape(&pkg_ref),
            depends_on
                .iter()
                .map(|r| format!("\"{}\"", json_escape(r)))
                .collect::<Vec<_>>()
                .join(",")
        ));

        if pkg.name == "kcore-controller" && pkg.version == root.version {
            continue;
        }
        if !first_component {
            components.push(',');
        }
        first_component = false;
        components.push_str(&format!(
            "{{\"type\":\"library\",\"name\":\"{}\",\"version\":\"{}\",\"bom-ref\":\"{}\",\"purl\":\"pkg:cargo/{}@{}\",\"scope\":\"required\"}}",
            json_escape(&pkg.name),
            json_escape(&pkg.version),
            json_escape(&pkg_ref),
            json_escape(&pkg.name),
            json_escape(&pkg.version),
        ));
    }

    let document = format!(
        "{{\"bomFormat\":\"CycloneDX\",\"specVersion\":\"1.5\",\"version\":1,\"metadata\":{{\"component\":{{\"type\":\"application\",\"name\":\"kcore-controller\",\"version\":\"{}\",\"bom-ref\":\"{}\",\"purl\":\"pkg:cargo/kcore-controller@{}\"}}}},\"components\":[{components}],\"dependencies\":[{dependencies}]}}",
        json_escape(&root.version),
        json_escape(&root_ref),
        json_escape(&root.version),
    );
    Ok(document)
}

struct Pkg {
    name: String,
    version: String,
    deps: Vec<String>,
}

fn parse_lockfile(text: &str) -> Result<Vec<Pkg>, String> {
    let mut pkgs = Vec::new();
    let mut cur: Option<Pkg> = None;
    let mut in_deps = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line == "[[package]]" {
            if let Some(pkg) = cur.take() {
                if !pkg.name.is_empty() {
                    pkgs.push(pkg);
                }
            }
            cur = Some(Pkg {
                name: String::new(),
                version: String::new(),
                deps: Vec::new(),
            });
            in_deps = false;
            continue;
        }
        let Some(pkg) = cur.as_mut() else {
            continue;
        };
        if in_deps {
            if line == "]" {
                in_deps = false;
                continue;
            }
            let spec = line.trim_end_matches(',').trim();
            let spec = spec
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .ok_or_else(|| format!("bad dependency line: {line}"))?;
            if !spec.is_empty() {
                pkg.deps.push(spec.to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("name = ") {
            pkg.name = unquote(rest)?;
        } else if let Some(rest) = line.strip_prefix("version = ") {
            pkg.version = unquote(rest)?;
        } else if line == "dependencies = [" {
            in_deps = true;
        }
    }
    if let Some(pkg) = cur {
        if !pkg.name.is_empty() {
            pkgs.push(pkg);
        }
    }
    Ok(pkgs)
}

fn unquote(value: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches(',');
    let inner = value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| format!("expected quoted string, got {value}"))?;
    Ok(inner.to_string())
}

fn bom_ref(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

fn resolve<'a>(
    by_name: &std::collections::HashMap<&str, Vec<&'a Pkg>>,
    spec: &str,
) -> Result<&'a Pkg, String> {
    let mut parts = spec.split_whitespace();
    let name = parts
        .next()
        .ok_or_else(|| format!("empty dependency spec '{spec}'"))?;
    let version = parts.next().filter(|v| !v.starts_with('('));
    let cands = by_name
        .get(name)
        .ok_or_else(|| format!("Cargo.lock dependency '{spec}' does not match a package"))?;
    if let Some(version) = version {
        return cands
            .iter()
            .copied()
            .find(|p| p.version == version)
            .ok_or_else(|| format!("Cargo.lock dependency '{spec}' does not match a package"));
    }
    if cands.len() == 1 {
        return Ok(cands[0]);
    }
    Err(format!("Cargo.lock dependency '{spec}' is ambiguous"))
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
