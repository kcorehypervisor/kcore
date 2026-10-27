//! Desired state for every network the provisioner can serve.

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub upstream: Vec<String>,
    /// `host:port` of the authoritative PowerDNS listener.
    pub authoritative: String,
    pub networks: Vec<Network>,
    pub reservations: Vec<Reservation>,
    pub records: Vec<DnsRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    pub name: String,
    pub interface: String,
    pub cidr: String,
    pub gateway: String,
    pub domain: String,
    #[serde(default)]
    pub dns_servers: Vec<String>,
    #[serde(default)]
    pub pools: Vec<AddressPool>,
    #[serde(default)]
    pub dhcp_enabled: bool,
    #[serde(default = "default_lifetime")]
    pub valid_lifetime: u32,
    #[serde(default)]
    pub next_server: String,
    #[serde(default)]
    pub boot_file_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressPool {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub network: String,
    pub mac_address: String,
    pub ip_address: String,
    pub hostname: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub static_lease: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRecord {
    pub zone: String,
    pub name: String,
    pub rrtype: String,
    pub content: String,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Net {
    pub addr: u32,
    pub prefix: u8,
}

fn default_lifetime() -> u32 {
    1800
}

fn default_ttl() -> u32 {
    3600
}

impl Catalog {
    pub fn validate(&self) -> Result<(), String> {
        if self.upstream.is_empty() {
            return Err("at least one upstream DNS server is required".into());
        }
        for server in &self.upstream {
            parse_ipv4(server).map_err(|e| format!("upstream {server}: {e}"))?;
        }
        parse_host_port(&self.authoritative)?;
        if self.networks.is_empty() {
            return Err("at least one network is required".into());
        }
        let mut nets = Vec::with_capacity(self.networks.len());
        for network in &self.networks {
            network.validate()?;
            let parsed = parse_cidr(&network.cidr)?;
            for (other_name, other) in &nets {
                if parsed.overlaps(*other) {
                    return Err(format!("network {} overlaps {}", network.name, other_name));
                }
            }
            nets.push((network.name.clone(), parsed));
        }
        let mut macs = std::collections::BTreeSet::new();
        for reservation in &self.reservations {
            let network = self
                .networks
                .iter()
                .find(|n| n.name == reservation.network)
                .ok_or_else(|| {
                    format!(
                        "reservation {} is on unknown network {}",
                        reservation.mac_address, reservation.network
                    )
                })?;
            let net = parse_cidr(&network.cidr)?;
            let ip = parse_ipv4(&reservation.ip_address)?;
            if !net.contains(ip) {
                return Err(format!(
                    "{} is outside {}",
                    reservation.ip_address, network.cidr
                ));
            }
            normalize_mac(&reservation.mac_address)?;
            if !macs.insert(reservation.mac_address.to_ascii_lowercase()) {
                return Err(format!(
                    "duplicate mac {}",
                    reservation.mac_address.to_ascii_lowercase()
                ));
            }
            if !reservation.hostname.is_empty() {
                dns_label(&reservation.hostname).ok_or_else(|| {
                    format!("hostname {} is not a dns label", reservation.hostname)
                })?;
            }
        }
        for record in &self.records {
            if record.rrtype.is_empty() || record.name.is_empty() || record.content.is_empty() {
                return Err("dns record needs a name, type, and content".into());
            }
        }
        Ok(())
    }

    pub fn network(&self, name: &str) -> Option<&Network> {
        self.networks.iter().find(|n| n.name == name)
    }

    pub fn enabled_networks(&self) -> impl Iterator<Item = &Network> {
        self.networks.iter().filter(|n| n.dhcp_enabled)
    }
}

impl Network {
    pub fn validate(&self) -> Result<(), String> {
        if !is_name(&self.name) {
            return Err(format!("network name {} is invalid", self.name));
        }
        let net = parse_cidr(&self.cidr)?;
        let gateway = parse_ipv4(&self.gateway).map_err(|e| format!("gateway: {e}"))?;
        if !net.contains(gateway) {
            return Err(format!("gateway {} is outside {}", self.gateway, self.cidr));
        }
        if !is_domain(&self.domain) {
            return Err(format!("domain {} is invalid", self.domain));
        }
        if self.dhcp_enabled {
            if self.interface.is_empty() {
                return Err(format!(
                    "network {} serves dhcp and needs an interface",
                    self.name
                ));
            }
            if self.pools.is_empty() {
                return Err(format!(
                    "network {} serves dhcp and needs a pool",
                    self.name
                ));
            }
            if self.dns_servers.is_empty() {
                return Err(format!(
                    "network {} serves dhcp and needs a dns server",
                    self.name
                ));
            }
            if self.valid_lifetime < 60 {
                return Err(format!(
                    "network {} lifetime {} is below 60s",
                    self.name, self.valid_lifetime
                ));
            }
        }
        for server in &self.dns_servers {
            parse_ipv4(server).map_err(|e| format!("dns server {server}: {e}"))?;
        }
        if !self.next_server.is_empty() {
            parse_ipv4(&self.next_server).map_err(|e| format!("next-server: {e}"))?;
        }
        for pool in &self.pools {
            let start = parse_ipv4(&pool.start)?;
            let end = parse_ipv4(&pool.end)?;
            if u32::from(start) > u32::from(end) {
                return Err(format!("pool {}-{} is reversed", pool.start, pool.end));
            }
            if !net.contains(start) || !net.contains(end) {
                return Err(format!(
                    "pool {}-{} is outside {}",
                    pool.start, pool.end, self.cidr
                ));
            }
        }
        Ok(())
    }

    pub fn parsed(&self) -> Result<Ipv4Net, String> {
        parse_cidr(&self.cidr)
    }
}

impl Ipv4Net {
    pub fn contains(self, ip: Ipv4Addr) -> bool {
        let mask = mask(self.prefix);
        (u32::from(ip) & mask) == (self.addr & mask)
    }

    pub fn overlaps(self, other: Self) -> bool {
        let prefix = self.prefix.min(other.prefix);
        let mask = mask(prefix);
        (self.addr & mask) == (other.addr & mask)
    }

    pub fn canonical(self) -> String {
        format!("{}/{}", Ipv4Addr::from(self.addr), self.prefix)
    }

    /// Reverse zones covering this prefix, one per contained /24.
    pub fn reverse_zones(self) -> Vec<String> {
        if self.prefix > 24 {
            return vec![reverse_zone(self.addr)];
        }
        let mut zones = Vec::new();
        let count = 1u32 << (24 - self.prefix);
        let base = self.addr;
        for index in 0..count {
            zones.push(reverse_zone(base.wrapping_add(index << 8)));
        }
        zones
    }
}

pub fn parse_ipv4(text: &str) -> Result<Ipv4Addr, String> {
    let text = text.trim().trim_end_matches('.');
    text.parse::<Ipv4Addr>()
        .map_err(|_| format!("invalid ipv4 address {text}"))
}

pub fn parse_cidr(text: &str) -> Result<Ipv4Net, String> {
    let (addr, prefix) = text
        .trim()
        .split_once('/')
        .ok_or_else(|| format!("cidr {text} needs a prefix"))?;
    let addr = parse_ipv4(addr)?;
    let prefix: u8 = prefix
        .parse()
        .map_err(|_| format!("cidr prefix in {text} is invalid"))?;
    if prefix > 32 {
        return Err(format!("cidr prefix {prefix} is above 32"));
    }
    let mask = mask(prefix);
    Ok(Ipv4Net {
        addr: u32::from(addr) & mask,
        prefix,
    })
}

pub fn parse_host_port(text: &str) -> Result<(String, u16), String> {
    let (host, port) = text
        .rsplit_once(':')
        .ok_or_else(|| format!("{text} is not host:port"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("port in {text} is invalid"))?;
    if host.is_empty() {
        return Err(format!("{text} is missing a host"));
    }
    Ok((host.to_string(), port))
}

pub fn normalize_mac(text: &str) -> Result<String, String> {
    let hex: String = text
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() != 12 {
        return Err(format!("mac {text} is invalid"));
    }
    let mut out = String::with_capacity(17);
    for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
        if index > 0 {
            out.push(':');
        }
        out.push_str(std::str::from_utf8(chunk).expect("hex is ascii"));
    }
    Ok(out)
}

pub fn dns_label(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let source = if raw.split_whitespace().nth(1).is_some() {
        raw.split_whitespace().next().unwrap_or(raw)
    } else {
        raw
    };
    let mut out = String::new();
    let mut dash = false;
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() || out.len() > 63 || out.starts_with('-') {
        None
    } else {
        Some(out)
    }
}

pub fn is_domain(text: &str) -> bool {
    let text = text.trim().trim_end_matches('.');
    !text.is_empty()
        && text.len() <= 253
        && !text.starts_with('.')
        && !text.ends_with('.')
        && text
            .split('.')
            .all(|label| dns_label(label).as_deref() == Some(label.to_ascii_lowercase().as_str()))
}

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    text.len() <= 32
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn reverse_zone(addr: u32) -> String {
    let ip = Ipv4Addr::from(addr);
    let [a, b, c, _] = ip.octets();
    format!("{c}.{b}.{a}.in-addr.arpa")
}

pub fn fqdn(name: &str) -> String {
    let name = name.trim().trim_end_matches('.');
    format!("{name}.")
}

/// RouterOS durations such as `30m`, `1d`, and `01:00:00`.
pub fn parse_duration(text: &str) -> Result<u32, String> {
    let text = text.trim();
    if let Some((hours, rest)) = text.split_once(':') {
        let hours: u32 = hours.parse().map_err(|_| format!("duration {text}"))?;
        let (minutes, seconds) = match rest.split_once(':') {
            Some((minutes, seconds)) => (minutes, seconds),
            None => (rest, "0"),
        };
        let minutes: u32 = minutes.parse().map_err(|_| format!("duration {text}"))?;
        let seconds: u32 = seconds.parse().map_err(|_| format!("duration {text}"))?;
        return Ok(hours * 3600 + minutes * 60 + seconds);
    }
    let mut total: u32 = 0;
    let mut number = String::new();
    let mut saw = false;
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        if number.is_empty() {
            continue;
        }
        let value: u32 = number.parse().map_err(|_| format!("duration {text}"))?;
        number.clear();
        let unit = match ch {
            'w' => 7 * 24 * 3600,
            'd' => 24 * 3600,
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return Err(format!("duration {text} has unit {ch}")),
        };
        total = total.saturating_add(value.saturating_mul(unit));
        saw = true;
    }
    if !saw {
        return Err(format!("duration {text} is invalid"));
    }
    Ok(total)
}

pub fn assign_hostnames(reservations: &mut [Reservation]) {
    let mut used = std::collections::BTreeSet::new();
    for reservation in reservations.iter_mut() {
        let octet = reservation
            .ip_address
            .rsplit('.')
            .next()
            .unwrap_or("0")
            .to_string();
        let from_host = dns_label(&reservation.hostname);
        let from_comment = dns_label(&reservation.comment);
        let mut chosen = from_host
            .clone()
            .or_else(|| from_comment.clone())
            .unwrap_or_else(|| format!("host-{octet}"));
        if used.contains(&chosen) {
            if let Some(comment) = from_comment.clone() {
                if comment != chosen && !used.contains(&comment) {
                    chosen = comment;
                }
            }
        }
        if used.contains(&chosen) {
            chosen = format!("{chosen}-{octet}");
        }
        let mut suffix = 2u32;
        let base = chosen.clone();
        while used.contains(&chosen) {
            chosen = format!("{base}-{suffix}");
            suffix += 1;
        }
        used.insert(chosen.clone());
        reservation.hostname = chosen;
    }
}

#[cfg(test)]
pub(crate) fn sample_catalog() -> Catalog {
    Catalog {
        upstream: vec!["1.1.1.1".into(), "8.8.8.8".into()],
        authoritative: "127.0.0.1:5300".into(),
        networks: vec![
            Network {
                name: "lan".into(),
                interface: "lan10g".into(),
                cidr: "192.168.40.0/24".into(),
                gateway: "192.168.40.1".into(),
                domain: "home.arpa".into(),
                dns_servers: vec!["192.168.40.10".into()],
                pools: vec![AddressPool {
                    start: "192.168.40.100".into(),
                    end: "192.168.40.200".into(),
                }],
                dhcp_enabled: true,
                valid_lifetime: 1800,
                next_server: "192.168.40.10".into(),
                boot_file_name: "http://192.168.40.10/boot".into(),
            },
            Network {
                name: "garage".into(),
                interface: "vlan50".into(),
                cidr: "192.168.50.0/24".into(),
                gateway: "192.168.50.1".into(),
                domain: "garage.home.arpa".into(),
                dns_servers: vec!["192.168.40.10".into()],
                pools: vec![AddressPool {
                    start: "192.168.50.100".into(),
                    end: "192.168.50.200".into(),
                }],
                dhcp_enabled: true,
                valid_lifetime: 1800,
                next_server: String::new(),
                boot_file_name: String::new(),
            },
        ],
        reservations: vec![Reservation {
            network: "lan".into(),
            mac_address: "00:00:5e:00:53:21".into(),
            ip_address: "192.168.40.105".into(),
            hostname: "node-105".into(),
            comment: "node-105".into(),
            static_lease: true,
        }],
        records: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidrs_overlap_and_reverse() {
        let lan = parse_cidr("192.168.40.10/24").unwrap();
        let other = parse_cidr("192.168.50.0/24").unwrap();
        let wide = parse_cidr("192.168.40.0/23").unwrap();
        assert!(!lan.overlaps(other));
        assert!(lan.overlaps(wide));
        assert!(lan.contains(parse_ipv4("192.168.40.200").unwrap()));
        assert!(!lan.contains(parse_ipv4("192.168.41.1").unwrap()));
        assert_eq!(
            lan.reverse_zones(),
            vec!["40.168.192.in-addr.arpa".to_string()]
        );
        assert_eq!(wide.reverse_zones().len(), 2);
    }

    #[test]
    fn durations_and_macs() {
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("1d").unwrap(), 86400);
        assert_eq!(parse_duration("01:02:03").unwrap(), 3723);
        assert_eq!(
            normalize_mac("8C-16-45-D6-C4-8C").unwrap(),
            "8c:16:45:d6:c4:8c"
        );
        assert!(normalize_mac("zz").is_err());
    }

    #[test]
    fn hostname_collision_uses_comment() {
        let mut rows = vec![
            Reservation {
                network: "lan".into(),
                mac_address: "00:00:5e:00:53:01".into(),
                ip_address: "192.168.40.10".into(),
                hostname: "kvm-node".into(),
                comment: "node-103 br0/eno1 (Ceph mon)".into(),
                static_lease: true,
            },
            Reservation {
                network: "lan".into(),
                mac_address: "00:00:5e:00:53:02".into(),
                ip_address: "192.168.40.11".into(),
                hostname: "kvm-node".into(),
                comment: "node-113".into(),
                static_lease: true,
            },
        ];
        assign_hostnames(&mut rows);
        assert_eq!(rows[0].hostname, "kvm-node");
        assert_eq!(rows[1].hostname, "node-113");
    }

    #[test]
    fn two_networks_validate() {
        let catalog = sample_catalog();
        catalog.validate().unwrap();
    }

    #[test]
    fn overlapping_networks_fail() {
        let mut catalog = sample_catalog();
        catalog.networks[1].cidr = "192.168.40.0/23".into();
        catalog.networks[1].gateway = "192.168.41.1".into();
        catalog.networks[1].pools[0].start = "192.168.41.100".into();
        catalog.networks[1].pools[0].end = "192.168.41.150".into();
        assert!(catalog.validate().unwrap_err().contains("overlaps"));
    }
}
