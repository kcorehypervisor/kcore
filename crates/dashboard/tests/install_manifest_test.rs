//! Guardrails: production install paths must not regress to loopback for controller gRPC
//! or use a TLS domain that doesn't match the controller cert SANs.

use std::path::Path;

fn read_flake() -> Option<String> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let flake = manifest_dir.join("../../flake.nix");
    std::fs::read_to_string(&flake).ok()
}

#[test]
fn iso_install_dashboard_env_uses_host_ip_not_loopback_for_controller() {
    let Some(content) = read_flake() else {
        eprintln!("skipping: flake.nix not available in this build sandbox");
        return;
    };

    assert!(
        !content.contains("KCORE_CONTROLLER=127.0.0.1:9090"),
        "install-to-disk must not set KCORE_CONTROLLER to 127.0.0.1:9090 (not in controller mTLS SANs); use EXTERNAL_IP"
    );
    assert!(
        content.contains("KCORE_CONTROLLER=$EXTERNAL_IP:9090"),
        "install-generated dashboard.env must dial the host IP: KCORE_CONTROLLER=$EXTERNAL_IP:9090"
    );
}

#[test]
fn iso_install_dashboard_env_no_tls_domain_override() {
    let Some(content) = read_flake() else {
        eprintln!("skipping: flake.nix not available in this build sandbox");
        return;
    };

    assert!(
        !content.contains("KCORE_TLS_DOMAIN="),
        "dashboard.env must not set KCORE_TLS_DOMAIN: the controller cert uses an IP SAN \
         (from EXTERNAL_IP) not a hostname SAN, so TLS verification must use the IP from \
         KCORE_CONTROLLER directly"
    );
}
