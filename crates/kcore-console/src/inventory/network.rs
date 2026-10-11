//! Network interface inventory (Linux, `ip -j` with sysfs fallbacks).

use serde::Deserialize;

use super::route::read_default_ifname;

fn dash(s: &str) -> String {
    if s.is_empty() {
        "—".to_string()
    } else {
        s.to_string()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Nic {
    pub name: String,
    pub mac: String,
    pub oper_state: String,
    /// IPv4 address with prefix, e.g. `192.168.40.143/24`, or `—`.
    pub ipv4: String,
    /// All IPv6 addresses (global first), comma-separated, or `—`.
    pub ipv6: String,
    pub mtu: String,
    pub speed: String,
    pub driver: String,
    pub default_route: bool,
    pub management: bool,
    /// Bridge (or bond) this interface is enslaved to, if any.
    pub master: String,
}

impl Nic {
    /// First IPv6 for table cells: prefer a global address over link-local.
    /// Returns `(display, extra_count)` where `extra_count` is addresses beyond the first.
    pub fn primary_ipv6_display(&self) -> (String, usize) {
        primary_ipv6_display(&self.ipv6)
    }

    /// Iface label with optional ` -> master` suffix when enslaved.
    pub fn iface_label(&self, with_master_suffix: bool) -> String {
        if with_master_suffix && !self.master.is_empty() && self.master != "—" {
            format!("{} -> {}", self.name, self.master)
        } else {
            self.name.clone()
        }
    }
}

/// Split a stored ipv6 cell into a primary display and a count of extras.
pub fn primary_ipv6_display(ipv6: &str) -> (String, usize) {
    if ipv6.is_empty() || ipv6 == "—" {
        return ("—".into(), 0);
    }
    let parts: Vec<&str> = ipv6
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return ("—".into(), 0);
    }
    let primary = parts
        .iter()
        .find(|a| !a.starts_with("fe80:"))
        .copied()
        .unwrap_or(parts[0]);
    let extra = parts.len().saturating_sub(1);
    (primary.to_string(), extra)
}

#[derive(Debug, Deserialize)]
struct IpEntry {
    ifname: String,
    addr_info: Option<Vec<AddrInfo>>,
}

#[derive(Debug, Deserialize)]
struct AddrInfo {
    family: String,
    local: String,
    scope: Option<String>,
    #[serde(default)]
    prefixlen: Option<u32>,
}

pub fn list_nics() -> Vec<Nic> {
    let out = std::process::Command::new("ip")
        .args(["-j", "link", "show"])
        .output();
    let links: Vec<serde_json::Value> = out
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or_default();

    let out_a = std::process::Command::new("ip")
        .args(["-j", "address", "show"])
        .output();
    let addr_json: Option<Vec<IpEntry>> = out_a
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice(&o.stdout).ok());

    let default_if = read_default_ifname();
    let mgmt_if = default_if.clone().or_else(|| {
        std::env::var("KCORE_MGMT_IFACE")
            .ok()
            .or_else(|| std::env::var("KCORE_MANAGEMENT_IF").ok())
    });

    let mut rows = build_nics_from_links(
        &links,
        addr_json.as_deref(),
        default_if.as_deref(),
        mgmt_if.as_deref(),
    );
    if rows.is_empty() {
        rows.extend(sysfs_enum_fallback());
    }
    rows
}

fn build_nics_from_links(
    links: &[serde_json::Value],
    addr_json: Option<&[IpEntry]>,
    default_if: Option<&str>,
    mgmt_if: Option<&str>,
) -> Vec<Nic> {
    let mut rows = Vec::new();
    for link in links {
        let ifname = link
            .get("ifname")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if ifname.is_empty() || ifname == "lo" {
            continue;
        }
        let mtu = link
            .get("mtu")
            .and_then(|v| v.as_u64())
            .map(|m| m.to_string())
            .unwrap_or_else(|| "—".to_string());
        let state = link
            .get("operstate")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let mac: String = link
            .get("address")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| read_sysfs_mac(&ifname))
            .unwrap_or_else(|| "—".to_string());
        let master = link
            .get("master")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| read_sysfs_master(&ifname))
            .unwrap_or_default();
        let (v4, v6) = extract_addrs(&ifname, addr_json);
        let speed = read_link_speed(&ifname);
        let driver = read_driver(&ifname);
        let default_route = default_if.map(|d| d == ifname).unwrap_or(false);
        let management = mgmt_if
            .map(|m| m == ifname)
            .unwrap_or_else(|| default_route);

        rows.push(Nic {
            name: ifname,
            mac: if mac.is_empty() {
                "—".to_string()
            } else {
                mac
            },
            oper_state: dash(state),
            ipv4: v4,
            ipv6: v6,
            mtu: dash(&mtu),
            speed: dash(&speed),
            driver: dash(&driver),
            default_route,
            management,
            master: if master.is_empty() {
                String::new()
            } else {
                master
            },
        });
    }
    rows
}

fn extract_addrs(ifname: &str, entries: Option<&[IpEntry]>) -> (String, String) {
    let mut v4 = "—".to_string();
    let mut v6_global = Vec::new();
    let mut v6_link = Vec::new();
    let Some(entries) = entries else {
        return (v4, "—".to_string());
    };
    for e in entries {
        if e.ifname != ifname {
            continue;
        }
        for a in e.addr_info.as_deref().unwrap_or(&[]) {
            let with_prefix = |local: &str, pref: Option<u32>| -> String {
                match pref {
                    Some(p) => format!("{local}/{p}"),
                    None => local.to_string(),
                }
            };
            if a.family == "inet" {
                if a.scope.as_deref() == Some("global") || v4 == "—" {
                    v4 = with_prefix(&a.local, a.prefixlen);
                }
            } else if a.family == "inet6" {
                if a.local == "::1" {
                    continue;
                }
                let s = with_prefix(&a.local, a.prefixlen);
                if a.scope.as_deref() == Some("link") || a.local.starts_with("fe80:") {
                    v6_link.push(s);
                } else {
                    v6_global.push(s);
                }
            }
        }
    }
    let mut v6_all = v6_global;
    v6_all.extend(v6_link);
    let v6 = if v6_all.is_empty() {
        "—".to_string()
    } else {
        v6_all.join(", ")
    };
    (v4, v6)
}

fn read_link_speed(ifname: &str) -> String {
    let p = format!("/sys/class/net/{ifname}/speed");
    match std::fs::read_to_string(&p) {
        Ok(s) => {
            let t = s.trim();
            if t == "-1" {
                "—".to_string()
            } else {
                format!("{t} Mbps")
            }
        }
        Err(_) => "—".to_string(),
    }
}

fn read_driver(ifname: &str) -> String {
    let p = format!("/sys/class/net/{ifname}/device/uevent");
    if let Ok(s) = std::fs::read_to_string(p) {
        for l in s.lines() {
            if let Some(d) = l.strip_prefix("DRIVER=") {
                return d.to_string();
            }
        }
    }
    "—".to_string()
}

fn read_sysfs_mac(ifname: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/class/net/{ifname}/address"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_sysfs_master(ifname: &str) -> Option<String> {
    let path = format!("/sys/class/net/{ifname}/master");
    let link = std::fs::read_link(&path).ok()?;
    link.file_name().and_then(|s| s.to_str()).map(String::from)
}

/// Very small fallback: enumerate /sys/class/net
fn sysfs_enum_fallback() -> Vec<Nic> {
    let mut n = Vec::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/net") else {
        return n;
    };
    for e in dir.flatten() {
        let ifname = e.file_name().to_string_lossy().to_string();
        if ifname == "lo" {
            continue;
        }
        let master = read_sysfs_master(&ifname).unwrap_or_default();
        n.push(Nic {
            name: ifname,
            master,
            ..Default::default()
        });
    }
    n
}

#[cfg(test)]
mod tests {
    use super::{build_nics_from_links, primary_ipv6_display, IpEntry};

    #[test]
    fn parses_link_and_address_json_with_default_and_management_markers() {
        let links: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
              {"ifname":"lo","mtu":65536,"operstate":"UNKNOWN","address":"00:00:00:00:00:00"},
              {"ifname":"eth0","mtu":1500,"operstate":"UP","address":"aa:bb:cc:dd:ee:ff"},
              {"ifname":"eth1","mtu":9000,"operstate":"DOWN","address":"11:22:33:44:55:66","master":"br0"}
            ]"#,
        )
        .unwrap();
        let addrs: Vec<IpEntry> = serde_json::from_str(
            r#"[
              {"ifname":"eth0","addr_info":[
                {"family":"inet","local":"10.190.15.20","scope":"global","prefixlen":24},
                {"family":"inet6","local":"fe80::1","scope":"link","prefixlen":64},
                {"family":"inet6","local":"fd00::20","scope":"global","prefixlen":64}
              ]},
              {"ifname":"eth1","addr_info":[]}
            ]"#,
        )
        .unwrap();

        let nics = build_nics_from_links(&links, Some(&addrs), Some("eth0"), Some("eth0"));
        assert_eq!(nics.len(), 2);
        assert_eq!(nics[0].name, "eth0");
        assert_eq!(nics[0].ipv4, "10.190.15.20/24");
        assert!(nics[0].ipv6.starts_with("fd00::20/64"));
        assert!(nics[0].ipv6.contains("fe80::1/64"));
        assert!(nics[0].default_route);
        assert!(nics[0].management);
        assert_eq!(nics[1].ipv4, "—");
        assert_eq!(nics[1].master, "br0");
        assert_eq!(nics[1].iface_label(true), "eth1 -> br0");
    }

    #[test]
    fn primary_ipv6_prefers_global_and_counts_extras() {
        let (p, n) = primary_ipv6_display("2a02:ef8:1000:1d00:7d1d:ca86:5156:8dd8/64, fe80::1/64");
        assert_eq!(p, "2a02:ef8:1000:1d00:7d1d:ca86:5156:8dd8/64");
        assert_eq!(n, 1);
        let (p2, n2) = primary_ipv6_display("fe80::1/64");
        assert_eq!(p2, "fe80::1/64");
        assert_eq!(n2, 0);
    }
}
