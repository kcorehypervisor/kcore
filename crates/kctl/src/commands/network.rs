use anyhow::Result;

use crate::client::{self, controller_proto as proto};
use crate::config::ConnectionInfo;

pub struct CreateArgs {
    pub name: String,
    pub external_ip: String,
    pub gateway_ip: String,
    pub internal_netmask: String,
    pub target_node: Option<String>,
    pub vlan_id: i32,
    pub network_type: String,
    pub enable_outbound_nat: bool,
}

pub async fn create(info: &ConnectionInfo, args: CreateArgs) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_network(proto::CreateNetworkRequest {
            name: args.name.clone(),
            external_ip: args.external_ip,
            gateway_ip: args.gateway_ip,
            internal_netmask: args.internal_netmask,
            target_node: args.target_node.unwrap_or_default(),
            allowed_tcp_ports: vec![],
            allowed_udp_ports: vec![],
            vlan_id: args.vlan_id,
            network_type: args.network_type,
            enable_outbound_nat: args.enable_outbound_nat,
        })
        .await?
        .into_inner();

    if resp.success {
        println!("Network '{}' created", args.name);
        println!("  Node: {}", resp.node_id);
        if !resp.message.is_empty() {
            println!("  Info: {}", resp.message);
        }
    } else {
        println!("Network '{}' creation rejected", args.name);
    }
    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str, target_node: Option<String>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    client
        .delete_network(proto::DeleteNetworkRequest {
            name: name.to_string(),
            target_node: target_node.unwrap_or_default(),
        })
        .await?;
    println!("Network '{name}' deleted");
    Ok(())
}

pub async fn list(info: &ConnectionInfo, target_node: Option<String>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_networks(proto::ListNetworksRequest {
            target_node: target_node.unwrap_or_default(),
        })
        .await?
        .into_inner();

    if resp.networks.is_empty() {
        println!("No custom networks found");
        return Ok(());
    }

    println!(
        "{:<20}  {:<7}  {:<16}  {:<16}  {:<15}  {:>4}  {:<16}",
        "NAME", "TYPE", "GATEWAY", "NETMASK", "EXTERNAL_IP", "VLAN", "NODE"
    );
    for n in &resp.networks {
        let vlan = if n.vlan_id > 0 {
            n.vlan_id.to_string()
        } else {
            "-".to_string()
        };
        let net_type = if n.network_type.is_empty() {
            "nat"
        } else {
            &n.network_type
        };
        println!(
            "{:<20}  {:<7}  {:<16}  {:<16}  {:<15}  {:>4}  {:<16}",
            n.name, net_type, n.gateway_ip, n.internal_netmask, n.external_ip, vlan, n.node_id
        );
    }
    Ok(())
}

pub async fn describe(info: &ConnectionInfo, name: &str, target_node: Option<String>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_networks(proto::ListNetworksRequest {
            target_node: target_node.clone().unwrap_or_default(),
        })
        .await?
        .into_inner();

    let mut matches: Vec<_> = resp.networks.into_iter().filter(|n| n.name == name).collect();
    if matches.is_empty() {
        anyhow::bail!("network '{name}' not found");
    }
    if matches.len() > 1 && target_node.is_none() {
        let nodes = matches
            .iter()
            .map(|n| n.node_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "network '{name}' exists on multiple nodes ({nodes}); rerun with --target-node"
        );
    }
    let net = matches.swap_remove(0);
    let net_type = if net.network_type.is_empty() {
        "nat".to_string()
    } else {
        net.network_type.clone()
    };
    let vlan = if net.vlan_id > 0 {
        net.vlan_id.to_string()
    } else {
        "-".to_string()
    };

    println!("Name:              {}", net.name);
    println!("Node:              {}", net.node_id);
    println!("Type:              {net_type}");
    println!("Gateway IP:        {}", net.gateway_ip);
    println!("Internal netmask:  {}", net.internal_netmask);
    if let Some(cidr) = ipv4_subnet_from_gateway_mask(&net.gateway_ip, &net.internal_netmask) {
        println!("Network CIDR:      {cidr}");
    }
    println!("External IP:       {}", net.external_ip);
    println!("VLAN:              {vlan}");
    println!(
        "Outbound NAT:      {}",
        if net.enable_outbound_nat {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "Allowed TCP ports: {}",
        if net.allowed_tcp_ports.is_empty() {
            "(none)".to_string()
        } else {
            net.allowed_tcp_ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!(
        "Allowed UDP ports: {}",
        if net.allowed_udp_ports.is_empty() {
            "(none)".to_string()
        } else {
            net.allowed_udp_ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    Ok(())
}

fn ipv4_subnet_from_gateway_mask(gateway_ip: &str, netmask: &str) -> Option<String> {
    fn parse_ipv4(ip: &str) -> Option<[u8; 4]> {
        let mut parts = ip.split('.');
        let a = parts.next()?.parse::<u8>().ok()?;
        let b = parts.next()?.parse::<u8>().ok()?;
        let c = parts.next()?.parse::<u8>().ok()?;
        let d = parts.next()?.parse::<u8>().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some([a, b, c, d])
    }

    let ip = parse_ipv4(gateway_ip)?;
    let mask = parse_ipv4(netmask)?;
    let network = [
        ip[0] & mask[0],
        ip[1] & mask[1],
        ip[2] & mask[2],
        ip[3] & mask[3],
    ];
    let prefix = mask.iter().map(|b| b.count_ones()).sum::<u32>();
    Some(format!(
        "{}.{}.{}.{}/{}",
        network[0], network[1], network[2], network[3], prefix
    ))
}
