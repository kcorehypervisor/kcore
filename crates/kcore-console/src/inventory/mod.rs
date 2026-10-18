//! Host inventory: network, disks, metadata, API probe, diagnostics.

pub mod api;
pub mod diagnostics;
pub mod disk;
pub mod format;
pub mod logs;
pub mod network;
pub mod route;
pub mod system;

use std::sync::OnceLock;

use self::system::Meta;

static PRODUCT: OnceLock<String> = OnceLock::new();

/// Records the `--product` name for the overview card. The first call wins.
pub fn set_product(name: &str) {
    let _ = PRODUCT.set(name.to_string());
}

pub(crate) fn product_label() -> String {
    match PRODUCT.get().map(String::as_str) {
        Some("vault") => "kcore vault".to_string(),
        Some(name) if !name.is_empty() => format!("kcore {name}"),
        _ => "kcore hypervisor".to_string(),
    }
}

/// Full snapshot for one UI refresh.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub meta: Meta,
    pub nics: Vec<network::Nic>,
    pub disks: Vec<disk::Disk>,
    pub diag: Vec<diagnostics::ServiceLine>,
    pub logs: Vec<logs::LogLine>,
}

/// Collects inventory (may be partially empty on errors).
pub fn load_snapshot() -> Snapshot {
    let api = api::probe();
    let nics = network::list_nics();
    let disks = disk::list_disks();
    let diag = diagnostics::kcore_diagnostics();
    let logs = logs::recent_system_logs(80);
    let meta = system::load_meta(&api);
    Snapshot {
        meta,
        nics,
        disks,
        diag,
        logs,
    }
}
