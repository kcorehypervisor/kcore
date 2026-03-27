//! Integration-style checks for global dashboard config (serial: one test process).

use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

#[test]
fn dashboard_config_sets_once() {
    let _g = LOCK.lock().expect("lock");
    assert!(kcore_dashboard::state::set_dashboard_config(
        kcore_dashboard::config::DashboardConfig::local_insecure()
    )
    .is_ok());
    assert!(kcore_dashboard::state::set_dashboard_config(
        kcore_dashboard::config::DashboardConfig::local_insecure()
    )
    .is_err());
}
