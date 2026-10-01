//! Optional IPv6 addressing and VXLAN peer selection.
//!
//! IPv4 allocation is unchanged. A network grows an IPv6 /64 only when its
//! policy stores a prefix, and east-west filtering is off unless that policy
//! asks for it.

use std::net::Ipv6Addr;

use crate::db::NodeRow;

/// Host address inside a /64 whose host bits are zero.
///
/// `fd00:10:240::/64` with host 2 is `fd00:10:240::2`.
pub fn ipv6_for_host(prefix: &str, host: u32) -> Result<String, String> {
    let network = parse_prefix64(prefix)?;
    if host == 0 {
        return Err("IPv6 host id must be at least 1".to_string());
    }
    let bytes = network.octets();
    let mut addr = [0u8; 16];
    addr[..8].copy_from_slice(&bytes[..8]);
    let host_bytes = host.to_be_bytes();
    addr[12..].copy_from_slice(&host_bytes);
    Ok(Ipv6Addr::from(addr).to_string())
}

/// Gateway must be `prefix::1` and the prefix must be a /64 with host bits clear.
pub fn validate_ipv6_network(prefix: &str, gateway: &str) -> Result<(), String> {
    let expected = ipv6_for_host(prefix, 1)?;
    let gateway_addr: Ipv6Addr = gateway
        .parse()
        .map_err(|_| format!("invalid IPv6 gateway {gateway}"))?;
    let expected_addr: Ipv6Addr = expected
        .parse()
        .map_err(|_| format!("invalid IPv6 address {expected}"))?;
    if gateway_addr != expected_addr {
        return Err(format!(
            "IPv6 gateway must be {expected} (the first address of {prefix})"
        ));
    }
    Ok(())
}

fn parse_prefix64(prefix: &str) -> Result<Ipv6Addr, String> {
    let (addr, len) = prefix
        .split_once('/')
        .ok_or_else(|| format!("IPv6 prefix {prefix} must be a /64"))?;
    if len != "64" {
        return Err(format!("IPv6 prefix {prefix} must be a /64"));
    }
    let parsed: Ipv6Addr = addr
        .parse()
        .map_err(|_| format!("invalid IPv6 prefix {prefix}"))?;
    let bytes = parsed.octets();
    if bytes[8..].iter().any(|b| *b != 0) {
        return Err(format!(
            "IPv6 prefix {prefix} must have its host bits clear"
        ));
    }
    Ok(parsed)
}

/// VXLAN FDB peers: other ready, approved nodes, one row each, sorted by id.
pub fn vxlan_peer_nodes<'a>(nodes: &'a [NodeRow], self_id: &str) -> Vec<&'a NodeRow> {
    let mut peers: Vec<&NodeRow> = nodes
        .iter()
        .filter(|node| {
            node.id != self_id && node.status == "ready" && node.approval_status == "approved"
        })
        .collect();
    peers.sort_by(|a, b| a.id.cmp(&b.id));
    peers.dedup_by(|a, b| a.id == b.id);
    peers
}

/// Pool key for a released IPv6 address.
///
/// VXLAN shares one L2 domain, so the pool is the network name. NAT and
/// bridge networks are local to a node.
pub fn ipv6_scope(global: bool, node_id: &str, network: &str) -> String {
    if global {
        network.to_string()
    } else {
        format!("{node_id}/{network}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, status: &str, approval: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            hostname: id.into(),
            address: format!("192.168.40.{id}:9091"),
            cpu_cores: 4,
            memory_bytes: 8,
            status: status.into(),
            last_heartbeat: String::new(),
            gateway_interface: String::new(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: approval.into(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".into(),
        }
    }

    #[test]
    fn ipv6_host_addresses_stay_inside_the_prefix() {
        assert_eq!(
            ipv6_for_host("fd00:10:240::/64", 1).unwrap(),
            "fd00:10:240::1"
        );
        assert_eq!(
            ipv6_for_host("fd00:10:240::/64", 2).unwrap(),
            "fd00:10:240::2"
        );
        assert!(ipv6_for_host("fd00:10:240::1/64", 2).is_err());
        assert!(ipv6_for_host("fd00:10:240::/48", 2).is_err());
        assert!(validate_ipv6_network("fd00:10:240::/64", "fd00:10:240::1").is_ok());
        assert!(validate_ipv6_network("fd00:10:240::/64", "fd00:10:240::2").is_err());
    }

    #[test]
    fn vxlan_peers_skip_self_and_nodes_that_are_not_ready() {
        let nodes = vec![
            node("node-c", "ready", "approved"),
            node("node-a", "ready", "approved"),
            node("node-b", "not-ready", "approved"),
            node("node-d", "ready", "pending"),
            node("node-a", "ready", "approved"),
        ];
        let peers = vxlan_peer_nodes(&nodes, "node-c");
        assert_eq!(
            peers.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["node-a"]
        );
    }
}
