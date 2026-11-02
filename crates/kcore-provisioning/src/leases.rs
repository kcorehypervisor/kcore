//! Read Kea's memfile lease database.
//!
//! Kea appends a row for every lease change, so the newest row for an address
//! wins. Rows with state 2 were reclaimed and no longer belong to anyone.

use crate::model::{normalize_mac, parse_cidr, Catalog};
use std::collections::HashMap;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRow {
    pub address: Ipv4Addr,
    pub mac_address: String,
    pub valid_lifetime: u32,
    pub expires_at: i64,
    pub hostname: String,
    pub state: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub network: String,
    pub row: LeaseRow,
    pub status: &'static str,
    pub reserved: bool,
}

pub fn parse(text: &str) -> Vec<LeaseRow> {
    let mut latest: HashMap<Ipv4Addr, LeaseRow> = HashMap::new();
    let mut order: Vec<Ipv4Addr> = Vec::new();
    let mut lines = text.lines();
    let columns: Vec<&str> = match lines.next() {
        Some(header) => header.split(',').collect(),
        None => return Vec::new(),
    };
    let index = |name: &str| columns.iter().position(|c| *c == name);
    let (Some(address), Some(hwaddr), Some(lifetime), Some(expire), Some(hostname), Some(state)) = (
        index("address"),
        index("hwaddr"),
        index("valid_lifetime"),
        index("expire"),
        index("hostname"),
        index("state"),
    ) else {
        return Vec::new();
    };
    for line in lines {
        let cells: Vec<&str> = line.split(',').collect();
        let get = |i: usize| cells.get(i).copied().unwrap_or("");
        let Ok(ip) = get(address).parse::<Ipv4Addr>() else {
            continue;
        };
        let row = LeaseRow {
            address: ip,
            mac_address: normalize_mac(get(hwaddr)).unwrap_or_else(|_| get(hwaddr).to_string()),
            valid_lifetime: get(lifetime).parse().unwrap_or(0),
            expires_at: get(expire).parse().unwrap_or(0),
            hostname: get(hostname).trim_end_matches('.').to_string(),
            state: get(state).parse().unwrap_or(0),
        };
        if latest.insert(ip, row).is_none() {
            order.push(ip);
        }
    }
    order
        .into_iter()
        .filter_map(|ip| latest.remove(&ip))
        .collect()
}

pub fn status(row: &LeaseRow, now: i64) -> &'static str {
    match row.state {
        1 => "declined",
        2 => "reclaimed",
        _ if row.expires_at <= now => "expired",
        _ => "active",
    }
}

/// Attach each lease to the network whose subnet holds its address.
pub fn classify(catalog: &Catalog, rows: Vec<LeaseRow>, now: i64) -> Vec<Lease> {
    let nets: Vec<_> = catalog
        .networks
        .iter()
        .filter_map(|n| parse_cidr(&n.cidr).ok().map(|net| (n.name.clone(), net)))
        .collect();
    let mut leases: Vec<Lease> = rows
        .into_iter()
        .filter_map(|row| {
            let (network, _) = nets.iter().find(|(_, net)| net.contains(row.address))?;
            let reserved = catalog.reservations.iter().any(|r| {
                r.network == *network && r.mac_address.eq_ignore_ascii_case(&row.mac_address)
            });
            Some(Lease {
                network: network.clone(),
                status: status(&row, now),
                reserved,
                row,
            })
        })
        .collect();
    leases.sort_by_key(|l| (l.network.clone(), u32::from(l.row.address)));
    leases
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AddressPool, Network, Reservation};

    const HEADER: &str =
        "address,hwaddr,client_id,valid_lifetime,expire,subnet_id,fqdn_fwd,fqdn_rev,hostname,state,user_context,pool_id";

    fn catalog() -> Catalog {
        Catalog {
            networks: vec![Network {
                name: "lan".into(),
                interface: "lan10g".into(),
                cidr: "192.168.40.0/24".into(),
                gateway: "192.168.40.1".into(),
                domain: "lab.local".into(),
                dns_servers: vec![],
                pools: vec![AddressPool {
                    start: "192.168.40.100".into(),
                    end: "192.168.40.199".into(),
                }],
                dhcp_enabled: true,
                valid_lifetime: 1800,
                next_server: String::new(),
                boot_file_name: String::new(),
            }],
            upstream: vec![],
            authoritative: "127.0.0.1:5300".into(),
            records: vec![],
            reservations: vec![Reservation {
                network: "lan".into(),
                mac_address: "aa:bb:cc:00:00:01".into(),
                ip_address: "192.168.40.107".into(),
                hostname: "node-107".into(),
                comment: String::new(),
                static_lease: true,
            }],
        }
    }

    #[test]
    fn newest_row_wins_and_reclaimed_rows_are_labelled() {
        let text = format!(
            "{HEADER}\n\
             192.168.40.112,48:9d:31:4a:9a:28,01,1800,100,1,0,0,old,0,,0\n\
             192.168.40.112,48:9d:31:4a:9a:28,01,1800,5000,1,0,0,espressif,0,,0\n\
             192.168.40.117,ec:71:db:a9:2e:15,01,1800,200,1,0,0,driveway,2,,0\n"
        );
        let rows = parse(&text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].hostname, "espressif");
        assert_eq!(status(&rows[0], 1000), "active");
        assert_eq!(status(&rows[0], 6000), "expired");
        assert_eq!(status(&rows[1], 1000), "reclaimed");
    }

    #[test]
    fn leases_join_their_network_and_reservation() {
        let text = format!(
            "{HEADER}\n\
             192.168.40.107,AA:BB:CC:00:00:01,01,1800,5000,1,0,0,node-107.,0,,0\n\
             10.9.9.9,aa:aa:aa:aa:aa:aa,01,1800,5000,1,0,0,,0,,0\n"
        );
        let leases = classify(&catalog(), parse(&text), 1000);
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].network, "lan");
        assert!(leases[0].reserved);
        assert_eq!(leases[0].row.hostname, "node-107");
    }

    #[test]
    fn a_missing_or_foreign_header_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("a,b,c\n1,2,3\n").is_empty());
    }
}
