//! Read RouterOS DHCP configuration and turn it into catalog changes.

use crate::model::{
    assign_hostnames, dns_label, normalize_mac, parse_cidr, parse_duration, parse_ipv4,
    AddressPool, Catalog, Network, Reservation,
};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub reservations: usize,
    pub skipped: usize,
    pub warnings: Vec<String>,
    pub networks: Vec<String>,
}

pub fn merge_routeros(catalog: &mut Catalog, document: &str, target: &str) -> Result<ImportReport> {
    let parsed = parse_document(document)?;
    if !catalog.networks.iter().any(|n| n.name == target) && parsed.active.is_none() {
        return Err(anyhow!("network {target} does not exist"));
    }
    let dns_servers = catalog
        .network(target)
        .map(|n| n.dns_servers.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| vec!["192.168.40.10".into()]);
    let domain = catalog
        .network(target)
        .map(|n| n.domain.clone())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "home.arpa".into());
    let interface = catalog
        .network(target)
        .map(|n| n.interface.clone())
        .unwrap_or_default();

    let mut warnings = Vec::new();
    let mut touched = Vec::new();
    if let Some(active) = &parsed.active {
        let network = upsert_target(catalog, target, active, &interface, &domain, &dns_servers);
        if !active.previous_dns.is_empty() {
            warnings.push(format!(
                "dhcp dns option becomes {}; mikrotik advertised {}",
                dns_servers.join(","),
                active.previous_dns.join(",")
            ));
        }
        touched.push(network);
    }

    for extra in &parsed.inactive {
        let name = unique_name(catalog, &extra.comment, &extra.cidr);
        if catalog.network(&name).is_none() {
            let slug = dns_label(&extra.comment).unwrap_or_else(|| name.clone());
            catalog.networks.push(Network {
                name: name.clone(),
                interface: String::new(),
                cidr: extra.cidr.clone(),
                gateway: extra.gateway.clone(),
                domain: format!("{slug}.home.arpa"),
                dns_servers: dns_servers.clone(),
                pools: Vec::new(),
                dhcp_enabled: false,
                valid_lifetime: extra.lifetime,
                next_server: String::new(),
                boot_file_name: String::new(),
            });
        }
        touched.push(name);
    }

    catalog
        .reservations
        .retain(|r| !touched.contains(&r.network));
    let mut skipped = 0usize;
    let mut imported = Vec::new();
    for lease in parsed.leases {
        if lease.disabled || lease.blocked {
            skipped += 1;
            warnings.push(format!("skipped disabled lease {}", lease.address));
            continue;
        }
        let Ok(mac) = normalize_mac(&lease.mac) else {
            skipped += 1;
            warnings.push(format!(
                "skipped lease {} with mac {}",
                lease.address, lease.mac
            ));
            continue;
        };
        let Ok(ip) = parse_ipv4(&lease.address) else {
            skipped += 1;
            warnings.push(format!("skipped lease address {}", lease.address));
            continue;
        };
        let Some(network) = catalog
            .networks
            .iter()
            .find(|n| n.parsed().map(|net| net.contains(ip)).unwrap_or(false))
        else {
            skipped += 1;
            warnings.push(format!(
                "skipped {} because it is outside every network",
                lease.address
            ));
            continue;
        };
        imported.push(Reservation {
            network: network.name.clone(),
            mac_address: mac,
            ip_address: lease.address,
            hostname: lease.hostname,
            comment: lease.comment,
            static_lease: lease.static_lease,
        });
    }
    assign_hostnames(&mut imported);
    let count = imported.len();
    catalog.reservations.extend(imported);
    catalog
        .validate()
        .map_err(|e| anyhow!("imported catalog is invalid: {e}"))?;
    Ok(ImportReport {
        reservations: count,
        skipped,
        warnings,
        networks: touched,
    })
}

fn upsert_target(
    catalog: &mut Catalog,
    target: &str,
    active: &RouterNetwork,
    interface: &str,
    domain: &str,
    dns_servers: &[String],
) -> String {
    if let Some(network) = catalog.networks.iter_mut().find(|n| n.name == target) {
        network.cidr = active.cidr.clone();
        network.gateway = active.gateway.clone();
        network.pools = active.pools.clone();
        network.valid_lifetime = active.lifetime;
        network.next_server = active.next_server.clone();
        network.boot_file_name = active.boot_file_name.clone();
        if network.domain.is_empty() {
            network.domain = domain.to_string();
        }
        if network.dns_servers.is_empty() {
            network.dns_servers = dns_servers.to_vec();
        }
        if network.interface.is_empty() && !interface.is_empty() {
            network.interface = interface.to_string();
        }
        return target.to_string();
    }
    catalog.networks.push(Network {
        name: target.to_string(),
        interface: interface.to_string(),
        cidr: active.cidr.clone(),
        gateway: active.gateway.clone(),
        domain: domain.to_string(),
        dns_servers: dns_servers.to_vec(),
        pools: active.pools.clone(),
        dhcp_enabled: !interface.is_empty(),
        valid_lifetime: active.lifetime,
        next_server: active.next_server.clone(),
        boot_file_name: active.boot_file_name.clone(),
    });
    target.to_string()
}

fn unique_name(catalog: &Catalog, comment: &str, cidr: &str) -> String {
    let base = dns_label(comment).unwrap_or_else(|| {
        cidr.split('.')
            .nth(2)
            .map(|o| format!("net{o}"))
            .unwrap_or_else(|| "net".into())
    });
    if catalog.network(&base).is_none() && base != "server" && base != "lan" {
        return base;
    }
    let octet = cidr.split(['.', '/']).nth(2).unwrap_or("0");
    format!("net{octet}")
}

struct Parsed {
    active: Option<RouterNetwork>,
    inactive: Vec<RouterNetwork>,
    leases: Vec<RouterLease>,
}

struct RouterNetwork {
    cidr: String,
    gateway: String,
    pools: Vec<AddressPool>,
    lifetime: u32,
    next_server: String,
    boot_file_name: String,
    previous_dns: Vec<String>,
    comment: String,
}

struct RouterLease {
    address: String,
    mac: String,
    hostname: String,
    comment: String,
    static_lease: bool,
    disabled: bool,
    blocked: bool,
}

fn parse_document(document: &str) -> Result<Parsed> {
    let trimmed = document.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        parse_json(trimmed)
    } else {
        parse_export(trimmed)
    }
}

fn parse_json(document: &str) -> Result<Parsed> {
    let value: Value = serde_json::from_str(document).context("routeros json")?;
    if let Some(rows) = value.as_array() {
        return Ok(Parsed {
            active: None,
            inactive: Vec::new(),
            leases: rows.iter().filter_map(lease_from_json).collect(),
        });
    }
    let leases = array_of(&value, &["/ip/dhcp-server/lease/print", "leases"])
        .into_iter()
        .filter_map(lease_from_json)
        .collect();
    let pools = array_of(&value, &["/ip/pool/print", "pools"]);
    let servers = array_of(&value, &["/ip/dhcp-server/print", "servers"]);
    let networks = array_of(&value, &["/ip/dhcp-server/network/print", "networks"]);
    Ok(assemble(pools, servers, networks, leases))
}

fn array_of<'a>(value: &'a Value, keys: &[&str]) -> Vec<&'a Value> {
    for key in keys {
        if let Some(rows) = value.get(*key).and_then(|v| v.as_array()) {
            return rows.iter().collect();
        }
    }
    Vec::new()
}

fn lease_from_json(value: &Value) -> Option<RouterLease> {
    let address = value
        .get("address")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| value.get("active-address").and_then(|v| v.as_str()))?
        .to_string();
    let mac = value
        .get("mac-address")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| value.get("active-mac-address").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    Some(RouterLease {
        address,
        mac,
        hostname: value
            .get("host-name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        comment: value
            .get("comment")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        static_lease: flag(value, "dynamic") == Some(false),
        disabled: flag(value, "disabled") == Some(true),
        blocked: flag(value, "blocked") == Some(true),
    })
}

fn flag(value: &Value, key: &str) -> Option<bool> {
    match value.get(key)? {
        Value::Bool(v) => Some(*v),
        Value::String(s) => match s.as_str() {
            "true" | "yes" => Some(true),
            "false" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn assemble(
    pools: Vec<&Value>,
    servers: Vec<&Value>,
    networks: Vec<&Value>,
    leases: Vec<RouterLease>,
) -> Parsed {
    let pool_ranges: BTreeMap<String, Vec<AddressPool>> = pools
        .iter()
        .filter_map(|pool| {
            let name = pool.get("name")?.as_str()?.to_string();
            let ranges = pool.get("ranges")?.as_str().unwrap_or("");
            Some((name, parse_ranges(ranges)))
        })
        .collect();
    let mut active_pool = None;
    let mut lifetime = 1800u32;
    for server in &servers {
        if flag(server, "disabled") == Some(true) {
            continue;
        }
        if let Some(pool) = server.get("address-pool").and_then(|v| v.as_str()) {
            active_pool = Some(pool.to_string());
        }
        if let Some(text) = server.get("lease-time").and_then(|v| v.as_str()) {
            if let Ok(seconds) = parse_duration(text) {
                lifetime = seconds;
            }
        }
    }
    let ranges = active_pool
        .as_ref()
        .and_then(|name| pool_ranges.get(name))
        .cloned()
        .unwrap_or_default();
    let mut active = None;
    let mut inactive = Vec::new();
    for network in networks {
        let Some(cidr) = network.get("address").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(parsed) = parse_cidr(cidr) else {
            continue;
        };
        let gateway = network
            .get("gateway")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .split([',', '/'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let item = RouterNetwork {
            cidr: parsed.canonical(),
            gateway,
            pools: if ranges.iter().any(|p| {
                parse_ipv4(&p.start)
                    .map(|ip| parsed.contains(ip))
                    .unwrap_or(false)
            }) {
                ranges.clone()
            } else {
                Vec::new()
            },
            lifetime,
            next_server: network
                .get("next-server")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            boot_file_name: network
                .get("boot-file-name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            previous_dns: network
                .get("dns-server")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            comment: network
                .get("comment")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        };
        if !item.pools.is_empty() && active.is_none() {
            active = Some(item);
        } else {
            inactive.push(item);
        }
    }
    Parsed {
        active,
        inactive,
        leases,
    }
}

fn parse_ranges(text: &str) -> Vec<AddressPool> {
    text.split(',')
        .filter_map(|range| {
            let (start, end) = range.trim().split_once('-')?;
            Some(AddressPool {
                start: start.trim().to_string(),
                end: end.trim().to_string(),
            })
        })
        .collect()
}

fn parse_export(document: &str) -> Result<Parsed> {
    let mut section = String::new();
    let mut pools = Vec::new();
    let mut servers = Vec::new();
    let mut networks = Vec::new();
    let mut leases = Vec::new();
    for line in document.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('/') {
            section = rest.to_string();
            continue;
        }
        let Some(rest) = line.strip_prefix("add ") else {
            continue;
        };
        let fields = parse_fields(rest);
        let value = Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        );
        match section.as_str() {
            "ip pool" => pools.push(value),
            "ip dhcp-server" => servers.push(value),
            "ip dhcp-server network" => networks.push(value),
            "ip dhcp-server lease" => leases.push(value),
            _ => {}
        }
    }
    Ok(assemble(
        pools.iter().collect(),
        servers.iter().collect(),
        networks.iter().collect(),
        leases.iter().filter_map(lease_from_json).collect(),
    ))
}

fn parse_fields(text: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        if let Some(quoted) = after.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        value.push(next);
                    }
                } else if ch == '"' {
                    break;
                } else {
                    value.push(ch);
                }
            }
            fields.push((key.to_string(), value));
            rest = chars.as_str();
        } else {
            let (value, tail) = after.split_once(' ').unwrap_or((after, ""));
            fields.push((key.to_string(), value.to_string()));
            rest = tail;
        }
    }
    fields
}

pub fn pull(host: &str, user: &str, password: &str) -> Result<String> {
    let mut stream = connect(host)?;
    login(&mut stream, user, password)?;
    let mut dump = serde_json::Map::new();
    for command in [
        "/system/identity/print",
        "/ip/dhcp-server/print",
        "/ip/pool/print",
        "/ip/dhcp-server/network/print",
        "/ip/dhcp-server/lease/print",
        "/ip/dns/print",
    ] {
        let rows = print_cmd(&mut stream, command)?;
        dump.insert(command.to_string(), Value::Array(rows));
    }
    Ok(serde_json::to_string_pretty(&Value::Object(dump))?)
}

pub fn set_dhcp_disabled(
    host: &str,
    user: &str,
    password: &str,
    disabled: bool,
) -> Result<Vec<String>> {
    let mut stream = connect(host)?;
    login(&mut stream, user, password)?;
    let rows = print_cmd(&mut stream, "/ip/dhcp-server/print")?;
    let mut changed = Vec::new();
    let flag = if disabled { "yes" } else { "no" };
    for row in rows {
        let Some(id) = row.get(".id").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = row
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();
        let already = flag(&row, "disabled") == Some(disabled);
        if already {
            changed.push(format!("{name} already disabled={flag}"));
            continue;
        }
        sentence(
            &mut stream,
            &[
                "/ip/dhcp-server/set",
                &format!("=.id={id}"),
                &format!("=disabled={flag}"),
            ],
        )?;
        expect_done(&mut stream).with_context(|| format!("updating dhcp server {name}"))?;
        changed.push(format!("{name} disabled={flag}"));
    }
    Ok(changed)
}

fn connect(host: &str) -> Result<TcpStream> {
    let addr = format!("{host}:8728");
    let mut addrs = addr
        .to_socket_addrs()
        .with_context(|| format!("resolving {addr}"))?;
    let sockaddr = addrs
        .next()
        .ok_or_else(|| anyhow!("no address for {addr}"))?;
    let stream = TcpStream::connect_timeout(&sockaddr, Duration::from_secs(8))
        .with_context(|| format!("connecting to {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;
    Ok(stream)
}

fn login(stream: &mut TcpStream, user: &str, password: &str) -> Result<()> {
    sentence(
        stream,
        &[
            "/login",
            &format!("=name={user}"),
            &format!("=password={password}"),
        ],
    )?;
    let reply = read_sentence(stream)?;
    if reply.first().map(String::as_str) != Some("!done") {
        return Err(anyhow!("routeros login failed"));
    }
    Ok(())
}

fn print_cmd(stream: &mut TcpStream, command: &str) -> Result<Vec<Value>> {
    sentence(stream, &[command])?;
    let mut rows = Vec::new();
    loop {
        let sentence = read_sentence(stream)?;
        match sentence.first().map(String::as_str) {
            Some("!re") => {
                let mut map = serde_json::Map::new();
                for word in sentence.iter().skip(1) {
                    if let Some(rest) = word.strip_prefix('=') {
                        let (key, value) = rest.split_once('=').unwrap_or((rest, ""));
                        map.insert(key.to_string(), Value::String(value.to_string()));
                    }
                }
                rows.push(Value::Object(map));
            }
            Some("!done") => return Ok(rows),
            Some("!trap") | Some("!fatal") => {
                return Err(anyhow!("routeros {command} failed: {}", sentence.join(" ")));
            }
            _ => {}
        }
    }
}

fn expect_done(stream: &mut TcpStream) -> Result<()> {
    let sentence = read_sentence(stream)?;
    if sentence.first().map(String::as_str) == Some("!done") {
        Ok(())
    } else {
        Err(anyhow!("routeros command failed: {}", sentence.join(" ")))
    }
}

fn sentence(stream: &mut TcpStream, words: &[&str]) -> Result<()> {
    for word in words {
        stream.write_all(&encode_word(word.as_bytes()))?;
    }
    stream.write_all(&[0])?;
    stream.flush()?;
    Ok(())
}

fn encode_word(word: &[u8]) -> Vec<u8> {
    let len = word.len();
    let mut out = Vec::new();
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x4000 {
        let n = len | 0x8000;
        out.push((n >> 8) as u8);
        out.push(n as u8);
    } else if len < 0x20_0000 {
        let n = len | 0xC0_0000;
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
    } else {
        let n = len | 0xE000_0000;
        out.push((n >> 24) as u8);
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
    }
    out.extend_from_slice(word);
    out
}

fn read_sentence(stream: &mut TcpStream) -> Result<Vec<String>> {
    let mut words = Vec::new();
    loop {
        let word = read_word(stream)?;
        if word.is_empty() {
            break;
        }
        words.push(String::from_utf8_lossy(&word).into_owned());
    }
    Ok(words)
}

fn read_word(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut first = [0u8; 1];
    stream.read_exact(&mut first)?;
    let c = first[0];
    let len = if c & 0x80 == 0 {
        c as usize
    } else if c & 0xC0 == 0x80 {
        let mut rest = [0u8; 1];
        stream.read_exact(&mut rest)?;
        (((c & !0xC0) as usize) << 8) | rest[0] as usize
    } else if c & 0xE0 == 0xC0 {
        let mut rest = [0u8; 2];
        stream.read_exact(&mut rest)?;
        (((c & !0xE0) as usize) << 16) | ((rest[0] as usize) << 8) | rest[1] as usize
    } else if c & 0xF0 == 0xE0 {
        let mut rest = [0u8; 3];
        stream.read_exact(&mut rest)?;
        (((c & !0xF0) as usize) << 24)
            | ((rest[0] as usize) << 16)
            | ((rest[1] as usize) << 8)
            | rest[2] as usize
    } else if c == 0xF0 {
        let mut rest = [0u8; 4];
        stream.read_exact(&mut rest)?;
        u32::from_be_bytes(rest) as usize
    } else {
        return Err(anyhow!("routeros length byte {:02x}", c));
    };
    let mut data = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut data)?;
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Catalog;

    fn base() -> Catalog {
        Catalog {
            upstream: vec!["1.1.1.1".into(), "8.8.8.8".into()],
            authoritative: "127.0.0.1:5300".into(),
            networks: vec![Network {
                name: "lan".into(),
                interface: "lan10g".into(),
                cidr: "192.168.40.0/24".into(),
                gateway: "192.168.40.1".into(),
                domain: "home.arpa".into(),
                dns_servers: vec!["192.168.40.10".into()],
                pools: vec![],
                dhcp_enabled: true,
                valid_lifetime: 86400,
                next_server: String::new(),
                boot_file_name: String::new(),
            }],
            reservations: vec![],
            records: vec![],
        }
    }

    const DUMP: &str = r#"{
      "/ip/dhcp-server/print": [{
        "name": "dhcp1", "interface": "bridge1", "lease-time": "30m",
        "address-pool": "dhcp_pool0", "disabled": "false"
      }],
      "/ip/pool/print": [{"name": "dhcp_pool0", "ranges": "192.168.40.100-192.168.40.200"}],
      "/ip/dhcp-server/network/print": [
        {"address": "192.168.40.0/24", "gateway": "192.168.40.1", "dns-server": "1.1.1.1,8.8.8.8",
         "next-server": "192.168.40.10", "boot-file-name": "http://192.168.40.10/boot", "comment": "server"},
        {"address": "192.168.50.0/24", "gateway": "192.168.50.1", "dns-server": "1.1.1.1", "comment": "garage"},
        {"address": "192.168.20.0/24", "gateway": "192.168.20.1", "comment": "office"}
      ],
      "/ip/dhcp-server/lease/print": [
        {"address": "192.168.40.105", "mac-address": "00:00:5E:00:53:11", "host-name": "node-105",
         "dynamic": "false", "disabled": "false", "comment": "node-105"},
        {"address": "192.168.40.106", "mac-address": "00:00:5E:00:53:12", "host-name": "kvm-node",
         "dynamic": "false", "disabled": "false", "comment": "node-106 extra"},
        {"address": "192.168.40.107", "mac-address": "00:00:5E:00:53:13", "host-name": "kvm-node",
         "dynamic": "true", "disabled": "false", "comment": "node-107"},
        {"address": "19.168.40.201", "mac-address": "00:00:5E:00:53:14", "dynamic": "false",
         "disabled": "false", "comment": "bad address"}
      ]
    }"#;

    #[test]
    fn imports_active_subnet_and_keeps_our_dns() {
        let mut catalog = base();
        let report = merge_routeros(&mut catalog, DUMP, "lan").unwrap();
        assert_eq!(report.reservations, 3);
        assert_eq!(report.skipped, 1);
        assert!(report.warnings.iter().any(|w| w.contains("19.168.40.201")));
        assert!(report.warnings.iter().any(|w| w.contains("192.168.40.10")));
        let lan = catalog.network("lan").unwrap();
        assert_eq!(lan.pools[0].start, "192.168.40.100");
        assert_eq!(lan.valid_lifetime, 1800);
        assert_eq!(lan.next_server, "192.168.40.10");
        assert_eq!(lan.dns_servers, vec!["192.168.40.10".to_string()]);
        assert_eq!(lan.interface, "lan10g");
        assert!(catalog.network("garage").is_some());
        assert!(!catalog.network("garage").unwrap().dhcp_enabled);
        assert!(catalog.network("office").is_some());
        let names: Vec<_> = catalog
            .reservations
            .iter()
            .map(|r| r.hostname.as_str())
            .collect();
        assert!(names.contains(&"node-105"));
        assert!(names.contains(&"kvm-node"));
        assert!(names.contains(&"node-107"));
    }

    #[test]
    fn export_text_parses() {
        let export = r#"
/ip pool
add name=dhcp_pool0 ranges=192.168.40.100-192.168.40.200
/ip dhcp-server
add address-pool=dhcp_pool0 disabled=no interface=bridge1 lease-time=30m name=dhcp1
/ip dhcp-server network
add address=192.168.40.0/24 dns-server=1.1.1.1,8.8.8.8 gateway=192.168.40.1
/ip dhcp-server lease
add address=192.168.40.105 comment=node-105 mac-address=00:00:5E:00:53:11 server=dhcp1
"#;
        let mut catalog = base();
        let report = merge_routeros(&mut catalog, export, "lan").unwrap();
        assert_eq!(report.reservations, 1);
        assert_eq!(catalog.reservations[0].hostname, "node-105");
        assert_eq!(catalog.network("lan").unwrap().valid_lifetime, 1800);
    }
}
