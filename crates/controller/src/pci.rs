//! PCI bus/device/function addresses for VFIO passthrough.
//!
//! Stored on a VM row as a comma-separated list of canonical addresses
//! (`0000:03:00.0`). The node Nix module binds each one to `vfio-pci` and
//! passes it to cloud-hypervisor.

/// Accept `0000:03:00.0` or `03:00.0` and return the canonical lowercase form.
pub fn normalize_pci_bdf(raw: &str) -> Result<String, String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("PCI address is empty".into());
    }
    let (domain, rest) = match s.split_once(':') {
        Some((domain, rest)) if rest.contains(':') => (domain, rest),
        Some(_) => ("0000", s.as_str()),
        None => {
            return Err(format!("PCI address '{raw}' must look like 0000:03:00.0"));
        }
    };
    let Some((bus, devfn)) = rest.split_once(':') else {
        return Err(format!("PCI address '{raw}' must look like 0000:03:00.0"));
    };
    let Some((dev, func)) = devfn.split_once('.') else {
        return Err(format!("PCI address '{raw}' must look like 0000:03:00.0"));
    };
    if !is_hex(domain, 4) || !is_hex(bus, 2) || !is_hex(dev, 2) || !is_func(func) {
        return Err(format!("PCI address '{raw}' must look like 0000:03:00.0"));
    }
    Ok(format!("{domain}:{bus}:{dev}.{func}"))
}

/// Canonical comma-separated form. An empty list is an empty string.
pub fn join_pci_devices(addrs: &[String]) -> Result<String, String> {
    if addrs.is_empty() {
        return Ok(String::new());
    }
    let mut out = Vec::with_capacity(addrs.len());
    for raw in addrs {
        let bdf = normalize_pci_bdf(raw)?;
        if out.iter().any(|existing| existing == &bdf) {
            return Err(format!("duplicate PCI address {bdf}"));
        }
        out.push(bdf);
    }
    Ok(out.join(","))
}

pub fn split_pci_devices(stored: &str) -> Vec<String> {
    stored
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// First address in `incoming` that another VM on the same node already owns.
///
/// `incoming` is the canonical comma-separated list. Each assigned entry is
/// `(vm name, stored pci_devices)`.
pub fn first_overlap<'a>(
    incoming: &str,
    assigned: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<(String, String)> {
    let wanted = split_pci_devices(incoming);
    if wanted.is_empty() {
        return None;
    }
    for (vm_name, stored) in assigned {
        for addr in split_pci_devices(stored) {
            if wanted.iter().any(|candidate| candidate == &addr) {
                return Some((addr, vm_name.to_string()));
            }
        }
    }
    None
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_func(s: &str) -> bool {
    s.len() == 1 && matches!(s.as_bytes()[0], b'0'..=b'7')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_full_and_short_forms() {
        assert_eq!(normalize_pci_bdf("0000:03:00.0").unwrap(), "0000:03:00.0");
        assert_eq!(normalize_pci_bdf("03:00.1").unwrap(), "0000:03:00.1");
        assert_eq!(
            normalize_pci_bdf("  0000:0D:00.0 ").unwrap(),
            "0000:0d:00.0"
        );
    }

    #[test]
    fn rejects_malformed_addresses() {
        for raw in [
            "",
            "gpu0",
            "0000:03:00",
            "0000:03:00.8",
            "../vfio",
            "0000:zz:00.0",
        ] {
            assert!(normalize_pci_bdf(raw).is_err(), "{raw} should be rejected");
        }
    }

    #[test]
    fn joins_without_duplicates() {
        assert_eq!(join_pci_devices(&[]).unwrap(), "");
        assert_eq!(
            join_pci_devices(&["03:00.0".into(), "0000:03:00.1".into(),]).unwrap(),
            "0000:03:00.0,0000:03:00.1"
        );
        assert!(join_pci_devices(&["03:00.0".into(), "0000:03:00.0".into()]).is_err());
    }

    #[test]
    fn split_round_trips() {
        let joined = join_pci_devices(&["0000:01:00.0".into(), "0000:01:00.1".into()]).unwrap();
        assert_eq!(
            split_pci_devices(&joined),
            vec!["0000:01:00.0".to_string(), "0000:01:00.1".to_string()]
        );
        assert!(split_pci_devices("").is_empty());
    }

    #[test]
    fn overlap_finds_a_shared_address() {
        assert!(first_overlap("", [("other", "0000:03:00.0")]).is_none());
        assert!(first_overlap("0000:04:00.0", [("other", "0000:03:00.0")]).is_none());
        let hit = first_overlap(
            "0000:03:00.1,0000:03:00.0",
            [
                ("gpu-a", "0000:01:00.0"),
                ("gpu-b", "0000:03:00.0,0000:03:00.1"),
            ],
        )
        .unwrap();
        assert_eq!(hit.0, "0000:03:00.0");
        assert_eq!(hit.1, "gpu-b");
    }
}
