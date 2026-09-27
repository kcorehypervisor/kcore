//! PCI devices discovered from sysfs.
//!
//! Operators ask for a name (`radeon0`, `nic0`, `nvme0`), not a PCI address.
//! Names are per machine: devices of one family are sorted by address and
//! numbered. GPUs are for AI compute. Audio and same-slot USB are IOMMU
//! companions, not separate devices. A raw BDF list is the escape hatch for
//! anything this scanner does not name.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// One passthrough candidate and the PCI functions that belong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    pub name: String,
    pub family: String,
    pub model: String,
    pub address: String,
    pub functions: Vec<String>,
    pub iommu_group: i32,
    pub assignable: bool,
    pub blocked_reason: String,
    /// `gpu`, `nic`, `nvme`, or `raw`.
    pub kind: String,
    /// `compute` (AI), `platform` (host iGPU), or `passthrough`.
    pub role: String,
    pub class_code: String,
    pub characteristics: String,
    pub driver: String,
}

/// `radeon0` picks that device. `radeon` picks the first free device of that family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuSelector {
    Exact(String),
    Family(String),
}

#[derive(Debug, Clone)]
struct PciFn {
    address: String,
    vendor: u16,
    device: u16,
    class: u32,
    iommu_group: Option<i32>,
    driver: String,
}

const FAMILIES: &[&str] = &["nvme", "radeon", "nvidia", "intel", "accel", "nic"];

/// Read `/sys/bus/pci/devices` (or a fixture laid out the same way).
/// Returns GPUs only. Use [`scan_pci`] for NICs, NVMe, and raw groups too.
pub fn scan_sysfs(devices_dir: &Path) -> Result<Vec<GpuDevice>, String> {
    let mut devices = scan_pci(devices_dir)?;
    devices.retain(|dev| dev.kind == "gpu");
    Ok(devices)
}

/// Every named GPU, NIC, and NVMe, plus raw groups the typed scan did not claim.
pub fn scan_pci(devices_dir: &Path) -> Result<Vec<GpuDevice>, String> {
    let entries =
        fs::read_dir(devices_dir).map_err(|e| format!("reading {}: {e}", devices_dir.display()))?;
    let mut devices = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", devices_dir.display()))?;
        let Some(address) = normalize_bdf(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let path = entry.path();
        let Some(vendor) = read_hex_u16(&path.join("vendor")) else {
            continue;
        };
        let Some(device) = read_hex_u16(&path.join("device")) else {
            continue;
        };
        let Some(class) = read_hex_u32(&path.join("class")) else {
            continue;
        };
        devices.push(PciFn {
            address,
            vendor,
            device,
            class,
            iommu_group: iommu_group_of(&path),
            driver: driver_of(&path),
        });
    }
    Ok(classify(devices))
}

pub fn parse_selector(raw: &str) -> Result<GpuSelector, String> {
    let name = raw.trim().to_ascii_lowercase();
    if name.is_empty() {
        return Err("device name is empty".into());
    }
    match family_of_name(&name) {
        Some(family) if name == family => Ok(GpuSelector::Family(family.to_string())),
        Some(_) => Ok(GpuSelector::Exact(name)),
        None => Err(format!(
            "device '{raw}' should look like radeon0, nvidia1, nic0, nvme0, or a family such as radeon"
        )),
    }
}

/// `radeon0` and `radeon` both yield `radeon`. `nic0` yields `nic`.
pub fn family_of_name(name: &str) -> Option<&'static str> {
    for family in FAMILIES {
        if let Some(rest) = name.strip_prefix(family) {
            if rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit()) {
                return Some(*family);
            }
        }
    }
    None
}

/// Pick devices for `requests`. `taken` holds names already assigned on that node.
pub fn select_gpus<'a>(
    inventory: &'a [GpuDevice],
    requests: &[GpuSelector],
    taken: &HashSet<String>,
) -> Result<Vec<&'a GpuDevice>, String> {
    if requests.is_empty() {
        return Err("no device requested".into());
    }
    let mut claimed = taken.clone();
    let mut chosen = Vec::with_capacity(requests.len());
    for request in requests {
        let device = match request {
            GpuSelector::Exact(name) => inventory
                .iter()
                .find(|dev| &dev.name == name && dev.kind != "raw")
                .ok_or_else(|| missing_exact(name, inventory))?,
            GpuSelector::Family(family) => first_free(inventory, family, &claimed)?,
        };
        if device.kind == "raw" {
            return Err(format!(
                "device {} is raw PCI; pass its addresses with --pci",
                device.name
            ));
        }
        if claimed.contains(&device.name) {
            return Err(format!(
                "{} {} is already assigned on this node",
                noun(&device.family),
                device.name
            ));
        }
        if !device.assignable {
            return Err(format!(
                "{} {} is not assignable: {}",
                noun(&device.family),
                device.name,
                device.blocked_reason
            ));
        }
        claimed.insert(device.name.clone());
        chosen.push(device);
    }
    Ok(chosen)
}

fn first_free<'a>(
    inventory: &'a [GpuDevice],
    family: &str,
    claimed: &HashSet<String>,
) -> Result<&'a GpuDevice, String> {
    let mut same_family: Vec<&GpuDevice> = inventory
        .iter()
        .filter(|dev| dev.family == family && dev.kind != "raw")
        .collect();
    same_family.sort_by(|a, b| a.address.cmp(&b.address));
    let label = match family {
        "nic" => "NIC".to_string(),
        "nvme" => "NVMe".to_string(),
        "accel" => "accelerator".to_string(),
        other => format!("{other} GPU"),
    };
    if same_family.is_empty() {
        return Err(format!("no {label} on this node"));
    }
    if let Some(dev) = same_family
        .iter()
        .find(|dev| dev.assignable && !claimed.contains(&dev.name))
    {
        return Ok(*dev);
    }
    let blocked: Vec<String> = same_family
        .iter()
        .filter(|dev| !dev.assignable)
        .map(|dev| format!("{} ({})", dev.name, dev.blocked_reason))
        .collect();
    if blocked.is_empty() {
        Err(format!("every {label} on this node is already assigned"))
    } else {
        Err(format!(
            "no free assignable {label} ({})",
            blocked.join("; ")
        ))
    }
}

fn noun(family: &str) -> &str {
    match family {
        "nic" => "NIC",
        "nvme" => "NVMe",
        "accel" => "accelerator",
        _ => "GPU",
    }
}

fn missing_exact(name: &str, inventory: &[GpuDevice]) -> String {
    let named: Vec<&str> = inventory
        .iter()
        .filter(|dev| dev.kind != "raw")
        .map(|dev| dev.name.as_str())
        .collect();
    if named.is_empty() {
        return format!(
            "device {name} was not found; this node has not reported any named PCI devices"
        );
    }
    format!(
        "device {name} was not found (this node has {})",
        named.join(", ")
    )
}

fn classify(mut devices: Vec<PciFn>) -> Vec<GpuDevice> {
    devices.sort_by(|a, b| a.address.cmp(&b.address));
    let mut claimed: HashSet<String> = HashSet::new();
    let mut counters: HashMap<&'static str, u32> = HashMap::new();
    let mut found = Vec::new();

    let primaries: Vec<PciFn> = devices
        .iter()
        .filter(|dev| is_gpu(dev) || is_nic(dev) || is_nvme(dev))
        .cloned()
        .collect();
    for primary in primaries {
        if claimed.contains(&primary.address) {
            continue;
        }
        let kind = if is_gpu(&primary) {
            "gpu"
        } else if is_nic(&primary) {
            "nic"
        } else {
            "nvme"
        };
        let family = family_of(&primary, kind);
        let index = counters.entry(family).or_insert(0);
        let name = format!("{family}{index}");
        *index += 1;
        let built = build_device(&primary, &devices, kind, family, name);
        if built.assignable {
            for address in &built.functions {
                claimed.insert(address.clone());
            }
        } else {
            claimed.insert(primary.address.clone());
        }
        found.push(built);
    }

    let mut raw_groups: HashMap<String, Vec<&PciFn>> = HashMap::new();
    for dev in &devices {
        if claimed.contains(&dev.address) {
            continue;
        }
        let key = dev
            .iommu_group
            .map(|group| format!("group:{group}"))
            .unwrap_or_else(|| format!("addr:{}", dev.address));
        raw_groups.entry(key).or_default().push(dev);
    }
    let mut raw_keys: Vec<String> = raw_groups.keys().cloned().collect();
    raw_keys.sort();
    for key in raw_keys {
        let mut members = raw_groups.remove(&key).unwrap_or_default();
        members.sort_by(|a, b| a.address.cmp(&b.address));
        let Some(primary) = members.first() else {
            continue;
        };
        found.push(build_device(
            primary,
            &devices,
            "raw",
            "raw",
            primary.address.clone(),
        ));
    }
    found
}

fn build_device(
    primary: &PciFn,
    devices: &[PciFn],
    kind: &str,
    family: &'static str,
    name: String,
) -> GpuDevice {
    let (assignable, blocked_reason, functions, group) = assignment(primary, devices);
    let role = role_of(kind, primary.class, &blocked_reason);
    let companions: Vec<String> = functions
        .iter()
        .filter(|addr| *addr != &primary.address)
        .map(|addr| {
            let label = devices
                .iter()
                .find(|dev| &dev.address == addr)
                .map(|dev| class_detail(dev.class))
                .unwrap_or("other");
            format!("{addr}:{label}")
        })
        .collect();
    GpuDevice {
        name,
        family: family.to_string(),
        model: format!(
            "{} {:04x}:{:04x}",
            model_vendor(kind, primary.vendor),
            primary.vendor,
            primary.device
        ),
        address: primary.address.clone(),
        functions,
        iommu_group: group,
        assignable,
        blocked_reason,
        kind: kind.to_string(),
        role: role.to_string(),
        class_code: format!("0x{:06x}", primary.class & 0xffffff),
        characteristics: characteristics(kind, role, primary, &companions),
        driver: primary.driver.clone(),
    }
}

fn assignment(primary: &PciFn, devices: &[PciFn]) -> (bool, String, Vec<String>, i32) {
    if class_base(primary.class) == 0x06 {
        return (
            false,
            "PCI bridge stays on the host".into(),
            vec![primary.address.clone()],
            primary.iommu_group.unwrap_or(-1),
        );
    }
    let Some(group) = primary.iommu_group else {
        return (
            false,
            "no IOMMU group; boot with the IOMMU enabled before assigning this device".into(),
            vec![primary.address.clone()],
            -1,
        );
    };
    let members: Vec<&PciFn> = devices
        .iter()
        .filter(|dev| dev.iommu_group == Some(group))
        .collect();
    let mut foreign = Vec::new();
    for member in &members {
        if is_companion(primary, member) {
            continue;
        }
        foreign.push(format!(
            "{} ({})",
            member.address,
            class_label(member.class)
        ));
    }
    foreign.sort();
    if foreign.is_empty() {
        let mut functions: Vec<String> = members.iter().map(|dev| dev.address.clone()).collect();
        functions.sort();
        (true, String::new(), functions, group)
    } else {
        (
            false,
            format!("IOMMU group {group} also contains {}", foreign.join(", ")),
            vec![primary.address.clone()],
            group,
        )
    }
}

/// Same-slot functions travel with the device. Bridges never do.
fn is_companion(primary: &PciFn, member: &PciFn) -> bool {
    if member.address == primary.address {
        return true;
    }
    if class_base(member.class) == 0x06 {
        return false;
    }
    slot_of(&member.address) == slot_of(&primary.address)
}

fn role_of(kind: &str, class: u32, blocked_reason: &str) -> &'static str {
    if kind != "gpu" {
        return "passthrough";
    }
    let dirty = !blocked_reason.is_empty();
    if dirty && (class & 0xffff00) == 0x030000 {
        "platform"
    } else {
        "compute"
    }
}

fn characteristics(kind: &str, role: &str, primary: &PciFn, companions: &[String]) -> String {
    let purpose = match (kind, role) {
        ("gpu", "compute") => "purpose=ai",
        ("gpu", "platform") => "purpose=host",
        ("nic", _) => "purpose=network",
        ("nvme", _) => "purpose=storage",
        _ => "purpose=raw",
    };
    let mut parts = vec![
        purpose.to_string(),
        format!("class=0x{:06x}", primary.class & 0xffffff),
        class_detail(primary.class).to_string(),
        format!("vendor={:04x}:{:04x}", primary.vendor, primary.device),
    ];
    if !primary.driver.is_empty() {
        parts.push(format!("driver={}", primary.driver));
    }
    if !companions.is_empty() {
        parts.push(format!(
            "iommu-companions={} (required by the group, not the workload)",
            companions.join(",")
        ));
    }
    parts.join(" ")
}

fn is_gpu(dev: &PciFn) -> bool {
    matches!(class_base(dev.class), 0x03 | 0x12)
}

fn is_nic(dev: &PciFn) -> bool {
    (dev.class & 0xffff00) == 0x020000
}

fn is_nvme(dev: &PciFn) -> bool {
    (dev.class & 0xffff00) == 0x010800
}

fn family_of(dev: &PciFn, kind: &str) -> &'static str {
    match kind {
        "nic" => "nic",
        "nvme" => "nvme",
        "gpu" => match dev.vendor {
            0x1002 | 0x1022 => "radeon",
            0x10de => "nvidia",
            0x8086 => "intel",
            _ => "accel",
        },
        _ => "raw",
    }
}

fn model_vendor(kind: &str, vendor: u16) -> &'static str {
    match kind {
        "nic" => "Ethernet",
        "nvme" => "NVMe",
        "raw" => "PCI",
        _ => match vendor {
            0x1002 | 0x1022 => "AMD",
            0x10de => "NVIDIA",
            0x8086 => "Intel",
            _ => "PCI",
        },
    }
}

fn class_base(class: u32) -> u32 {
    (class >> 16) & 0xff
}

fn class_detail(class: u32) -> &'static str {
    match class & 0xffff00 {
        0x030000 => "VGA",
        0x030200 => "3D",
        0x038000 => "display",
        0x120000 => "accelerator",
        0x020000 => "ethernet",
        0x010800 => "nvme",
        _ => class_label(class),
    }
}

fn class_label(class: u32) -> &'static str {
    match class_base(class) {
        0x01 => "storage",
        0x02 => "network",
        0x03 => "display",
        0x04 => "audio",
        0x06 => "bridge",
        0x0c => "serial bus",
        0x10 => "encryption",
        0x12 => "accelerator",
        _ => "other",
    }
}

fn slot_of(address: &str) -> &str {
    address
        .rsplit_once('.')
        .map(|(slot, _)| slot)
        .unwrap_or(address)
}

fn iommu_group_of(device_dir: &Path) -> Option<i32> {
    let target = fs::read_link(device_dir.join("iommu_group")).ok()?;
    target.file_name()?.to_str()?.parse().ok()
}

fn driver_of(device_dir: &Path) -> String {
    fs::read_link(device_dir.join("driver"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

fn read_hex_u16(path: &Path) -> Option<u16> {
    read_hex_u32(path).map(|value| value as u16)
}

fn read_hex_u32(path: &Path) -> Option<u32> {
    let text = fs::read_to_string(path).ok()?;
    let text = text
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    u32::from_str_radix(text, 16).ok()
}

fn normalize_bdf(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.split([':', '.']).collect();
    if parts.len() != 4 {
        return None;
    }
    if parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 || parts[3].len() != 1 {
        return None;
    }
    if !parts
        .iter()
        .all(|part| part.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    let func = parts[3].as_bytes()[0];
    if !func.is_ascii_digit() || func > b'7' {
        return None;
    }
    Some(raw.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write_fn(
        root: &std::path::Path,
        address: &str,
        vendor: &str,
        device: &str,
        class: &str,
        group: Option<&str>,
    ) {
        let dir = root.join(address);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("vendor"), vendor).unwrap();
        fs::write(dir.join("device"), device).unwrap();
        fs::write(dir.join("class"), class).unwrap();
        if let Some(group) = group {
            let group_dir = root.join("groups").join(group);
            fs::create_dir_all(&group_dir).unwrap();
            symlink(&group_dir, dir.join("iommu_group")).unwrap();
        }
    }

    fn sample(name: &str, family: &str, address: &str) -> GpuDevice {
        GpuDevice {
            name: name.into(),
            family: family.into(),
            model: "AMD 1002:744c".into(),
            address: address.into(),
            functions: vec![address.into()],
            iommu_group: 4,
            assignable: true,
            blocked_reason: String::new(),
            kind: "gpu".into(),
            role: "compute".into(),
            class_code: "0x030000".into(),
            characteristics: "purpose=ai".into(),
            driver: String::new(),
        }
    }

    #[test]
    fn names_a_discrete_radeon_and_its_audio_function() {
        let tmp = tempfile::tempdir().unwrap();
        write_fn(
            tmp.path(),
            "0000:03:00.0",
            "0x1002",
            "0x744c",
            "0x030000",
            Some("15"),
        );
        write_fn(
            tmp.path(),
            "0000:03:00.1",
            "0x1002",
            "0xab30",
            "0x040300",
            Some("15"),
        );
        let gpus = scan_sysfs(tmp.path()).unwrap();
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "radeon0");
        assert_eq!(gpus[0].role, "compute");
        assert!(gpus[0].characteristics.contains("purpose=ai"));
        assert!(gpus[0].assignable);
        assert_eq!(
            gpus[0].functions,
            vec!["0000:03:00.0".to_string(), "0000:03:00.1".to_string()]
        );
        assert!(gpus[0]
            .characteristics
            .contains("iommu-companions=0000:03:00.1:audio"));
    }

    #[test]
    fn three_d_nvidia_is_an_ai_gpu_and_audio_is_not_a_second_device() {
        let tmp = tempfile::tempdir().unwrap();
        write_fn(
            tmp.path(),
            "0000:41:00.0",
            "0x10de",
            "0x2330",
            "0x030200",
            Some("8"),
        );
        write_fn(
            tmp.path(),
            "0000:41:00.1",
            "0x10de",
            "0x228b",
            "0x040300",
            Some("8"),
        );
        let all = scan_pci(tmp.path()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "nvidia0");
        assert_eq!(all[0].kind, "gpu");
        assert_eq!(all[0].role, "compute");
        assert!(all[0].characteristics.contains("purpose=ai"));
        assert!(all[0].characteristics.contains("3D"));
        assert!(all[0].assignable);
    }

    #[test]
    fn processing_accelerator_from_an_unknown_vendor_is_accel() {
        let tmp = tempfile::tempdir().unwrap();
        write_fn(
            tmp.path(),
            "0000:18:00.0",
            "0x1da3",
            "0x1020",
            "0x120000",
            Some("3"),
        );
        let gpus = scan_sysfs(tmp.path()).unwrap();
        assert_eq!(gpus[0].name, "accel0");
        assert_eq!(gpus[0].role, "compute");
        assert!(gpus[0].assignable);
    }

    #[test]
    fn same_slot_usb_stays_with_an_ai_gpu() {
        let tmp = tempfile::tempdir().unwrap();
        write_fn(
            tmp.path(),
            "0000:03:00.0",
            "0x10de",
            "0x2684",
            "0x030200",
            Some("9"),
        );
        write_fn(
            tmp.path(),
            "0000:03:00.1",
            "0x10de",
            "0x22ba",
            "0x040300",
            Some("9"),
        );
        write_fn(
            tmp.path(),
            "0000:03:00.2",
            "0x10de",
            "0x22a0",
            "0x0c0330",
            Some("9"),
        );
        let gpus = scan_sysfs(tmp.path()).unwrap();
        assert!(gpus[0].assignable, "{}", gpus[0].blocked_reason);
        assert_eq!(gpus[0].functions.len(), 3);
        assert!(gpus[0].characteristics.contains("purpose=ai"));
    }

    #[test]
    fn integrated_gpu_sharing_a_group_with_sata_is_not_assignable() {
        let tmp = tempfile::tempdir().unwrap();
        write_fn(
            tmp.path(),
            "0000:0d:00.0",
            "0x1002",
            "0x1638",
            "0x030000",
            Some("2"),
        );
        write_fn(
            tmp.path(),
            "0000:0d:00.1",
            "0x1002",
            "0x1637",
            "0x040300",
            Some("2"),
        );
        write_fn(
            tmp.path(),
            "0000:0e:00.0",
            "0x1022",
            "0x7901",
            "0x010601",
            Some("2"),
        );
        write_fn(
            tmp.path(),
            "0000:0d:00.3",
            "0x1022",
            "0x1639",
            "0x0c0330",
            Some("2"),
        );
        let gpus = scan_sysfs(tmp.path()).unwrap();
        assert_eq!(gpus[0].name, "radeon0");
        assert_eq!(gpus[0].role, "platform");
        assert!(gpus[0].characteristics.contains("purpose=host"));
        assert!(!gpus[0].assignable);
        assert!(gpus[0].blocked_reason.contains("0000:0e:00.0"));
        assert!(gpus[0].blocked_reason.contains("storage"));
    }

    #[test]
    fn lists_nic_nvme_and_a_raw_function() {
        let tmp = tempfile::tempdir().unwrap();
        write_fn(
            tmp.path(),
            "0000:01:00.0",
            "0x8086",
            "0x15f3",
            "0x020000",
            Some("4"),
        );
        write_fn(
            tmp.path(),
            "0000:02:00.0",
            "0x144d",
            "0xa808",
            "0x010802",
            Some("5"),
        );
        write_fn(
            tmp.path(),
            "0000:04:00.0",
            "0x8086",
            "0x15b8",
            "0x088000",
            Some("6"),
        );
        let all = scan_pci(tmp.path()).unwrap();
        let nic = all.iter().find(|dev| dev.kind == "nic").unwrap();
        assert_eq!(nic.name, "nic0");
        assert!(nic.assignable);
        assert!(nic.characteristics.contains("purpose=network"));
        let nvme = all.iter().find(|dev| dev.kind == "nvme").unwrap();
        assert_eq!(nvme.name, "nvme0");
        assert!(nvme.characteristics.contains("purpose=storage"));
        let raw = all.iter().find(|dev| dev.kind == "raw").unwrap();
        assert_eq!(raw.name, "0000:04:00.0");
        assert!(raw.assignable);
        assert!(parse_selector(&raw.name).is_err());
    }

    #[test]
    fn numbers_two_radeons_by_address() {
        let tmp = tempfile::tempdir().unwrap();
        write_fn(
            tmp.path(),
            "0000:41:00.0",
            "0x1002",
            "0x744c",
            "0x030000",
            Some("8"),
        );
        write_fn(
            tmp.path(),
            "0000:03:00.0",
            "0x1002",
            "0x73bf",
            "0x030000",
            Some("4"),
        );
        let gpus = scan_sysfs(tmp.path()).unwrap();
        assert_eq!(gpus[0].name, "radeon0");
        assert_eq!(gpus[0].address, "0000:03:00.0");
        assert_eq!(gpus[1].name, "radeon1");
        assert_eq!(gpus[1].address, "0000:41:00.0");
    }

    #[test]
    fn selects_exact_and_free_family_members() {
        let gpus = vec![
            sample("radeon0", "radeon", "0000:03:00.0"),
            sample("radeon1", "radeon", "0000:41:00.0"),
        ];
        let taken = HashSet::from(["radeon0".to_string()]);
        let picked = select_gpus(&gpus, &[GpuSelector::Family("radeon".into())], &taken).unwrap();
        assert_eq!(picked[0].name, "radeon1");
        let err = select_gpus(&gpus, &[GpuSelector::Exact("radeon0".into())], &taken).unwrap_err();
        assert!(err.contains("already assigned"));
    }

    #[test]
    fn family_selector_picks_a_free_nic() {
        let mut nic0 = sample("nic0", "nic", "0000:01:00.0");
        nic0.kind = "nic".into();
        let mut nic1 = sample("nic1", "nic", "0000:02:00.0");
        nic1.kind = "nic".into();
        let taken = HashSet::from(["nic0".to_string()]);
        let inventory = [nic0, nic1];
        let picked = select_gpus(&inventory, &[GpuSelector::Family("nic".into())], &taken).unwrap();
        assert_eq!(picked[0].name, "nic1");
    }

    #[test]
    fn this_host_igpu_is_not_assignable_when_present() {
        let path = Path::new("/sys/bus/pci/devices");
        let Ok(gpus) = scan_sysfs(path) else {
            return;
        };
        let Some(igpu) = gpus.iter().find(|gpu| gpu.address == "0000:0d:00.0") else {
            return;
        };
        assert_eq!(igpu.name, "radeon0");
        assert_eq!(igpu.role, "platform");
        assert!(!igpu.assignable, "{}", igpu.blocked_reason);
        assert!(igpu.blocked_reason.contains("0000:0e:00.0"));
    }
}
