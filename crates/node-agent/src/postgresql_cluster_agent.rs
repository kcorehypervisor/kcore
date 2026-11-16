//! PostgreSQL cluster member on this node: values file, secrets, preflight,
//! status, and the closed set of admin commands.
//!
//! The module (`modules/kcore-postgresql-cluster.nix`) is imported by the host
//! module; this agent writes `/etc/nixos/kcore-postgresql-cluster.nix` and the
//! files under `/var/lib/kcore-postgresql/<cluster>`. Patroni is reached on
//! 127.0.0.1 only. SQL is built here from validated names by
//! `kcore_postgresql::ops`; the controller never sends SQL text.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

use base64::Engine as _;
use kcore_postgresql::nixgen::{self, MemberFragment, Stage};
use kcore_postgresql::ops;
use kcore_postgresql::spec::{self, ClusterSpec};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::proto;

pub const VALUES_FILE: &str = nixgen::VALUES_FILE;
const SW: &str = "/run/current-system/sw/bin";
const PG_CURRENT: &str = "/etc/kcore-postgresql/current/bin";
const PG_PREVIOUS: &str = "/etc/kcore-postgresql/previous/bin";
const PATRONI_YML: &str = "/etc/kcore-postgresql/patroni.yml";
const VIP_FILE: &str = "/etc/kcore-postgresql/vip";
const SOCKET_DIR: &str = "/run/kcore-postgresql";
const OS_USER: &str = "kcore-pg";
const NAMESPACE: &str = "/kcore-postgresql/";

fn bin(name: &str) -> String {
    format!("{SW}/{name}")
}

/// Parse and re-validate the controller's spec. The agent does not trust the
/// controller to have validated it.
pub fn parse_spec(cluster: &str, spec_json: &str) -> Result<ClusterSpec, String> {
    let mut spec: ClusterSpec =
        serde_json::from_str(spec_json).map_err(|e| format!("spec_json: {e}"))?;
    spec::normalize(&mut spec);
    let blocking: Vec<String> = spec::validate(cluster, &spec)
        .into_iter()
        .filter(|i| i.blocking)
        .map(|i| i.message)
        .collect();
    if !blocking.is_empty() {
        return Err(blocking.join("; "));
    }
    Ok(spec)
}

pub fn stage_of(stage: &str) -> Result<(Stage, &'static str), String> {
    match stage {
        "" | "full" | "upgrade" | "rollback" => Ok((Stage::Full, "new")),
        "etcd-only" => Ok((Stage::EtcdOnly, "new")),
        "join" => Ok((Stage::Full, "existing")),
        other => Err(format!("unknown stage {other}")),
    }
}

/// Values file for an apply request. `None` stage "disable".
pub fn render_values(req: &proto::ApplyPostgresqlClusterConfigRequest) -> Result<String, String> {
    if req.stage == "disable" {
        return Ok(nixgen::render_disabled());
    }
    if !spec::valid_identifier(&req.cluster_name) {
        return Err("cluster_name is not a valid identifier".into());
    }
    let spec = parse_spec(&req.cluster_name, &req.spec_json)?;
    let (stage, etcd_state) = stage_of(&req.stage)?;
    let previous = match req.stage.as_str() {
        "upgrade" => {
            if !spec::PACKAGES.contains(&req.previous_package.as_str()) {
                return Err("upgrade stage needs previous_package".into());
            }
            Some(req.previous_package.as_str())
        }
        _ => None,
    };
    // A node that already joined keeps "existing" on later applies; etcd
    // ignores the flag once its data directory exists.
    let fragment = MemberFragment::from_spec(
        &req.cluster_name,
        req.generation,
        &spec,
        &req.node_id,
        &req.etcd_voters,
        etcd_state,
        previous,
    )
    .ok_or_else(|| format!("{} is not a member of the spec", req.node_id))?;
    Ok(nixgen::render_member(&fragment, stage))
}

fn write_private(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Write passwords and TLS material. Files are root-owned until a unit's
/// privileged ExecStartPre hands them to kcore-pg; when the user already
/// exists they are handed over now so a running server keeps reading them.
pub async fn write_secrets(cluster: &str, s: &proto::PostgresqlNodeSecrets) -> Result<(), String> {
    let secrets = nixgen::secrets_dir(cluster);
    let tls = nixgen::tls_dir(cluster);
    let files: [(&str, &str, &[u8], u32); 7] = [
        (
            &secrets,
            "superuser.password",
            s.superuser_password.as_bytes(),
            0o400,
        ),
        (
            &secrets,
            "replication.password",
            s.replication_password.as_bytes(),
            0o400,
        ),
        (
            &secrets,
            "rewind.password",
            s.rewind_password.as_bytes(),
            0o400,
        ),
        (
            &secrets,
            "restapi.password",
            s.restapi_password.as_bytes(),
            0o400,
        ),
        (&tls, "node.crt", &s.tls_cert_pem, 0o444),
        (&tls, "node.key", &s.tls_key_pem, 0o400),
        (&tls, "ca.crt", &s.tls_ca_pem, 0o444),
    ];
    for (dir, name, bytes, mode) in files {
        if bytes.is_empty() {
            return Err(format!("secret {name} is empty"));
        }
        write_private(&Path::new(dir).join(name), bytes, mode)?;
    }
    if user_exists().await {
        for dir in [&secrets, &tls] {
            let _ = Command::new(bin("chown"))
                .args(["-R", &format!("{OS_USER}:{OS_USER}"), dir])
                .status()
                .await;
        }
    }
    Ok(())
}

pub fn remove_secrets(cluster: &str) {
    let _ = std::fs::remove_dir_all(nixgen::secrets_dir(cluster));
    let _ = std::fs::remove_dir_all(nixgen::tls_dir(cluster));
}

async fn user_exists() -> bool {
    Command::new(bin("id"))
        .arg(OS_USER)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub async fn unit_active(unit: &str) -> bool {
    Command::new(bin("systemctl"))
        .args(["is-active", "--quiet", unit])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_secret(cluster: &str, name: &str) -> Result<String, String> {
    std::fs::read_to_string(Path::new(&nixgen::secrets_dir(cluster)).join(name))
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("reading {name}: {e}"))
}

fn tls_on(cluster: &str) -> bool {
    Path::new(&nixgen::tls_dir(cluster)).join("ca.crt").exists()
}

/// Port of the Patroni REST API from the live config, else the default.
fn api_port() -> u16 {
    patroni_yaml()
        .and_then(|v| {
            v.get("restapi")?
                .get("listen")?
                .as_str()?
                .rsplit(':')
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(spec::DEFAULT_PORTS.patroni_api)
}

fn patroni_yaml() -> Option<serde_yaml::Value> {
    serde_yaml::from_str(&std::fs::read_to_string(PATRONI_YML).ok()?).ok()
}

/// Call the local Patroni REST API. Credentials go to curl on stdin, never
/// on the command line.
pub async fn patroni(
    cluster: &str,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<String, String> {
    let tls = tls_on(cluster);
    let url = format!(
        "{}://127.0.0.1:{}{path}",
        if tls { "https" } else { "http" },
        api_port()
    );
    let mut config = String::from("silent\nshow-error\nmax-time = 60\n");
    if method != "GET" {
        let password = read_secret(cluster, "restapi.password")?;
        config.push_str(&format!(
            "user = \"kcore:{}\"\n",
            password.replace('"', "\\\"")
        ));
    }
    let mut cmd = Command::new(bin("curl"));
    cmd.args(["-K", "-", "-X", method, "-w", "\n%{http_code}"]);
    if tls {
        cmd.args(["--cacert", &format!("{}/ca.crt", nixgen::tls_dir(cluster))]);
    }
    if let Some(b) = body {
        cmd.args(["-H", "Content-Type: application/json", "--data-binary"])
            .arg(b.to_string());
    }
    cmd.arg(&url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("curl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .await
            .map_err(|e| format!("curl stdin: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("curl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
    let code: u16 = code.trim().parse().unwrap_or(0);
    if code == 0 {
        return Err(format!(
            "{method} {path}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // GET /patroni answers 503 on a replica that is not running; the body is
    // still the status.
    if code >= 400 && !(method == "GET" && code == 503) {
        return Err(format!("{method} {path}: HTTP {code}: {}", body.trim()));
    }
    Ok(body.to_string())
}

fn psql_env(cmd: &mut Command, cluster: &str, port: u16) -> Result<(), String> {
    cmd.env("PGPASSWORD", read_secret(cluster, "superuser.password")?)
        .env("PGHOST", SOCKET_DIR)
        .env("PGPORT", port.to_string())
        .env("PGUSER", "postgres")
        .env("PGCONNECT_TIMEOUT", "10");
    Ok(())
}

/// Run one statement (or a `;`-separated batch without CREATE DATABASE) and
/// return unaligned `a|b` rows.
pub async fn psql(cluster: &str, port: u16, database: &str, sql: &str) -> Result<String, String> {
    let mut cmd = Command::new(format!("{PG_CURRENT}/psql"));
    psql_env(&mut cmd, cluster, port)?;
    cmd.args([
        "-X",
        "-q",
        "-A",
        "-t",
        "-v",
        "ON_ERROR_STOP=1",
        "-d",
        database,
        "-c",
        sql,
    ]);
    run(&mut cmd).await
}

/// SCRAM-SHA-256 verifier for a password kcore generated, so the plaintext
/// never reaches the server or its logs.
pub fn scram_verifier(password: &str) -> String {
    let mut salt = [0u8; 16];
    let _ = aws_lc_rs::rand::fill(&mut salt);
    scram_verifier_with_salt(password, &salt)
}

pub fn scram_verifier_with_salt(password: &str, salt: &[u8]) -> String {
    use aws_lc_rs::{digest, hmac, pbkdf2};
    const ITERATIONS: u32 = 4096;
    let mut salted = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        std::num::NonZeroU32::new(ITERATIONS).unwrap_or(std::num::NonZeroU32::MIN),
        salt,
        password.as_bytes(),
        &mut salted,
    );
    let key = hmac::Key::new(hmac::HMAC_SHA256, &salted);
    let client_key = hmac::sign(&key, b"Client Key");
    let stored_key = digest::digest(&digest::SHA256, client_key.as_ref());
    let server_key = hmac::sign(&key, b"Server Key");
    let b64 = base64::engine::general_purpose::STANDARD;
    format!(
        "SCRAM-SHA-256${ITERATIONS}:{}${}:{}",
        b64.encode(salt),
        b64.encode(stored_key.as_ref()),
        b64.encode(server_key.as_ref())
    )
}

pub async fn ensure_roles(
    cluster: &str,
    spec: &ClusterSpec,
    generated: &BTreeMap<String, String>,
    drop: &[String],
) -> Result<String, String> {
    let port = spec.ports.postgres;
    for role in &spec.roles {
        psql(
            cluster,
            port,
            "postgres",
            &ops::create_role_if_missing_sql(&role.name),
        )
        .await?;
        let verifier = if !role.password_scram.is_empty() {
            Some(role.password_scram.clone())
        } else {
            generated.get(&role.name).map(|p| scram_verifier(p))
        };
        psql(
            cluster,
            port,
            "postgres",
            &ops::alter_role_sql(role, verifier.as_deref()),
        )
        .await?;
    }
    for role in &spec.roles {
        for parent in &role.member_of {
            psql(
                cluster,
                port,
                "postgres",
                &ops::grant_role_sql(parent, &role.name),
            )
            .await?;
        }
    }
    for name in drop {
        if !spec::valid_identifier(name) || spec::RESERVED_ROLES.contains(&name.as_str()) {
            return Err(format!("refusing to drop role {name}"));
        }
        psql(cluster, port, "postgres", &ops::drop_role_sql(name)).await?;
    }
    Ok(format!(
        "{} roles ensured, {} dropped",
        spec.roles.len(),
        drop.len()
    ))
}

pub async fn ensure_databases(
    cluster: &str,
    spec: &ClusterSpec,
    drop: &[String],
) -> Result<String, String> {
    let port = spec.ports.postgres;
    for db in &spec.databases {
        let exists = psql(
            cluster,
            port,
            "postgres",
            &ops::database_exists_sql(&db.name),
        )
        .await?;
        if exists.trim().is_empty() {
            psql(cluster, port, "postgres", &ops::create_database_sql(db)).await?;
        }
        psql(
            cluster,
            port,
            "postgres",
            &ops::alter_database_owner_sql(db),
        )
        .await?;
    }
    for name in drop {
        if !spec::valid_identifier(name)
            || matches!(name.as_str(), "postgres" | "template0" | "template1")
        {
            return Err(format!("refusing to drop database {name}"));
        }
        psql(cluster, port, "postgres", &ops::drop_database_sql(name)).await?;
    }
    Ok(format!(
        "{} databases ensured, {} dropped",
        spec.databases.len(),
        drop.len()
    ))
}

pub async fn ensure_extensions(
    cluster: &str,
    spec: &ClusterSpec,
    update: bool,
) -> Result<String, String> {
    let port = spec.ports.postgres;
    let mut count = 0;
    for db in &spec.databases {
        for ext in &db.extensions {
            if spec::extension(ext).is_none() {
                return Err(format!("unknown extension {ext}"));
            }
            psql(cluster, port, &db.name, &ops::create_extension_sql(ext)).await?;
            if update {
                psql(cluster, port, &db.name, &ops::update_extension_sql(ext)).await?;
            }
            count += 1;
        }
    }
    Ok(format!("{count} extensions ensured"))
}

pub async fn analyze(cluster: &str, port: u16) -> Result<String, String> {
    let mut cmd = Command::new(format!("{PG_CURRENT}/vacuumdb"));
    psql_env(&mut cmd, cluster, port)?;
    cmd.args(["--all", "--analyze-in-stages"]);
    run(&mut cmd).await.map(|_| "analyze finished".into())
}

pub async fn systemctl(action: &str, unit: &str) -> Result<String, String> {
    if !matches!(action, "start" | "stop" | "restart") {
        return Err(format!(
            "unit action {action} is not start, stop or restart"
        ));
    }
    run(Command::new(bin("systemctl")).args([action, unit]))
        .await
        .map(|_| format!("{action} {unit}"))
}

pub fn unit_name(cluster: &str, unit: &str) -> Result<String, String> {
    match unit {
        "patroni" => Ok("patroni.service".into()),
        "etcd" => Ok(nixgen::etcd_unit(cluster)),
        other => Err(format!("unit {other} is not patroni or etcd")),
    }
}

fn data_dir(cluster: &str, major: u32) -> String {
    nixgen::data_path(cluster, major)
}

fn runuser(program: &str) -> Command {
    let mut cmd = Command::new(bin("runuser"));
    cmd.args(["-u", OS_USER, "--", program]);
    cmd
}

/// Stop a PostgreSQL that Patroni no longer manages (paused and stopped).
pub async fn stop_postgres(spec: &ClusterSpec, cluster: &str) -> Result<String, String> {
    let data = data_dir(cluster, spec.major());
    if !Path::new(&data).join("postmaster.pid").exists() {
        return Ok("not running".into());
    }
    run(runuser(&format!("{PG_CURRENT}/pg_ctl"))
        .args(["stop", "-D", &data, "-m", "fast", "-w", "-t", "600"]))
    .await
    .map(|_| format!("stopped {data}"))
}

fn upgrade_pair(from: &str, to: &str) -> Result<(u32, u32), String> {
    let old = spec::major_of(from).ok_or("from_package is not versioned")?;
    let new = spec::major_of(to).ok_or("to_package is not versioned")?;
    if new <= old {
        return Err(format!("{to} is not a newer major than {from}"));
    }
    Ok((old, new))
}

/// `true` when the data filesystem can `--clone` (XFS with reflink).
pub async fn reflink_supported(cluster: &str) -> bool {
    let mount = nixgen::data_mount(cluster);
    let fs = run(Command::new(bin("stat")).args(["-f", "-c", "%T", &mount]))
        .await
        .unwrap_or_default();
    if fs.trim() != "xfs" {
        return false;
    }
    run(Command::new(bin("xfs_info")).arg(&mount))
        .await
        .map(|o| o.contains("reflink=1"))
        .unwrap_or(false)
}

/// `pg_upgrade --check` against the running data directory. Before the
/// apply, the new binaries are not on the host yet, so the check builds them
/// with `nix build` of the pinned package (the closure is fetched or already
/// cached).
pub async fn pg_upgrade(
    cluster: &str,
    from: &str,
    to: &str,
    check: bool,
    port: u16,
) -> Result<String, String> {
    let (old, new) = upgrade_pair(from, to)?;
    let (old_bin, new_bin) = if check {
        (
            PG_CURRENT.to_string(),
            format!("{}/bin", resolve_package(to).await?),
        )
    } else {
        (PG_PREVIOUS.to_string(), PG_CURRENT.to_string())
    };
    let old_data = data_dir(cluster, old);
    let new_data = data_dir(cluster, new);
    let root = nixgen::cluster_root(cluster);
    let work = format!("{root}/upgrade-{new}");
    let pwfile = format!("{}/superuser.password", nixgen::secrets_dir(cluster));
    let _ = run(Command::new(bin("install"))
        .args(["-d", "-o", OS_USER, "-g", OS_USER, "-m", "0700", &work]))
    .await;
    let wal = Path::new(&nixgen::wal_mount(cluster)).exists();
    let target_data = if check {
        format!("{work}/check-data")
    } else {
        new_data.clone()
    };
    if check {
        let _ = std::fs::remove_dir_all(&target_data);
    }
    let initialized = Path::new(&target_data).join("PG_VERSION").exists();
    if !initialized {
        let _ = run(Command::new(bin("install")).args([
            "-d",
            "-o",
            OS_USER,
            "-g",
            OS_USER,
            "-m",
            "0700",
            &target_data,
        ]))
        .await;
        let mut init = runuser(&format!("{new_bin}/initdb"));
        init.args([
            "-D",
            &target_data,
            "--username=postgres",
            "--encoding=UTF8",
            "--locale=C.UTF-8",
            "--data-checksums",
            "--auth=scram-sha-256",
        ]);
        init.arg(format!("--pwfile={pwfile}"));
        if wal && !check {
            init.arg(format!("--waldir={}", nixgen::wal_path(cluster, new)));
        }
        run(&mut init).await.map_err(|e| format!("initdb: {e}"))?;
    }
    let mut cmd = runuser(&format!("{new_bin}/pg_upgrade"));
    psql_env(&mut cmd, cluster, port)?;
    cmd.current_dir(&work).args([
        "-b",
        &old_bin,
        "-B",
        &new_bin,
        "-d",
        &old_data,
        "-D",
        &target_data,
        "-U",
        "postgres",
        "--socketdir",
        &work,
    ]);
    if check {
        cmd.arg("--check");
    } else if reflink_supported(cluster).await {
        cmd.arg("--clone");
    } else {
        cmd.arg("--link");
    }
    let out = run(&mut cmd)
        .await
        .map_err(|e| format!("pg_upgrade: {e}"))?;
    if check {
        let _ = std::fs::remove_dir_all(&target_data);
    }
    Ok(out.lines().rev().take(5).collect::<Vec<_>>().join("\n"))
}

/// Store path of a pinned PostgreSQL package, built from the host flake's
/// nixpkgs without touching the running system.
async fn resolve_package(attr: &str) -> Result<String, String> {
    if !spec::PACKAGES.contains(&attr) {
        return Err(format!("package {attr} is not allowed"));
    }
    let expr = format!(
        "(builtins.getFlake \"path:/etc/nixos\").inputs.kcore.inputs.nixpkgs.legacyPackages.${{builtins.currentSystem}}.{attr}"
    );
    let out = run(Command::new(bin("nix")).args([
        "--extra-experimental-features",
        "nix-command flakes",
        "build",
        "--impure",
        "--no-link",
        "--print-out-paths",
        "--expr",
        &expr,
    ]))
    .await?;
    out.lines()
        .last()
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("/nix/store/"))
        .ok_or_else(|| format!("nix build {attr} printed no path"))
}

fn etcd_endpoints() -> Vec<String> {
    let Some(v) = patroni_yaml() else {
        return Vec::new();
    };
    let protocol = v
        .get("etcd3")
        .and_then(|e| e.get("protocol"))
        .and_then(|p| p.as_str())
        .unwrap_or("https")
        .to_string();
    v.get("etcd3")
        .and_then(|e| e.get("hosts"))
        .and_then(|h| h.as_sequence())
        .map(|hosts| {
            hosts
                .iter()
                .filter_map(|h| h.as_str())
                .map(|h| format!("{protocol}://{h}"))
                .collect()
        })
        .unwrap_or_default()
}

fn etcdctl(cluster: &str, endpoints: &[String]) -> Command {
    let mut cmd = Command::new(bin("etcdctl"));
    cmd.arg(format!("--endpoints={}", endpoints.join(",")))
        .arg("--dial-timeout=5s")
        .arg("--command-timeout=15s");
    if tls_on(cluster) {
        let tls = nixgen::tls_dir(cluster);
        cmd.arg(format!("--cacert={tls}/ca.crt"))
            .arg(format!("--cert={tls}/node.crt"))
            .arg(format!("--key={tls}/node.key"));
    }
    cmd
}

fn local_etcd(cluster: &str, port: u16) -> Vec<String> {
    let scheme = if tls_on(cluster) { "https" } else { "http" };
    vec![format!("{scheme}://127.0.0.1:{port}")]
}

/// Point Patroni's `initialize` key at the upgraded (or restored) data
/// directory's system identifier and drop the old timeline's history.
pub async fn set_initialize_key(cluster: &str, spec: &ClusterSpec) -> Result<String, String> {
    let data = data_dir(cluster, spec.major());
    let control = run(runuser(&format!("{PG_CURRENT}/pg_controldata")).arg(&data)).await?;
    let sysid = ops::parse_system_identifier(&control)
        .ok_or("pg_controldata printed no system identifier")?;
    let endpoints = etcd_endpoints();
    if endpoints.is_empty() {
        return Err("no etcd endpoints in patroni.yml".into());
    }
    let prefix = format!("{NAMESPACE}{cluster}");
    run(etcdctl(cluster, &endpoints).args(["put", &format!("{prefix}/initialize"), &sysid]))
        .await?;
    for key in ["history", "status", "optime/leader", "leader"] {
        let _ = run(etcdctl(cluster, &endpoints).args(["del", &format!("{prefix}/{key}")])).await;
    }
    Ok(format!("initialize={sysid}"))
}

pub async fn etcd_member_add(cluster: &str, arg: &str) -> Result<String, String> {
    let (name, url) = arg
        .split_once('=')
        .ok_or("etcd_member_add wants name=peer-url")?;
    if !spec::valid_node_token(name) || !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return Err("invalid etcd member".into());
    }
    let endpoints = etcd_endpoints();
    run(etcdctl(cluster, &endpoints).args(["member", "add", name, &format!("--peer-urls={url}")]))
        .await
}

pub async fn etcd_member_remove(cluster: &str, name: &str) -> Result<String, String> {
    if !spec::valid_node_token(name) {
        return Err("invalid etcd member name".into());
    }
    let endpoints = etcd_endpoints();
    let list = run(etcdctl(cluster, &endpoints).args(["member", "list", "-w", "json"])).await?;
    let v: serde_json::Value =
        serde_json::from_str(&list).map_err(|e| format!("member list: {e}"))?;
    let id = v
        .get("members")
        .and_then(|m| m.as_array())
        .and_then(|members| {
            members
                .iter()
                .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(name))
        })
        .and_then(|m| m.get("ID").and_then(|i| i.as_u64()));
    let Some(id) = id else {
        return Ok(format!("{name} is not an etcd member"));
    };
    run(etcdctl(cluster, &endpoints).args(["member", "remove", &format!("{id:x}")])).await
}

/// Remove `pgdata/<major>` (and `pgwal/<major>`) after finalize or rollback.
/// Refuses the running major.
pub async fn remove_data_major(
    cluster: &str,
    spec: &ClusterSpec,
    major: &str,
) -> Result<String, String> {
    let major: u32 = major.parse().map_err(|_| "major must be a number")?;
    if major == spec.major() {
        return Err(format!("refusing to remove the running major {major}"));
    }
    for path in [
        nixgen::data_path(cluster, major),
        nixgen::wal_path(cluster, major),
    ] {
        if Path::new(&path).exists() {
            std::fs::remove_dir_all(&path).map_err(|e| format!("{path}: {e}"))?;
        }
    }
    let _ = std::fs::remove_dir_all(format!(
        "{}/upgrade-{}",
        nixgen::cluster_root(cluster),
        spec.major()
    ));
    Ok(format!("removed major {major}"))
}

pub async fn verify(cluster: &str, spec: &ClusterSpec) -> Result<String, String> {
    let view = ops::parse_cluster(&patroni(cluster, "GET", "/cluster", None).await?)?;
    let leader = view.leader().ok_or("no leader in /cluster")?;
    for m in spec.postgres_members() {
        if !view.members.iter().any(|v| v.name == m.node_id) {
            return Err(format!("{} is missing from /cluster", m.node_id));
        }
    }
    psql(cluster, spec.ports.postgres, "postgres", "SELECT 1").await?;
    Ok(format!(
        "leader {}, {} members",
        leader.name,
        view.members.len()
    ))
}

async fn device_has_filesystem(device: &str) -> bool {
    run(Command::new(bin("blkid")).args(["-o", "value", "-s", "TYPE", device]))
        .await
        .map(|o| !o.trim().is_empty())
        .unwrap_or(false)
}

/// Create and format this member's declared volumes before the rebuild
/// mounts them. A device that already carries a filesystem is never touched.
pub async fn ensure_volumes(
    cluster: &str,
    spec: &ClusterSpec,
    node_id: &str,
) -> Result<(), String> {
    let Some(member) = spec.member(node_id) else {
        return Ok(());
    };
    for v in nixgen::member_volumes(cluster, member) {
        if !Path::new(&v.device).exists() {
            let size = format!("{}b", v.size_bytes);
            match v.storage_class.as_str() {
                "zfs" => {
                    run(Command::new(bin("zfs")).args([
                        "create",
                        "-V",
                        &v.size_bytes.to_string(),
                        &format!("{}/{}", v.pool, v.name),
                    ]))
                    .await
                    .map_err(|e| format!("zfs create {}: {e}", v.name))?;
                }
                _ => {
                    run(Command::new(bin("lvcreate"))
                        .args(["-y", "-L", &size, "-n", &v.name, &v.pool]))
                    .await
                    .map_err(|e| format!("lvcreate {}: {e}", v.name))?;
                }
            }
            let _ = run(Command::new(bin("udevadm")).args(["settle", "--timeout=30"])).await;
        }
        if !device_has_filesystem(&v.device).await {
            let mkfs = if v.fs_type == "ext4" {
                "mkfs.ext4"
            } else {
                "mkfs.xfs"
            };
            run(Command::new(bin(mkfs)).arg(&v.device))
                .await
                .map_err(|e| format!("{mkfs} {}: {e}", v.device))?;
        }
    }
    Ok(())
}

/// Delete this member's volumes. Refuses while one is still mounted.
pub async fn release_volumes(
    cluster: &str,
    spec: &ClusterSpec,
    node_id: &str,
) -> Result<String, String> {
    let Some(member) = spec.member(node_id) else {
        return Ok("not a member".into());
    };
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut released = Vec::new();
    for v in nixgen::member_volumes(cluster, member) {
        if mounts
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(v.mount_point.as_str()))
        {
            return Err(format!("{} is still mounted", v.mount_point));
        }
        if !Path::new(&v.device).exists() {
            continue;
        }
        match v.storage_class.as_str() {
            "zfs" => {
                run(Command::new(bin("zfs")).args(["destroy", &format!("{}/{}", v.pool, v.name)]))
                    .await?
            }
            _ => {
                run(Command::new(bin("lvremove")).args(["-y", &format!("{}/{}", v.pool, v.name)]))
                    .await?
            }
        };
        released.push(v.name);
    }
    Ok(format!("released {}", released.join(", ")))
}

/// Status of this member. Errors become a message; the caller always gets an
/// answer so the controller can tell "down" from "unreachable".
pub async fn status(cluster: &str) -> proto::GetPostgresqlClusterStatusResponse {
    let mut out = proto::GetPostgresqlClusterStatusResponse {
        patroni_active: unit_active("patroni.service").await,
        etcd_active: unit_active(&nixgen::etcd_unit(cluster)).await,
        ..Default::default()
    };
    let mut notes = Vec::new();
    if out.patroni_active {
        match patroni(cluster, "GET", "/patroni", None)
            .await
            .and_then(|b| ops::parse_patroni(&b))
        {
            Ok(p) => {
                out.role = p.role;
                out.state = p.state;
                out.timeline = p.timeline;
                out.pending_restart = p.pending_restart;
                out.paused = p.paused;
                out.version = ops::format_server_version(p.server_version);
            }
            Err(e) => notes.push(e),
        }
        match patroni(cluster, "GET", "/cluster", None)
            .await
            .and_then(|b| ops::parse_cluster(&b))
        {
            Ok(view) => {
                out.paused |= view.paused;
                out.members = view
                    .members
                    .into_iter()
                    .map(|m| proto::PostgresqlClusterMemberStatus {
                        name: m.name,
                        role: m.role,
                        state: m.state,
                        host: m.host,
                        timeline: m.timeline,
                        lag_bytes: m.lag_bytes,
                        pending_restart: m.pending_restart,
                        nofailover: m.nofailover,
                    })
                    .collect();
            }
            Err(e) => notes.push(e),
        }
    }
    if out.etcd_active {
        let port = etcd_client_port();
        match run(
            etcdctl(cluster, &local_etcd(cluster, port)).args(["endpoint", "status", "-w", "json"])
        )
        .await
        .and_then(|b| ops::parse_etcd_status(&b))
        {
            Ok((leader, _)) => out.etcd_leader = leader,
            Err(e) => notes.push(format!("etcd: {e}")),
        }
    }
    if let Ok(vip) = std::fs::read_to_string(VIP_FILE) {
        let vip = vip.trim().to_string();
        if !vip.is_empty() {
            out.holds_vip = run(Command::new(bin("ip")).args(["-o", "-4", "addr", "show"]))
                .await
                .map(|o| {
                    o.split_whitespace()
                        .any(|w| w.split('/').next() == Some(vip.as_str()))
                })
                .unwrap_or(false);
        }
    }
    out.data_free_bytes =
        run(Command::new(bin("df")).args(["-B1", "--output=avail", &nixgen::data_mount(cluster)]))
            .await
            .ok()
            .and_then(|o| o.lines().nth(1).and_then(|l| l.trim().parse().ok()))
            .unwrap_or(0);
    out.message = notes.join("; ");
    out
}

fn etcd_client_port() -> u16 {
    patroni_yaml()
        .and_then(|v| {
            v.get("etcd3")?
                .get("hosts")?
                .as_sequence()?
                .first()?
                .as_str()?
                .rsplit(':')
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(spec::DEFAULT_PORTS.etcd_client)
}

fn issue(
    blocking: bool,
    check: &str,
    message: impl Into<String>,
) -> proto::PostgresqlPreflightIssue {
    proto::PostgresqlPreflightIssue {
        blocking,
        check: check.into(),
        message: message.into(),
    }
}

/// `ss -ltnH` lines to listening TCP ports.
pub fn listening_ports(ss: &str) -> Vec<u16> {
    ss.lines()
        .filter_map(|line| line.split_whitespace().nth(3))
        .filter_map(|addr| addr.rsplit(':').next()?.parse().ok())
        .collect()
}

/// `MemTotal` in bytes from /proc/meminfo.
pub fn mem_total(meminfo: &str) -> i64 {
    meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse::<i64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// Cluster name of an enabled values file, if any.
pub fn values_cluster(values: &str) -> Option<String> {
    if !values.contains("enable = true;") {
        return None;
    }
    let line = values
        .lines()
        .find(|l| l.trim_start().starts_with("clusterName = "))?;
    Some(line.split('"').nth(1)?.to_string())
}

pub async fn preflight(
    req: &proto::CheckPostgresqlClusterPreflightRequest,
) -> Result<proto::CheckPostgresqlClusterPreflightResponse, String> {
    let spec = parse_spec(&req.cluster_name, &req.spec_json)?;
    let member = spec
        .member(&req.node_id)
        .ok_or_else(|| format!("{} is not a member of the spec", req.node_id))?
        .clone();
    let mut issues = Vec::new();
    let mut resp = proto::CheckPostgresqlClusterPreflightResponse::default();

    if !Path::new("/etc/nixos/flake.nix").exists() {
        issues.push(issue(
            true,
            "flake",
            "PostgreSQL clusters need a flake-based host (/etc/nixos/flake.nix); migrate the node first",
        ));
    }
    let ours = std::fs::read_to_string(VALUES_FILE)
        .ok()
        .and_then(|v| values_cluster(&v));
    if let Some(other) = ours.as_ref().filter(|c| **c != req.cluster_name) {
        issues.push(issue(
            true,
            "conflict",
            format!("node is a member of PostgreSQL cluster {other}"),
        ));
    }
    if unit_active("postgresql.service").await {
        issues.push(issue(
            true,
            "conflict",
            "services.postgresql is running here (a single-node Postgresql resource); remove it first",
        ));
    }

    let iface = Path::new("/sys/class/net").join(&member.iface);
    if !iface.exists() {
        issues.push(issue(
            true,
            "iface",
            format!("interface {} does not exist", member.iface),
        ));
    } else {
        let addrs =
            run(Command::new(bin("ip")).args(["-o", "-4", "addr", "show", "dev", &member.iface]))
                .await
                .unwrap_or_default();
        if !addrs
            .split_whitespace()
            .any(|w| w.split('/').next() == Some(member.advertise_address.as_str()))
        {
            issues.push(issue(
                true,
                "address",
                format!("{} is not on {}", member.advertise_address, member.iface),
            ));
        }
        if addrs.contains("dynamic") {
            issues.push(issue(
                true,
                "address",
                format!("{} is DHCP-managed; use a static address", member.iface),
            ));
        }
    }

    let ss = run(Command::new(bin("ss")).args(["-ltnH"]))
        .await
        .unwrap_or_default();
    let listening = listening_ports(&ss);
    let ours_running = ours.as_deref() == Some(req.cluster_name.as_str());
    for (name, port) in [
        ("postgres", spec.ports.postgres),
        ("patroniApi", spec.ports.patroni_api),
        ("etcdClient", spec.ports.etcd_client),
        ("etcdPeer", spec.ports.etcd_peer),
    ] {
        if listening.contains(&port) && !ours_running {
            issues.push(issue(true, "port", format!("{name} port {port} is in use")));
        }
    }

    let ntp =
        run(Command::new(bin("timedatectl")).args(["show", "-p", "NTPSynchronized", "--value"]))
            .await
            .unwrap_or_default();
    if ntp.trim() != "yes" {
        issues.push(issue(false, "clock", "the clock is not NTP-synchronised"));
    }

    resp.memory_bytes = std::fs::read_to_string("/proc/meminfo")
        .map(|m| mem_total(&m))
        .unwrap_or(0);
    if spec.memory_limit_bytes > 0
        && resp.memory_bytes > 0
        && (spec.memory_limit_bytes as i64) > resp.memory_bytes - (2 << 30)
    {
        issues.push(issue(
            false,
            "memory",
            "memoryLimitBytes leaves less than 2 GiB for the host and its guests",
        ));
    }

    for (label, volume) in [
        ("data", &member.data_volume),
        ("wal", &member.wal_volume),
        ("etcd", &member.etcd_volume),
    ] {
        let Some(v) = volume else { continue };
        if v.storage_class == "lvm" {
            let exists = run(Command::new(bin("lvs"))
                .args(["--noheadings", &format!("{}/{}", v.volume_group, v.name)]))
            .await
            .is_ok();
            if !exists {
                let free = run(Command::new(bin("vgs")).args([
                    "--noheadings",
                    "--units",
                    "b",
                    "--nosuffix",
                    "-o",
                    "vg_free",
                    &v.volume_group,
                ]))
                .await;
                match free.ok().and_then(|f| f.trim().parse::<i64>().ok()) {
                    Some(free) if free >= v.size_bytes => {}
                    Some(free) => issues.push(issue(
                        true,
                        "volume",
                        format!(
                            "{label} volume: {} has {free} bytes free, {} needed",
                            v.volume_group, v.size_bytes
                        ),
                    )),
                    None => issues.push(issue(
                        true,
                        "volume",
                        format!("volume group {} not found", v.volume_group),
                    )),
                }
            }
        }
    }

    if unit_active("patroni.service").await && ours_running {
        if let Ok(p) = patroni(&req.cluster_name, "GET", "/patroni", None)
            .await
            .and_then(|b| ops::parse_patroni(&b))
        {
            resp.running_version = ops::format_server_version(p.server_version);
        }
        if !req.changed_parameters.is_empty() {
            let keys: Vec<String> = req
                .changed_parameters
                .iter()
                .filter(|k| spec::valid_parameter_key(k))
                .cloned()
                .collect();
            if let Ok(rows) = psql(
                &req.cluster_name,
                spec.ports.postgres,
                "postgres",
                &ops::settings_context_sql(&keys),
            )
            .await
            {
                resp.restart_parameters = ops::parse_restart_keys(&rows);
            }
        }
    }

    if !req.upgrade_package.is_empty() && !member.witness {
        resp.reflink_supported = reflink_supported(&req.cluster_name).await;
        if !resp.reflink_supported {
            issues.push(issue(
                false,
                "reflink",
                "data filesystem has no reflink: pg_upgrade uses --link and the upgrade cannot be rolled back",
            ));
        }
    }

    resp.ok = !issues.iter().any(|i| i.blocking);
    resp.issues = issues;
    Ok(resp)
}

/// Execute one admin command. Returns the message for the response.
pub async fn admin(req: &proto::RunPostgresqlAdminRequest) -> Result<String, String> {
    use proto::run_postgresql_admin_request::Command as C;
    let cluster = req.cluster_name.as_str();
    if !spec::valid_identifier(cluster) {
        return Err("cluster_name is not a valid identifier".into());
    }
    let spec = || parse_spec(cluster, &req.spec_json);
    let command = req.command.as_ref().ok_or("command is required")?;
    match command {
        C::SwitchoverCandidate(candidate) => {
            if !candidate.is_empty() && !spec::valid_node_token(candidate) {
                return Err("invalid candidate".into());
            }
            let me = ops::parse_cluster(&patroni(cluster, "GET", "/cluster", None).await?)?;
            let leader = me.leader().ok_or("no leader")?.name.clone();
            patroni(
                cluster,
                "POST",
                "/switchover",
                Some(&ops::switchover_body(&leader, candidate, "")),
            )
            .await
        }
        C::FailoverCandidate(candidate) => {
            if !spec::valid_node_token(candidate) {
                return Err("invalid candidate".into());
            }
            patroni(
                cluster,
                "POST",
                "/failover",
                Some(&ops::failover_body(candidate)),
            )
            .await
        }
        C::RestartMember(_) => systemctl("restart", "patroni.service").await,
        C::Reinitialize(_) => {
            patroni(
                cluster,
                "POST",
                "/reinitialize",
                Some(&serde_json::json!({ "force": true })),
            )
            .await
        }
        C::Pause(paused) => {
            patroni(cluster, "PATCH", "/config", Some(&ops::pause_body(*paused))).await
        }
        C::PatchDynamicConfig(_) => {
            let spec = spec()?;
            let current: serde_json::Value =
                serde_json::from_str(&patroni(cluster, "GET", "/config", None).await?)
                    .unwrap_or_default();
            let mut body = ops::dynamic_config(&spec);
            // Parameters set in etcd but no longer wanted go as null.
            if let (Some(live), Some(want)) = (
                current
                    .pointer("/postgresql/parameters")
                    .and_then(|p| p.as_object()),
                body.pointer_mut("/postgresql/parameters")
                    .and_then(|p| p.as_object_mut()),
            ) {
                for key in live.keys() {
                    if !want.contains_key(key) {
                        want.insert(key.clone(), serde_json::Value::Null);
                    }
                }
            }
            patroni(cluster, "PATCH", "/config", Some(&body))
                .await
                .map(|_| "dynamic config patched".into())
        }
        C::Reload(_) => patroni(cluster, "POST", "/reload", None).await,
        C::Unit(u) => systemctl(&u.action, &unit_name(cluster, &u.unit)?).await,
        C::StopPostgres(_) => stop_postgres(&spec()?, cluster).await,
        C::EnsureRoles(e) => {
            let generated: BTreeMap<String, String> = e
                .generated_passwords
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            ensure_roles(cluster, &spec()?, &generated, &e.drop_roles).await
        }
        C::EnsureDatabases(e) => ensure_databases(cluster, &spec()?, &e.drop_databases).await,
        C::EnsureExtensions(e) => ensure_extensions(cluster, &spec()?, e.update_extensions).await,
        C::Analyze(_) => analyze(cluster, spec()?.ports.postgres).await,
        C::PgUpgradeCheck(u) => {
            pg_upgrade(
                cluster,
                &u.from_package,
                &u.to_package,
                true,
                spec()?.ports.postgres,
            )
            .await
        }
        C::PgUpgradeRun(u) => {
            pg_upgrade(
                cluster,
                &u.from_package,
                &u.to_package,
                false,
                spec()?.ports.postgres,
            )
            .await
        }
        C::SetInitializeKey(_) => set_initialize_key(cluster, &spec()?).await,
        C::RemoveDataMajor(major) => remove_data_major(cluster, &spec()?, major).await,
        C::EtcdMemberAdd(arg) => etcd_member_add(cluster, arg).await,
        C::EtcdMemberRemove(name) => etcd_member_remove(cluster, name).await,
        C::Verify(_) => verify(cluster, &spec()?).await,
        C::ReleaseVolumes(_) => release_volumes(cluster, &spec()?, &req.node_id).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"{"package":"postgresql_17","members":[
      {"nodeId":"node-105","advertiseAddress":"10.0.0.5","iface":"eth0","role":"primary"},
      {"nodeId":"node-106","advertiseAddress":"10.0.0.6","iface":"eth0"},
      {"nodeId":"node-108","advertiseAddress":"10.0.0.8","iface":"eth0"}]}"#;

    fn apply(stage: &str) -> proto::ApplyPostgresqlClusterConfigRequest {
        proto::ApplyPostgresqlClusterConfigRequest {
            cluster_name: "orders".into(),
            generation: 2,
            spec_json: SPEC.into(),
            node_id: "node-106".into(),
            etcd_voters: vec!["node-105".into(), "node-106".into(), "node-108".into()],
            stage: stage.into(),
            ..Default::default()
        }
    }

    #[test]
    fn renders_each_stage() {
        let full = render_values(&apply("full")).unwrap();
        assert!(full.contains("advertiseAddress = \"10.0.0.6\";"));
        assert!(full.contains("initialClusterState = \"new\";"));
        let join = render_values(&apply("join")).unwrap();
        assert!(join.contains("initialClusterState = \"existing\";"));
        let etcd = render_values(&apply("etcd-only")).unwrap();
        assert!(etcd.contains("postgres = {\n      enable = false;"));
        assert_eq!(
            render_values(&apply("disable")).unwrap(),
            nixgen::render_disabled()
        );
        assert!(render_values(&apply("upgrade")).is_err());
        let mut up = apply("upgrade");
        up.previous_package = "postgresql_16".into();
        assert!(render_values(&up)
            .unwrap()
            .contains("previousPackage = \"postgresql_16\";"));
        assert!(render_values(&apply("bogus")).is_err());
    }

    #[test]
    fn agent_revalidates_the_spec() {
        let mut req = apply("full");
        req.spec_json = SPEC.replace("eth0", "br0");
        assert!(render_values(&req).unwrap_err().contains("VM bridge"));
        req.spec_json = SPEC.into();
        req.node_id = "node-200".into();
        assert!(render_values(&req).is_err());
        req.node_id = "node-106".into();
        req.cluster_name = "Orders;".into();
        assert!(render_values(&req).is_err());
    }

    #[test]
    fn scram_verifier_matches_the_postgres_algorithm() {
        // Expected value from an independent implementation (Python
        // hashlib.pbkdf2_hmac + hmac, RFC 5802 / RFC 7677 key derivation).
        let v = scram_verifier_with_salt("pencil", b"0123456789abcdef");
        assert_eq!(
            v,
            "SCRAM-SHA-256$4096:MDEyMzQ1Njc4OWFiY2RlZg==$nQpbZ77WudtqufPwikHXGRt6g2QJ4zns8bZLw273DRM=:jn2amWP1q1h+jgjy0YTO14S6/F02SV7taipOeB7ef20="
        );
        assert!(spec::valid_scram_verifier(&v));
        assert_ne!(scram_verifier("pencil"), scram_verifier("pencil"));
    }

    #[test]
    fn parses_host_probes() {
        let ss = "LISTEN 0 4096 0.0.0.0:8008 0.0.0.0:*\nLISTEN 0 244 127.0.0.1:5432 0.0.0.0:*\nLISTEN 0 128 [::]:22 [::]:*\n";
        assert_eq!(listening_ports(ss), vec![8008, 5432, 22]);
        assert_eq!(
            mem_total("MemTotal:       65536 kB\nMemFree: 1 kB\n"),
            65536 * 1024
        );
        assert_eq!(
            values_cluster("{ ... }:\n{\n  kcore.postgresqlCluster = {\n    enable = true;\n    clusterName = \"orders\";\n"),
            Some("orders".into())
        );
        assert_eq!(values_cluster(&nixgen::render_disabled()), None);
    }

    #[test]
    fn unit_and_upgrade_arguments_are_closed_sets() {
        assert_eq!(
            unit_name("orders", "etcd").unwrap(),
            "kcore-pg-etcd-orders.service"
        );
        assert!(unit_name("orders", "sshd").is_err());
        assert!(upgrade_pair("postgresql_17", "postgresql_16").is_err());
        assert_eq!(
            upgrade_pair("postgresql_16", "postgresql_18").unwrap(),
            (16, 18)
        );
    }
}
