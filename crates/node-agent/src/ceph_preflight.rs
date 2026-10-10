//! Pure helpers for CephCluster preflight checks (unit-tested without root).

/// Hardware RAID / virtual-disk model strings that must not be used as OSDs.
pub fn is_hardware_raid_model(model: &str) -> bool {
    let u = model.to_ascii_uppercase();
    u.contains("PERC")
        || u.contains("VIRTUAL DISK")
        || u.contains("MEGARAID")
        || u.contains("DELLBOSS")
        || u.contains("MR9361")
        || u.contains("HBA330") && u.contains("VIRTUAL")
}

/// Prefer stable `/dev/disk/by-id/...` paths for OSD devices.
pub fn prefers_by_id_path(path: &str) -> bool {
    path.starts_with("/dev/disk/by-id/")
}

/// True when `ip -j addr` JSON (array of links) shows `ip` on `iface`.
pub fn iface_has_address(ip_json: &str, iface: &str, ip: &str) -> Result<bool, String> {
    let links: Vec<serde_json::Value> =
        serde_json::from_str(ip_json).map_err(|e| format!("parse ip -j addr: {e}"))?;
    let want = ip.trim();
    if want.is_empty() {
        return Ok(false);
    }
    for link in links {
        let name = link
            .get("ifname")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if name != iface {
            continue;
        }
        let addrs = link
            .get("addr_info")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for a in addrs {
            if a.get("local").and_then(|v| v.as_str()) == Some(want) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Heuristic: systemd-networkd DHCP lease files or networkctl JSON mentioning DHCP.
pub fn iface_appears_dhcp(networkctl_status: &str, lease_file_exists: bool) -> bool {
    if lease_file_exists {
        return true;
    }
    let lower = networkctl_status.to_ascii_lowercase();
    lower.contains("dhcp4")
        || lower.contains("dhcpv4")
        || lower.contains("dhcp: yes")
        || lower.contains("network file") && lower.contains("dhcp")
}

/// Strip optional `:port` from a mon address.
pub fn mon_ip(mon_addr: &str) -> &str {
    mon_addr.split(':').next().unwrap_or(mon_addr).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_perc_and_megaraid() {
        assert!(is_hardware_raid_model("PERC H730 Mini"));
        assert!(is_hardware_raid_model("Dell PERC HBA330"));
        assert!(is_hardware_raid_model("LSI MegaRAID SAS"));
        assert!(is_hardware_raid_model("Virtual Disk"));
        assert!(!is_hardware_raid_model("Micron_1300_SATA_512GB"));
        assert!(!is_hardware_raid_model("INTEL SSDSC2BB300G4"));
    }

    #[test]
    fn by_id_preference() {
        assert!(prefers_by_id_path(
            "/dev/disk/by-id/ata-Micron_1300_SATA_512GB_2144329E98F2"
        ));
        assert!(!prefers_by_id_path("/dev/sda"));
    }

    #[test]
    fn finds_address_on_iface() {
        let json = r#"[
          {"ifname":"enp6s0","addr_info":[{"family":"inet","local":"192.168.40.103"}]},
          {"ifname":"enp6s0d1","addr_info":[{"family":"inet","local":"192.168.47.2"}]}
        ]"#;
        assert!(iface_has_address(json, "enp6s0", "192.168.40.103").unwrap());
        assert!(iface_has_address(json, "enp6s0d1", "192.168.47.2").unwrap());
        assert!(!iface_has_address(json, "enp6s0", "192.168.47.2").unwrap());
    }

    #[test]
    fn dhcp_heuristic() {
        assert!(iface_appears_dhcp(
            "DHCP4: yes\nNetwork File: 10-eth.network",
            false
        ));
        assert!(iface_appears_dhcp("", true));
        assert!(!iface_appears_dhcp(
            "Address: 10.0.0.1\nNetwork File: 10-static.network",
            false
        ));
    }

    #[test]
    fn mon_ip_strips_port() {
        assert_eq!(mon_ip("192.168.40.103:6789"), "192.168.40.103");
        assert_eq!(mon_ip("192.168.40.103"), "192.168.40.103");
    }
}
