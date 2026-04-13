use std::path::Path;

use tonic::Status;

#[allow(clippy::result_large_err)]
pub fn ensure_bridge_backed_network(network: &str) -> Result<(), Status> {
    let net = network.trim();
    if net.is_empty() || net == "bridge" || net == "host" || net == "none" {
        return Ok(());
    }
    let bridge_name = if net.starts_with("kbr-") {
        net.to_string()
    } else {
        format!("kbr-{net}")
    };
    let bridge_path = format!("/sys/class/net/{bridge_name}");
    if Path::new(&bridge_path).exists() {
        Ok(())
    } else {
        Err(Status::failed_precondition(format!(
            "network '{net}' is not available on host bridge '{bridge_name}'"
        )))
    }
}
