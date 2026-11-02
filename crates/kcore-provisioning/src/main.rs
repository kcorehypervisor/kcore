//! Operator CLI for the provisioner. The long-running command is `serve`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kcore_provisioning::apply::{self, Layout};
use kcore_provisioning::grpc::{self, Engine, Service};
use kcore_provisioning::routeros;
use kcore_provisioning::store::{Seed, Store};
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "kcore-provisioning", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create secrets and render config. An empty database takes the seed.
    Prepare {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        seed: PathBuf,
    },
    /// Serve the gRPC API and keep PowerDNS in sync.
    Serve {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long, required = true)]
        listen: Vec<SocketAddr>,
    },
    /// Import a RouterOS export, REST dump, or API snapshot.
    Import {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "lan")]
        network: String,
        #[arg(long)]
        apply: bool,
    },
    /// Copy DHCP servers, pools, networks, and leases from a RouterOS API.
    PullRouteros {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        host: String,
        #[arg(long, default_value = "admin")]
        user: String,
        #[arg(long)]
        password_file: PathBuf,
        #[arg(long, default_value = "lan")]
        network: String,
    },
    /// Tell a running Kea to reread its configuration.
    ReloadKea,
    /// Disable or enable every DHCP server on a RouterOS router.
    SetRouterosDhcp {
        #[arg(long)]
        host: String,
        #[arg(long, default_value = "admin")]
        user: String,
        #[arg(long)]
        password_file: PathBuf,
        /// Pass `--disabled false` to turn the servers back on.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        disabled: bool,
    },
    /// Print the catalog as JSON.
    Status {
        #[arg(long)]
        state_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Prepare { state_dir, seed } => {
            let layout = Layout::new(state_dir);
            let mut store = Store::open(&layout.database())?;
            let seed: Seed = serde_json::from_str(&fs::read_to_string(&seed)?)
                .with_context(|| format!("parsing seed {}", seed.display()))?;
            let inserted = store.apply_seed(&seed)?;
            let catalog = store.load()?;
            let rendered = apply::render(&layout, &catalog)?;
            println!(
                "prepared {} networks, seed_inserted={inserted}, kea={}",
                catalog.networks.len(),
                rendered.kea_path.display()
            );
        }
        Command::Serve { state_dir, listen } => {
            let layout = Layout::new(state_dir);
            let mut engine = Engine::open(layout, true, true)?;
            for attempt in 1..=30 {
                match engine.apply() {
                    Ok(result) if !result.message.contains("powerdns sync failed") => {
                        tracing::info!("{}", result.message);
                        break;
                    }
                    Ok(result) if attempt == 30 => {
                        tracing::warn!(
                            "starting api before dns sync succeeded: {}",
                            result.message
                        );
                    }
                    Ok(result) => {
                        tracing::warn!(attempt, "{}", result.message);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    Err(err) if attempt == 30 => {
                        tracing::warn!("starting api with an unapplied catalog: {err}");
                    }
                    Err(err) => {
                        tracing::warn!(attempt, "apply failed: {err}");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
            grpc::serve(Service::new(engine), listen).await?;
        }
        Command::Import {
            state_dir,
            file,
            network,
            apply,
        } => {
            let layout = Layout::new(state_dir);
            let mut store = Store::open(&layout.database())?;
            let mut catalog = store.load()?;
            let document = fs::read_to_string(&file)?;
            let report = routeros::merge_routeros(&mut catalog, &document, &network)?;
            store.save(&catalog)?;
            apply::render(&layout, &catalog)?;
            println!(
                "imported {} reservations, skipped {}, networks {}",
                report.reservations,
                report.skipped,
                report.networks.join(",")
            );
            for warning in &report.warnings {
                println!("warning: {warning}");
            }
            if apply {
                let mut engine = Engine::open(layout, true, true)?;
                let result = engine.apply()?;
                println!("{}", result.message);
            }
        }
        Command::PullRouteros {
            state_dir,
            host,
            user,
            password_file,
            network,
        } => {
            let password = fs::read_to_string(&password_file)?;
            let document = routeros::pull(&host, &user, password.trim())?;
            let layout = Layout::new(&state_dir);
            fs::create_dir_all(&layout.state_dir)?;
            let snapshot = layout.state_dir.join("mikrotik-dhcp.json");
            fs::write(&snapshot, &document)?;
            let mut store = Store::open(&layout.database())?;
            let mut catalog = store.load()?;
            let report = routeros::merge_routeros(&mut catalog, &document, &network)?;
            store.save(&catalog)?;
            apply::render(&layout, &catalog)?;
            println!(
                "pulled {} from {host}, {} reservations, snapshot {}",
                report.networks.join(","),
                report.reservations,
                snapshot.display()
            );
            for warning in report.warnings {
                println!("warning: {warning}");
            }
        }
        Command::ReloadKea => {
            println!("{}", apply::reload_kea());
        }
        Command::SetRouterosDhcp {
            host,
            user,
            password_file,
            disabled,
        } => {
            let password = fs::read_to_string(&password_file)?;
            for line in routeros::set_dhcp_disabled(&host, &user, password.trim(), disabled)? {
                println!("{line}");
            }
        }
        Command::Status { state_dir } => {
            let store = Store::open(&Layout::new(state_dir).database())?;
            println!("{}", serde_json::to_string_pretty(&store.load()?)?);
        }
    }
    Ok(())
}
