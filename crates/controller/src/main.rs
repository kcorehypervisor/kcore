#![allow(
    dead_code,
    clippy::await_holding_lock,
    clippy::result_large_err,
    clippy::enum_variant_names,
    clippy::too_many_arguments
)]

mod auth;
mod ceph_cli;
mod ceph_cluster_reconciler;
mod ceph_cluster_spec;
mod cert_rotation_reconciler;
mod cluster_health;
mod cluster_update_reconciler;
mod cluster_update_spec;
mod config;
mod crypto_profile;
mod db;
mod disk_reconciler;
mod grpc;
mod guest_ops;
mod net_policy;
mod nixgen;
mod node_client;
mod object_store_reconciler;
mod object_store_spec;
mod path_safety;
mod pci;
mod pki;
mod rate_limit;
mod replication;
mod replication_policy;
mod sbom;
mod scheduler;
mod shared_filesystem_reconciler;
mod shared_filesystem_spec;
mod snapshot_policy_reconciler;
mod vm_nics;
mod vm_operation_policy;
mod vm_operation_reconciler;
mod volume_crypto;
mod volume_snapshot;
mod webhooks;

use std::sync::{Arc, Mutex};

use clap::Parser;
use tokio::signal;
use tonic::service::interceptor::InterceptedService;
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

pub mod controller_proto {
    #![allow(clippy::result_large_err)]
    tonic::include_proto!("kcore.controller");
}

pub mod node_proto {
    #![allow(clippy::result_large_err)]
    tonic::include_proto!("kcore.node");
}

#[derive(Parser)]
#[command(name = "kcore-controller", about = "kcore controller")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/kcore/controller.yaml")]
    config: String,

    /// Allow running without TLS (INSECURE: all RPCs are unauthenticated)
    #[arg(long)]
    allow_insecure: bool,
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
    let cfg = config::Config::load(&cli.config)?;
    let addr: std::net::SocketAddr = cfg.listen_addr.parse()?;

    if cfg.tls.is_none() && !cli.allow_insecure {
        anyhow::bail!(
            "TLS is not configured. All gRPC traffic would be unauthenticated and unencrypted.\n\
             Configure a [tls] section in the config file, or pass --allow-insecure to override."
        );
    }

    let database = db::Database::open(&cfg.db_path)?;
    let clients =
        node_client::NodeClients::new(cfg.tls.as_ref().map(|tls| node_client::TlsClientConfig {
            ca_file: tls.ca_file.clone(),
            cert_file: tls.cert_file.clone(),
            key_file: tls.key_file.clone(),
        }));

    let sub_ca_state = load_sub_ca(&cfg);
    let sub_ca = Arc::new(Mutex::new(sub_ca_state));

    replication::emit_controller_register(&database, &cfg);

    replication::spawn_replication_pollers(
        database.clone(),
        cfg.replication.clone(),
        cfg.tls.clone(),
        &cfg.listen_addr,
    );
    replication::spawn_compensation_executor(database.clone());
    replication::spawn_head_materializer(database.clone());
    replication::spawn_reservation_retry_executor(database.clone());
    disk_reconciler::spawn_disk_layout_reconciler(
        database.clone(),
        clients.clone(),
        disk_reconciler::DiskLayoutReconcilerConfig {
            default_network: cfg.default_network.clone(),
            sub_ca: sub_ca.clone(),
        },
    );
    ceph_cluster_reconciler::spawn_ceph_cluster_reconciler(database.clone(), clients.clone());
    shared_filesystem_reconciler::spawn_shared_filesystem_reconciler(database.clone());
    object_store_reconciler::spawn_object_store_reconciler(database.clone());
    cluster_update_reconciler::spawn_cluster_update_reconciler(database.clone(), clients.clone());
    snapshot_policy_reconciler::spawn_snapshot_policy_reconciler(database.clone(), clients.clone());
    vm_operation_reconciler::spawn_vm_operation_reconciler(database.clone(), clients.clone());
    {
        let db = database.clone();
        let clients = clients.clone();
        tokio::spawn(async move {
            // Refresh GuestOps pubkeys periodically so cloud-init can inject them.
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                ticker.tick().await;
                match guest_ops::sync_guest_ops_keys(&db, &clients).await {
                    Ok(n) if n > 0 => tracing::info!(synced = n, "guest-ops keys refreshed"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "guest-ops key sync failed"),
                }
            }
        });
    }

    let crl_cache = pki::crl::CrlCache::new();
    crl_cache.load_from_db(&database);
    let revocation = if cfg.revocation.enabled {
        let fail_mode = pki::revocation::FailMode::from_config_str(&cfg.revocation.fail_mode)
            .unwrap_or_default();
        info!(
            fail_mode = fail_mode.as_str(),
            max_staleness_secs = cfg.revocation.max_staleness_secs,
            "peer certificate revocation checking enabled"
        );
        pki::revocation::RevocationState::new(
            fail_mode,
            time::Duration::seconds(cfg.revocation.max_staleness_secs as i64),
        )
    } else {
        warn!("peer certificate revocation checking is DISABLED (revocation.enabled: false)");
        pki::revocation::RevocationState::disabled()
    };

    // Load the revoked set before the listener opens. The reconciler refreshes
    // it on every tick, but under `hard-fail` a listener that starts before the
    // first refresh would reject every peer for the length of that race.
    if let Err(error) = revocation.refresh(&database) {
        warn!(%error, "initial revoked-serial load failed");
    }

    let webhooks =
        webhooks::Dispatcher::spawn(&cfg.webhooks, webhooks::EventSource::from_config(&cfg));

    cert_rotation_reconciler::spawn_cert_rotation_reconciler(
        cert_rotation_reconciler::CertRotationContext {
            db: database.clone(),
            clients: clients.clone(),
            sub_ca: sub_ca.clone(),
            crl_cache: crl_cache.clone(),
            revocation: revocation.clone(),
            rotation: cfg.cert_rotation.clone(),
            pki: cfg.pki.clone(),
            webhooks: webhooks.clone(),
        },
    );

    if cfg.pki.http_enabled {
        match cfg.pki.http_listen_addr.parse::<std::net::SocketAddr>() {
            Ok(pki_addr) => {
                let state = pki::http::PkiHttpState {
                    db: database.clone(),
                    sub_ca: sub_ca.clone(),
                    crl_cache: crl_cache.clone(),
                    ocsp_validity: time::Duration::hours(cfg.pki.ocsp_validity_hours),
                };
                tokio::spawn(pki::http::serve(pki_addr, state));
            }
            Err(error) => warn!(
                addr = %cfg.pki.http_listen_addr,
                %error,
                "invalid pki.httpListenAddr; CRL/OCSP HTTP endpoints disabled"
            ),
        }
    } else {
        info!(
            "PKI HTTP endpoints disabled (pki.httpEnabled: false); nodes must use the GetCrl RPC"
        );
    }

    let staleness_db = database.clone();
    let staleness_webhooks = webhooks.clone();
    tokio::spawn(async move {
        const HEARTBEAT_TIMEOUT_SECS: i64 = 90;
        const CHECK_INTERVAL_SECS: u64 = 30;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(CHECK_INTERVAL_SECS)).await;
            match staleness_db.get_stale_nodes(HEARTBEAT_TIMEOUT_SECS) {
                Ok(stale) => {
                    for node in &stale {
                        if staleness_db
                            .update_node_status(&node.id, "not-ready")
                            .unwrap_or(false)
                        {
                            warn!(
                                node_id = %node.id,
                                last_heartbeat = %node.last_heartbeat,
                                "node missed heartbeat deadline, marked not-ready"
                            );
                            staleness_webhooks.emit(
                                webhooks::EVENT_NODE_HEARTBEAT_MISSED,
                                format!("node/{}", node.id),
                                serde_json::json!({
                                    "nodeId": node.id,
                                    "hostname": node.hostname,
                                    "address": node.address,
                                    "lastHeartbeat": node.last_heartbeat,
                                    "timeoutSeconds": HEARTBEAT_TIMEOUT_SECS,
                                }),
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "failed to check for stale nodes");
                }
            }
        }
    });

    let mut failover_task: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        if let Some(task) = failover_task.take() {
            task.abort();
        }
        let bootstrap_kctl = cfg.auth.as_ref().map(|a| a.bootstrap_kctl).unwrap_or(false);
        let mut svc = grpc::ControllerService::new(
            database.clone(),
            clients.clone(),
            cfg.default_network.clone(),
            sub_ca.clone(),
            cfg.replication.clone(),
            cfg.require_manual_approval,
            bootstrap_kctl,
        )
        .with_pki(grpc::PkiRuntime {
            crl_cache: crl_cache.clone(),
            revocation: revocation.clone(),
            rotation: cfg.cert_rotation.clone(),
            pki: cfg.pki.clone(),
        });
        if let Some(tls) = cfg.tls.as_ref() {
            svc = svc.with_tls_paths(grpc::TlsPaths {
                cert_file: tls.cert_file.clone(),
                key_file: tls.key_file.clone(),
            });
        }
        svc = svc.with_security(
            cfg.rate_limit.clone(),
            cfg.sbom.clone(),
            cfg.revocation.fail_mode.clone(),
        );
        svc = svc.with_webhooks(webhooks.clone());
        svc = svc.with_scheduler(&cfg.scheduler);
        if cfg.failover.enabled {
            let failover_svc = svc.clone();
            failover_task = Some(tokio::spawn(async move {
                let mut announced = std::collections::HashSet::new();
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    failover_svc.failover_not_ready_nodes(&mut announced).await;
                }
            }));
        }
        // Revocation and the rate limit are interceptors so every RPC on both
        // services is covered from one wiring point. The rate limit runs
        // first. tonic builds its rustls ServerConfig internally and takes no
        // custom ClientCertVerifier, so the interceptor is the earliest place
        // we can reject a revoked peer. The health service is not wrapped.
        let limiter = rate_limit::RateLimiter::from_config(&cfg.rate_limit);
        let controller_server = controller_proto::controller_server::ControllerServer::new(svc)
            .max_decoding_message_size(40 * 1024 * 1024)
            .max_encoding_message_size(40 * 1024 * 1024);
        let controller_svc = InterceptedService::new(
            InterceptedService::new(
                controller_server,
                pki::revocation::interceptor(revocation.clone()),
            ),
            limiter.interceptor(),
        );

        let admin_svc = InterceptedService::new(
            controller_proto::controller_admin_server::ControllerAdminServer::with_interceptor(
                grpc::ControllerAdminService::new(
                    database.clone(),
                    cfg.replication.clone(),
                    addr.port(),
                    bootstrap_kctl,
                    cfg.tls.is_some(),
                ),
                pki::revocation::interceptor(revocation.clone()),
            ),
            limiter.interceptor(),
        );

        let (mut health_reporter, health_svc) = tonic_health::server::health_reporter();
        health_reporter
            .set_serving::<controller_proto::controller_server::ControllerServer<grpc::ControllerService>>()
            .await;

        let mut server = Server::builder();
        if let Some(tls) = cfg.tls.as_ref() {
            let cert_pem = std::fs::read_to_string(&tls.cert_file)?;
            let key_pem = std::fs::read_to_string(&tls.key_file)?;
            let ca_pem = std::fs::read_to_string(&tls.ca_file)?;
            let server_tls = ServerTlsConfig::new()
                .identity(Identity::from_pem(cert_pem, key_pem))
                .client_ca_root(Certificate::from_pem(ca_pem));
            server = server.tls_config(server_tls)?;
            info!(addr = %addr, "starting controller with mTLS");
        } else {
            warn!(addr = %addr, "starting controller WITHOUT TLS (--allow-insecure) — all RPCs are unauthenticated");
        }

        let action = shutdown_or_reload_signal();
        let (action_tx, action_rx) = tokio::sync::oneshot::channel::<ShutdownAction>();

        tokio::spawn(async move {
            let result = action.await;
            let _ = action_tx.send(result);
        });

        server
            .add_service(health_svc)
            .add_service(controller_svc)
            .add_service(admin_svc)
            .serve_with_shutdown(addr, async {
                let _ = action_rx.await;
            })
            .await?;

        if matches!(LAST_ACTION.lock().unwrap().as_deref(), Some("shutdown")) {
            break;
        }

        info!("reloading TLS certificates and restarting listener");
    }

    Ok(())
}

static LAST_ACTION: Mutex<Option<String>> = Mutex::new(None);

enum ShutdownAction {
    Shutdown,
    Reload,
}

async fn shutdown_or_reload_signal() -> ShutdownAction {
    let ctrl_c = signal::ctrl_c();
    #[cfg(unix)]
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");
    #[cfg(unix)]
    let mut sighup = signal::unix::signal(signal::unix::SignalKind::hangup())
        .expect("failed to register SIGHUP handler");

    #[cfg(unix)]
    {
        tokio::select! {
            _ = ctrl_c => {
                info!("received Ctrl+C, shutting down");
                *LAST_ACTION.lock().unwrap() = Some("shutdown".into());
                ShutdownAction::Shutdown
            },
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                *LAST_ACTION.lock().unwrap() = Some("shutdown".into());
                ShutdownAction::Shutdown
            },
            _ = sighup.recv() => {
                info!("received SIGHUP, reloading TLS certificates");
                *LAST_ACTION.lock().unwrap() = Some("reload".into());
                ShutdownAction::Reload
            },
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
        info!("received Ctrl+C, shutting down");
        *LAST_ACTION.lock().unwrap() = Some("shutdown".into());
        ShutdownAction::Shutdown
    }
}

fn load_sub_ca(cfg: &config::Config) -> grpc::SubCaState {
    let tls = match cfg.tls.as_ref() {
        Some(t) => t,
        None => return grpc::SubCaState::default(),
    };

    let cert_file = match &tls.sub_ca_cert_file {
        Some(f) if !f.is_empty() => f.clone(),
        _ => return grpc::SubCaState::default(),
    };
    let key_file = match &tls.sub_ca_key_file {
        Some(f) if !f.is_empty() => f.clone(),
        _ => return grpc::SubCaState::default(),
    };

    let cert_pem = match std::fs::read_to_string(&cert_file) {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %cert_file, error = %e, "sub-CA cert file not found; cert renewal disabled");
            return grpc::SubCaState {
                cert_pem: String::new(),
                key_pem: String::new(),
                cert_file: Some(cert_file),
                key_file: Some(key_file),
            };
        }
    };
    let key_pem = match std::fs::read_to_string(&key_file) {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %key_file, error = %e, "sub-CA key file not found; cert renewal disabled");
            return grpc::SubCaState {
                cert_pem: String::new(),
                key_pem: String::new(),
                cert_file: Some(cert_file),
                key_file: Some(key_file),
            };
        }
    };

    info!("sub-CA loaded for automatic certificate renewal");
    grpc::SubCaState {
        cert_pem,
        key_pem,
        cert_file: Some(cert_file),
        key_file: Some(key_file),
    }
}
