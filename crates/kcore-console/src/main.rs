//! kcore hypervisor appliance TUI (Ratatui / crossterm).

use clap::Parser;

/// kcore local appliance console: read-only status, no local shell.
#[derive(Parser, Debug)]
#[command(name = "kcore-console", version, about = "kcore hypervisor appliance TUI", long_about = None)]
struct Cli {
    /// Development: allow q / Ctrl+C to exit; print extra diagnostics to stderr.
    #[arg(long)]
    dev: bool,
    /// Attach to this TTY (dup2 stdin/out/err). Typical: /dev/tty1 under systemd.
    #[arg(long, value_name = "PATH")]
    tty: Option<String>,
    /// Appliance product shown on the overview card (`vault` on the backup host).
    #[arg(long, value_name = "NAME")]
    product: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    if let Some(product) = cli.product.as_deref() {
        kcore_console::inventory::set_product(product);
    }
    let opts = kcore_console::Options {
        dev: cli.dev,
        tty: cli.tty,
    };
    if let Err(e) = kcore_console::run(opts) {
        eprintln!("kcore-console: {e}");
        std::process::exit(1);
    }
}
