//! Render Kea, PowerDNS, and the recursor from a catalog.

use crate::model::{fqdn, parse_cidr, Catalog, DnsRecord, Ipv4Net, Network};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZonePlan {
    pub name: String,
    pub nameserver: String,
    pub rrsets: Vec<Rrset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rrset {
    pub name: String,
    pub rrtype: String,
    pub ttl: u32,
    pub content: String,
}

pub fn kea_dhcp4(catalog: &Catalog, lease_file: &str, socket: &str) -> Value {
    let interfaces: Vec<&str> = catalog
        .enabled_networks()
        .map(|n| n.interface.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let subnets: Vec<Value> = catalog
        .enabled_networks()
        .enumerate()
        .map(|(index, network)| subnet(catalog, network, (index + 1) as i64))
        .collect();
    json!({
        "Dhcp4": {
            "interfaces-config": {
                "interfaces": interfaces,
                "dhcp-socket-type": "raw"
            },
            "control-socket": {
                "socket-type": "unix",
                "socket-name": socket
            },
            "lease-database": {
                "type": "memfile",
                "persist": true,
                "name": lease_file
            },
            "valid-lifetime": 1800,
            "renew-timer": 900,
            "rebind-timer": 1575,
            "authoritative": true,
            "reservations-global": false,
            "reservations-in-subnet": true,
            "reservations-out-of-pool": false,
            "subnet4": subnets,
            "loggers": [{
                "name": "kea-dhcp4",
                "output-options": [{"output": "stdout"}],
                "severity": "INFO"
            }]
        }
    })
}

fn subnet(catalog: &Catalog, network: &Network, id: i64) -> Value {
    let lifetime = network.valid_lifetime;
    let renew = lifetime / 2;
    let rebind = lifetime.saturating_mul(7) / 8;
    let mut options = vec![
        json!({"name": "routers", "data": network.gateway}),
        json!({"name": "domain-name-servers", "data": network.dns_servers.join(", ")}),
        json!({"name": "domain-name", "data": network.domain}),
    ];
    if !network.boot_file_name.is_empty() {
        options.push(json!({"name": "boot-file-name", "data": network.boot_file_name}));
    }
    let reservations: Vec<Value> = catalog
        .reservations
        .iter()
        .filter(|r| r.network == network.name)
        .map(|r| {
            json!({
                "hw-address": r.mac_address,
                "ip-address": r.ip_address,
                "hostname": r.hostname
            })
        })
        .collect();
    let mut body = json!({
        "id": id,
        "subnet": parse_cidr(&network.cidr).map(|n| n.canonical()).unwrap_or_else(|_| network.cidr.clone()),
        "interface": network.interface,
        "valid-lifetime": lifetime,
        "renew-timer": renew,
        "rebind-timer": rebind,
        "pools": network.pools.iter().map(|p| json!({"pool": format!("{} - {}", p.start, p.end)})).collect::<Vec<_>>(),
        "option-data": options,
        "reservations": reservations
    });
    if !network.next_server.is_empty() {
        body["next-server"] = json!(network.next_server);
    }
    if !network.boot_file_name.is_empty() {
        body["boot-file-name"] = json!(network.boot_file_name);
    }
    body
}

pub fn zone_plan(catalog: &Catalog) -> Vec<ZonePlan> {
    let primary = catalog
        .networks
        .iter()
        .find(|n| n.dhcp_enabled)
        .or_else(|| catalog.networks.first());
    let primary_domain = primary.map(|n| n.domain.as_str()).unwrap_or("home.arpa");
    let nameserver = format!("ns1.{primary_domain}.");
    let mut zones: BTreeMap<String, Vec<Rrset>> = BTreeMap::new();
    if let Some(network) = primary {
        zones.entry(fqdn(&network.domain)).or_default().push(Rrset {
            name: nameserver.clone(),
            rrtype: "A".into(),
            ttl: 3600,
            content: network
                .dns_servers
                .first()
                .cloned()
                .unwrap_or_else(|| "127.0.0.1".into()),
        });
    }
    for network in &catalog.networks {
        zones.entry(fqdn(&network.domain)).or_default();
        for zone in network
            .parsed()
            .map(|n| n.reverse_zones())
            .unwrap_or_default()
        {
            zones.entry(fqdn(&zone)).or_default();
        }
    }
    for reservation in &catalog.reservations {
        let Some(network) = catalog.network(&reservation.network) else {
            continue;
        };
        if reservation.hostname.is_empty() {
            continue;
        }
        let name = fqdn(&format!("{}.{}", reservation.hostname, network.domain));
        push_unique(
            zones.entry(fqdn(&network.domain)).or_default(),
            Rrset {
                name: name.clone(),
                rrtype: "A".into(),
                ttl: 3600,
                content: reservation.ip_address.clone(),
            },
        );
        if let Ok(zone) = reverse_for(&reservation.ip_address) {
            push_unique(
                zones.entry(fqdn(&zone)).or_default(),
                Rrset {
                    name: ptr_name(&reservation.ip_address),
                    rrtype: "PTR".into(),
                    ttl: 3600,
                    content: name,
                },
            );
        }
    }
    for record in &catalog.records {
        let zone = fqdn(&record.zone);
        push_unique(zones.entry(zone).or_default(), rrset_from(record));
    }
    zones
        .into_iter()
        .map(|(name, rrsets)| ZonePlan {
            name,
            nameserver: nameserver.clone(),
            rrsets,
        })
        .collect()
}

fn rrset_from(record: &DnsRecord) -> Rrset {
    let mut content = record.content.clone();
    if matches!(
        record.rrtype.as_str(),
        "CNAME" | "NS" | "PTR" | "MX" | "SRV"
    ) && !content.ends_with('.')
        && content.contains('.')
    {
        content.push('.');
    }
    Rrset {
        name: fqdn(&record.name),
        rrtype: record.rrtype.to_ascii_uppercase(),
        ttl: record.ttl,
        content,
    }
}

fn push_unique(rrsets: &mut Vec<Rrset>, rrset: Rrset) {
    if !rrsets
        .iter()
        .any(|have| have.name == rrset.name && have.rrtype == rrset.rrtype)
    {
        rrsets.push(rrset);
    }
}

fn reverse_for(ip: &str) -> Result<String, String> {
    let addr = crate::model::parse_ipv4(ip)?;
    Ok(Ipv4Net {
        addr: u32::from(addr),
        prefix: 24,
    }
    .reverse_zones()
    .pop()
    .unwrap_or_default())
}

fn ptr_name(ip: &str) -> String {
    let addr = ip
        .parse::<Ipv4Addr>()
        .map(|ip| ip.octets())
        .unwrap_or([0, 0, 0, 0]);
    fqdn(&format!(
        "{}.{}.{}.{}.in-addr.arpa",
        addr[3], addr[2], addr[1], addr[0]
    ))
}

pub fn forward_zones(catalog: &Catalog) -> String {
    let (host, port) =
        crate::model::parse_host_port(&catalog.authoritative).unwrap_or(("127.0.0.1".into(), 5300));
    let mut lines = vec!["# generated by kcore-provisioning".to_string()];
    for zone in zone_plan(catalog) {
        lines.push(format!("{}={host}:{port}", zone.name.trim_end_matches('.')));
    }
    let upstream = catalog.upstream.join(";");
    lines.push(format!("+.={upstream}"));
    lines.push(String::new());
    lines.join("\n")
}

pub fn recursor_lua(catalog: &Catalog) -> String {
    let mut lines = vec!["-- generated by kcore-provisioning".to_string()];
    let mut ntas = vec![
        "home.arpa.".to_string(),
        "10.in-addr.arpa.".to_string(),
        "168.192.in-addr.arpa.".to_string(),
    ];
    for octet in 16..=31 {
        ntas.push(format!("{octet}.172.in-addr.arpa."));
    }
    for zone in zone_plan(catalog) {
        if !ntas.iter().any(|have| have == &zone.name) {
            ntas.push(zone.name);
        }
    }
    for nta in ntas {
        lines.push(format!("addNTA({nta:?}, \"kcore local zone\")"));
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn recursor_yml(catalog: &Catalog, state_dir: &str) -> String {
    let mut listen = vec!["127.0.0.1".to_string()];
    for network in &catalog.networks {
        for server in &network.dns_servers {
            if !listen.contains(server) {
                listen.push(server.clone());
            }
        }
    }
    let mut allow = vec![
        "127.0.0.0/8".to_string(),
        "10.0.0.0/8".to_string(),
        "172.16.0.0/12".to_string(),
        "192.168.0.0/16".to_string(),
        "100.64.0.0/10".to_string(),
    ];
    for network in &catalog.networks {
        if let Ok(net) = parse_cidr(&network.cidr) {
            let cidr = net.canonical();
            if !allow.contains(&cidr) {
                allow.push(cidr);
            }
        }
    }
    let mut yaml = String::from("incoming:\n  listen:\n");
    for address in listen {
        yaml.push_str(&format!("    - {address}\n"));
    }
    yaml.push_str("  port: 53\n  allow_from:\n");
    for cidr in allow {
        yaml.push_str(&format!("    - {cidr}\n"));
    }
    yaml.push_str(&format!(
        "\
recursor:
  forward_zones_file: {state_dir}/forward.zones
  lua_config_file: {state_dir}/recursor.lua
  socket_dir: /run/pdns-recursor
  daemon: false
  write_pid: false
  threads: 2
  serve_rfc1918: false
dnssec:
  validation: validate
logging:
  timestamp: false
  disable_syslog: true
"
    ));
    yaml
}

pub fn pdns_conf(api_key: &str, database: &str) -> String {
    format!(
        "\
launch=gsqlite3
gsqlite3-database={database}
local-address=127.0.0.1
local-port=5300
api=yes
api-key={api_key}
webserver=yes
webserver-address=127.0.0.1
webserver-port=8082
webserver-allow-from=127.0.0.1,::1
default-ttl=3600
primary=no
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample_catalog;

    #[test]
    fn kea_has_both_subnets_and_boot_options() {
        let catalog = sample_catalog();
        let value = kea_dhcp4(&catalog, "/var/lib/kea/dhcp4.leases", "/run/kea/kea4.sock");
        let subnets = value["Dhcp4"]["subnet4"].as_array().unwrap();
        assert_eq!(subnets.len(), 2);
        assert_eq!(subnets[0]["interface"], "lan10g");
        assert_eq!(subnets[1]["subnet"], "192.168.50.0/24");
        assert_eq!(subnets[0]["next-server"], "192.168.40.10");
        assert_eq!(subnets[0]["reservations"][0]["hostname"], "node-105");
        assert_eq!(value["Dhcp4"]["authoritative"], true);
        assert_eq!(value["Dhcp4"]["reservations-in-subnet"], true);
        assert_eq!(value["Dhcp4"]["reservations-out-of-pool"], false);
        let ifaces = value["Dhcp4"]["interfaces-config"]["interfaces"]
            .as_array()
            .unwrap();
        assert_eq!(ifaces.len(), 2);
    }

    #[test]
    fn dns_plan_has_forward_and_reverse() {
        let catalog = sample_catalog();
        let zones = zone_plan(&catalog);
        let home = zones.iter().find(|z| z.name == "home.arpa.").unwrap();
        assert!(home
            .rrsets
            .iter()
            .any(|r| r.name == "node-105.home.arpa." && r.content == "192.168.40.105"));
        let rev = zones
            .iter()
            .find(|z| z.name == "40.168.192.in-addr.arpa.")
            .unwrap();
        assert!(rev.rrsets.iter().any(|r| r.rrtype == "PTR"));
        let forward = forward_zones(&catalog);
        assert!(forward.contains("home.arpa=127.0.0.1:5300"));
        assert!(forward.contains("garage.home.arpa=127.0.0.1:5300"));
        assert!(forward.contains("40.168.192.in-addr.arpa=127.0.0.1:5300"));
        assert!(forward.contains("+.=1.1.1.1;8.8.8.8"));
        let lua = recursor_lua(&catalog);
        assert!(lua.contains("addNTA(\"home.arpa.\""));
        assert!(lua.contains("garage.home.arpa."));
    }

    #[test]
    fn disabled_network_is_not_a_kea_subnet() {
        let mut catalog = sample_catalog();
        catalog.networks[1].dhcp_enabled = false;
        let value = kea_dhcp4(&catalog, "/tmp/leases", "/tmp/kea.sock");
        assert_eq!(value["Dhcp4"]["subnet4"].as_array().unwrap().len(), 1);
        assert!(zone_plan(&catalog)
            .iter()
            .any(|z| z.name == "garage.home.arpa."));
    }
}
