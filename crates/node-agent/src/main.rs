// TODO(tech-debt): Crate-wide `dead_code` suppression is temporary. Audit unused
// items and either remove them or narrow to item-level `#[allow(dead_code)]`.
#![allow(dead_code)]
// tonic::Status Err variants in generated gRPC stubs exceed clippy's size lint.
#![allow(clippy::result_large_err)]

mod auth;
mod ceph_bootstrap;
mod config;
mod console;
mod discovery;
mod disk;
mod grpc;
mod issue_screen;
mod live_migrate;
mod path_safety;
mod pki;
mod registration;
mod runtime;
mod storage;
mod vmm;

use clap::{Args, Parser, Subcommand};
use tokio::signal;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tracing::{info, warn};

fn install_fips_crypto_provider() {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();

    provider.cipher_suites.retain(|suite| {
        matches!(
            suite.suite(),
            rustls::CipherSuite::TLS13_AES_256_GCM_SHA384
                | rustls::CipherSuite::TLS13_AES_128_GCM_SHA256
                | rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
                | rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
                | rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
                | rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
        )
    });

    provider.kx_groups.retain(|group| {
        matches!(
            group.name(),
            rustls::NamedGroup::secp256r1 | rustls::NamedGroup::secp384r1
        )
    });

    provider
        .install_default()
        .expect("failed to install FIPS crypto provider");
}

pub mod proto {
    #![allow(clippy::result_large_err)]
    tonic::include_proto!("kcore.node");
}

pub mod controller_proto {
    #![allow(clippy::result_large_err)]
    tonic::include_proto!("kcore.controller");
}

#[derive(Parser)]
#[command(name = "kcore-node-agent", about = "kcore node agent")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/kcore/node-agent.yaml")]
    config: String,

    /// Allow running without TLS (INSECURE: all RPCs are unauthenticated)
    #[arg(long)]
    allow_insecure: bool,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Render an ESX-like pre-login issue screen.
    RenderIssue(RenderIssueArgs),
}

#[derive(Args)]
struct RenderIssueArgs {
    /// Destination file for rendered output ("-" for stdout).
    #[arg(long, default_value = "/etc/issue")]
    output: String,

    /// Disable ANSI colors in output.
    #[arg(long)]
    no_color: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    install_fips_crypto_provider();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    if let Some(CliCommand::RenderIssue(args)) = cli.command {
        let use_color = !args.no_color && issue_screen::should_use_color();
        let rendered = issue_screen::render_issue(use_color);
        if args.output == "-" {
            print!("{rendered}");
        } else {
            std::fs::write(&args.output, rendered)
                .map_err(|e| anyhow::anyhow!("writing {}: {e}", args.output))?;
        }
        return Ok(());
    }

    let cfg = config::Config::load(&cli.config)?;

    if cfg.tls.is_none() && !cli.allow_insecure {
        anyhow::bail!(
            "TLS is not configured. All gRPC traffic would be unauthenticated and unencrypted.\n\
             Configure a [tls] section in the config file, or pass --allow-insecure to override."
        );
    }

    let addr = cfg.listen_addr.parse()?;
    let vm_client = vmm::Client::new(&cfg.vm_socket_dir);
    let storage = storage::from_config(&cfg.storage).map_err(anyhow::Error::new)?;

    // Reload handle and revocation set are created once and shared: rebuilding
    // the listener must not lose the CRL we already fetched.
    let reload = pki::reload::ReloadHandle::new();
    let revocation = pki::revocation::RevocationState::from_config(&cfg.revocation);

    if !cfg.controller_endpoints().is_empty() {
        let reg_cfg = cfg.clone();
        let registered = registration::register_with_controller_tracked(reg_cfg);
        registration::start_heartbeat_loop(cfg.clone(), registered.clone());
        pki::rotate::spawn_rotation_loop(cfg.clone(), reload.clone());
        pki::revocation::spawn_crl_refresh_loop(cfg.clone(), revocation.clone());
    }

    loop {
        let compute_svc = proto::node_compute_server::NodeComputeServer::with_interceptor(
            grpc::ComputeService::new(vm_client.clone()),
            pki::revocation::interceptor(revocation.clone()),
        );
        let info_svc = proto::node_info_server::NodeInfoServer::with_interceptor(
            grpc::InfoService::new(cfg.node_id.clone()),
            pki::revocation::interceptor(revocation.clone()),
        );
        let container_svc = proto::node_container_server::NodeContainerServer::with_interceptor(
            grpc::ContainerService::new(),
            pki::revocation::interceptor(revocation.clone()),
        );
        // Message-size overrides live on the generated server, so it is built
        // first and wrapped in the interceptor afterwards.
        let admin_svc = tonic::service::interceptor::InterceptedService::new(
            proto::node_admin_server::NodeAdminServer::new(
                grpc::AdminService::new_with_storage(
                    cfg.nix_config_path.clone(),
                    cfg.vm_socket_dir.clone(),
                    storage.clone(),
                    live_migrate::LiveMigrateState::new(),
                )
                .with_pki(cfg.clone(), reload.clone()),
            )
            .max_decoding_message_size(1024 * 1024 * 1024)
            .max_encoding_message_size(64 * 1024 * 1024),
            pki::revocation::interceptor(revocation.clone()),
        );
        let storage_svc = proto::node_storage_server::NodeStorageServer::with_interceptor(
            grpc::StorageService::new_with_storage(storage.clone()),
            pki::revocation::interceptor(revocation.clone()),
        );

        let (mut health_reporter, health_svc) = tonic_health::server::health_reporter();
        health_reporter
            .set_serving::<proto::node_compute_server::NodeComputeServer<grpc::ComputeService>>()
            .await;
        health_reporter
            .set_serving::<proto::node_container_server::NodeContainerServer<grpc::ContainerService>>()
            .await;

        let mut server = Server::builder();
        if let Some(tls) = cfg.tls.as_ref() {
            // Read from disk on every iteration: that is what makes a reload
            // pick up a rotated certificate.
            let cert_pem = std::fs::read_to_string(&tls.cert_file)?;
            let key_pem = std::fs::read_to_string(&tls.key_file)?;
            let ca_pem = std::fs::read_to_string(&tls.ca_file)?;
            let server_tls = ServerTlsConfig::new()
                .identity(Identity::from_pem(cert_pem, key_pem))
                .client_ca_root(Certificate::from_pem(ca_pem));
            server = server.tls_config(server_tls)?;
            info!(addr = %addr, node_id = %cfg.node_id, "starting node-agent with mTLS");
        } else {
            warn!(addr = %addr, node_id = %cfg.node_id, "starting node-agent WITHOUT TLS (--allow-insecure) — all RPCs are unauthenticated");
        }

        let exit = serve_until(
            server
                .add_service(health_svc)
                .add_service(compute_svc)
                .add_service(container_svc)
                .add_service(info_svc)
                .add_service(admin_svc)
                .add_service(storage_svc),
            addr,
            reload.clone(),
        )
        .await?;

        if exit == pki::reload::ServeExit::Shutdown {
            break;
        }
        info!("reloading TLS material and restarting listener");
    }

    Ok(())
}

/// Serve until either a shutdown signal or a TLS reload request arrives.
///
/// `serve_with_shutdown` drains in-flight requests before returning, so a
/// reload is graceful: the listener closes, the loop rebuilds it from the new
/// files, and callers reconnect. The process, and everything it holds in
/// memory, survives.
async fn serve_until(
    router: tonic::transport::server::Router,
    addr: std::net::SocketAddr,
    reload: pki::reload::ReloadHandle,
) -> anyhow::Result<pki::reload::ServeExit> {
    let (tx, rx) = tokio::sync::oneshot::channel::<pki::reload::ServeExit>();
    let waiter = reload.clone();
    tokio::spawn(async move {
        let exit = tokio::select! {
            () = shutdown_signal() => pki::reload::ServeExit::Shutdown,
            () = waiter.wait() => {
                info!("certificate rotation requested a TLS reload");
                pki::reload::ServeExit::Reload
            }
        };
        let _ = tx.send(exit);
    });

    let reload_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = reload_requested.clone();
    router
        .serve_with_shutdown(addr, async move {
            if let Ok(pki::reload::ServeExit::Reload) = rx.await {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .await?;
    Ok(
        if reload_requested.load(std::sync::atomic::Ordering::SeqCst) {
            pki::reload::ServeExit::Reload
        } else {
            pki::reload::ServeExit::Shutdown
        },
    )
}

async fn shutdown_signal() {
    let ctrl_c = signal::ctrl_c();
    #[cfg(unix)]
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");
    #[cfg(unix)]
    let terminate = sigterm.recv();
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { info!("received Ctrl+C, shutting down"); },
        _ = terminate => { info!("received SIGTERM, shutting down"); },
    }
}
