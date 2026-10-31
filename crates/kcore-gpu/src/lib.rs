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
    /// Card name for a slice (`nvidia0`). Empty on a whole device.
    pub parent: String,
    /// Current vGPU type (`A2-4C`). Empty when the slice has none.
    pub profile: String,
    /// Types this slice can take now.
    pub profiles: Vec<String>,
}

/// A vGPU request. A profile is always present: a slice without one has no size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VgpuSelector {
    /// Any host card with a free slice of this profile.
    Profile(String),
    /// A free slice of this profile on one card (`nvidia0=A2-4C`).
    Card { card: String, profile: String },
    /// One virtual function (`nvidia0/3=A2-4C`).
    Slice { name: String, profile: String },
}

/// Older Virtual GPU Managers bind each virtual function to this driver.
pub const NVIDIA_VFIO_DRIVER: &str = "nvidia-vgpu-vfio";

/// Virtual GPU Manager 595 binds each virtual function to `nvidia` and
/// publishes `nvidia/creatable_vgpu_types`. The vfio device node is created
/// beside that binding. A function with no types is not a slice.
fn slice_driver_ok(vf: &PciFn) -> bool {
    if vf.driver == NVIDIA_VFIO_DRIVER {
        return true;
    }
    vf.driver == "nvidia"
        && (!vf.vgpu_types.is_empty() || vf.vgpu_current.is_some_and(|id| id != 0))
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
    physfn: Option<String>,
    sriov_numvfs: Option<u32>,
    sriov_totalvfs: Option<u32>,
    virtfns: Vec<(u32, String)>,
    /// `nvidia/current_vgpu_type`. `None` when the file is absent. `0` means no profile.
    vgpu_current: Option<u32>,
    vgpu_types: Vec<(u32, String)>,
    mdev: bool,
}

const FAMILIES: &[&str] = &["nvme", "radeon", "nvidia", "intel", "accel", "nic"];

/// Read `/sys/bus/pci/devices` (or a fixture laid out the same way).
/// Returns GPUs only. Use [`scan_pci`] for NICs, NVMe, and raw groups too.
pub fn scan_sysfs(devices_dir: &Path) -> Result<Vec<GpuDevice>, String> {
    let mut devices = scan_pci(devices_dir)?;
    devices.retain(|dev| dev.kind == "gpu" || dev.kind == "vgpu");
    Ok(devices)
}

/// Every named GPU, NIC, and NVMe, plus raw groups the typed scan did not claim.
pub fn scan_pci(devices_dir: &Path) -> Result<Vec<GpuDevice>, String> {
    scan_pci_with(devices_dir, &[])
}

/// Like [`scan_pci`], and treats addresses in `vgpu_hosts` as vGPU cards even
/// before `sriov-manage` has created virtual functions.
pub fn scan_pci_with(devices_dir: &Path, vgpu_hosts: &[String]) -> Result<Vec<GpuDevice>, String> {
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
            physfn: link_bdf(&path.join("physfn")),
            sriov_numvfs: read_u32_file(&path.join("sriov_numvfs")),
            sriov_totalvfs: read_u32_file(&path.join("sriov_totalvfs")),
            virtfns: virtfns_of(&path),
            vgpu_current: read_u32_file(&path.join("nvidia").join("current_vgpu_type")),
            vgpu_types: read_vgpu_types(&path.join("nvidia").join("creatable_vgpu_types")),
            mdev: path.join("mdev_supported_types").is_dir(),
        });
    }
    Ok(classify(devices, vgpu_hosts))
}

/// Addresses in `/etc/kcore/vgpu-hosts`, one PCI address per line.
pub fn load_vgpu_hosts(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(|line| line.trim().to_ascii_lowercase())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
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

/// Device half of a stored slice binding (`nvidia0/3=A2-4C` → `nvidia0/3`).
pub fn binding_device_name(stored: &str) -> &str {
    stored
        .split_once('=')
        .map(|(name, _)| name)
        .unwrap_or(stored)
}

/// Profile half of a stored slice binding.
pub fn binding_profile(stored: &str) -> Option<&str> {
    stored
        .split_once('=')
        .map(|(_, profile)| profile)
        .filter(|profile| !profile.is_empty())
}

/// `A2-4C`, `nvidia0=A2-4C`, or `nvidia0/3=A2-4C`.
pub fn parse_vgpu_selector(raw: &str) -> Result<VgpuSelector, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("vGPU request is empty".into());
    }
    if let Some((left, profile)) = raw.split_once('=') {
        let profile = normalize_profile(profile)?;
        let left = left.trim().to_ascii_lowercase();
        if left.is_empty() {
            return Err(format!(
                "vGPU request '{raw}' needs a card or slice before the profile"
            ));
        }
        if left.contains('/') {
            if !left.split_once('/').is_some_and(|(card, index)| {
                family_of_name(card).is_some()
                    && card != family_of_name(card).unwrap_or("")
                    && !index.is_empty()
                    && index.bytes().all(|b| b.is_ascii_digit())
            }) {
                return Err(format!(
                    "vGPU slice '{raw}' needs a profile, for example nvidia0/3=A2-4C"
                ));
            }
            return Ok(VgpuSelector::Slice {
                name: left,
                profile,
            });
        }
        match family_of_name(&left) {
            Some(family) if left != family => {
                return Ok(VgpuSelector::Card {
                    card: left,
                    profile,
                });
            }
            _ => {
                return Err(format!("vGPU card '{left}' should look like nvidia0=A2-4C"));
            }
        }
    }
    if raw.contains('/') {
        return Err(format!(
            "vGPU slice '{raw}' needs a profile, for example nvidia0/3=A2-4C"
        ));
    }
    Ok(VgpuSelector::Profile(normalize_profile(raw)?))
}

fn normalize_profile(raw: &str) -> Result<String, String> {
    let mut profile = raw.trim();
    if let Some(rest) = profile
        .strip_prefix("NVIDIA ")
        .or_else(|| profile.strip_prefix("nvidia "))
    {
        profile = rest.trim();
    }
    if profile.is_empty() {
        return Err("vGPU profile is empty".into());
    }
    if !profile
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
        || !profile.chars().any(|c| c.is_ascii_alphabetic())
        || !profile.chars().any(|c| c.is_ascii_digit())
    {
        return Err(format!("vGPU profile '{profile}' should look like A2-4C"));
    }
    Ok(profile.to_string())
}

/// Pick slices for `requests`. `taken` holds stored bindings (`nvidia0/3=A2-4C`
/// or a whole-device name). A card already running the profile is used before
/// an empty card. A card running a different profile offers nothing.
pub fn select_vgpus<'a>(
    inventory: &'a [GpuDevice],
    requests: &[VgpuSelector],
    taken: &HashSet<String>,
) -> Result<Vec<(&'a GpuDevice, String)>, String> {
    if requests.is_empty() {
        return Err("no vGPU slice requested".into());
    }
    let mut claimed: HashSet<String> = taken
        .iter()
        .map(|name| binding_device_name(name).to_string())
        .collect();
    let mut chosen = Vec::with_capacity(requests.len());
    for request in requests {
        let (device, profile) = match request {
            VgpuSelector::Profile(profile) => (
                pick_profile(inventory, profile, taken, &claimed)?,
                profile.clone(),
            ),
            VgpuSelector::Card { card, profile } => (
                pick_card(inventory, card, profile, taken, &claimed)?,
                profile.clone(),
            ),
            VgpuSelector::Slice { name, profile } => (
                pick_slice(inventory, name, profile, taken, &claimed)?,
                profile.clone(),
            ),
        };
        claimed.insert(device.name.clone());
        chosen.push((device, profile));
    }
    Ok(chosen)
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
                .find(|dev| &dev.name == name && dev.kind != "raw" && dev.kind != "vgpu")
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
        .filter(|dev| dev.family == family && dev.kind != "raw" && dev.kind != "vgpu")
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

fn classify(mut devices: Vec<PciFn>, vgpu_hosts: &[String]) -> Vec<GpuDevice> {
    devices.sort_by(|a, b| a.address.cmp(&b.address));
    let mut claimed: HashSet<String> = HashSet::new();
    let mut counters: HashMap<&'static str, u32> = HashMap::new();
    let mut found = Vec::new();

    let primaries: Vec<PciFn> = devices
        .iter()
        .filter(|dev| !is_nvidia_gpu_vf(dev) && (is_gpu(dev) || is_nic(dev) || is_nvme(dev)))
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
        if claimed.contains(&dev.address) || is_nvidia_gpu_vf(dev) {
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
    apply_vgpu(&mut found, &devices, vgpu_hosts, &mut claimed);
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
        parent: String::new(),
        profile: String::new(),
        profiles: Vec::new(),
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

fn is_nvidia_gpu_vf(dev: &PciFn) -> bool {
    dev.physfn.is_some() && dev.vendor == 0x10de && is_gpu(dev)
}

fn is_vgpu_host(pci: &PciFn, vgpu_hosts: &[String]) -> bool {
    pci.vendor == 0x10de
        && pci.driver == "nvidia"
        && (pci.sriov_numvfs.is_some_and(|count| count > 0)
            || vgpu_hosts.iter().any(|addr| addr == &pci.address))
}

fn apply_vgpu(
    found: &mut Vec<GpuDevice>,
    devices: &[PciFn],
    vgpu_hosts: &[String],
    claimed: &mut HashSet<String>,
) {
    let mut slices = Vec::new();
    for dev in found.iter_mut() {
        if dev.kind != "gpu" || dev.family != "nvidia" {
            continue;
        }
        let Some(pci) = devices.iter().find(|row| row.address == dev.address) else {
            continue;
        };
        if !is_vgpu_host(pci, vgpu_hosts) {
            continue;
        }
        dev.role = "vgpu-host".into();
        dev.assignable = false;
        dev.blocked_reason = "vGPU host card; request a slice with --vgpu".into();
        let vfs = pci.sriov_totalvfs.unwrap_or_else(|| {
            pci.sriov_numvfs
                .filter(|count| *count > 0)
                .unwrap_or(pci.virtfns.len() as u32)
        });
        dev.characteristics
            .push_str(&format!(" sriov=pf vfs={vfs}"));
    }

    let hosts: Vec<(String, String)> = found
        .iter()
        .filter(|dev| dev.role == "vgpu-host")
        .map(|dev| (dev.address.clone(), dev.name.clone()))
        .collect();
    for (address, name) in hosts {
        let Some(pci) = devices.iter().find(|row| row.address == address) else {
            continue;
        };
        let Some(parent) = found.iter().find(|dev| dev.name == name).cloned() else {
            continue;
        };
        for (index, vf_addr) in &pci.virtfns {
            let Some(vf) = devices.iter().find(|row| &row.address == vf_addr) else {
                continue;
            };
            if claimed.contains(&vf.address) {
                continue;
            }
            let slice = build_slice(&parent, vf, *index, devices);
            for addr in &slice.functions {
                claimed.insert(addr.clone());
            }
            claimed.insert(slice.address.clone());
            slices.push(slice);
        }
    }

    for vf in devices.iter().filter(|row| is_nvidia_gpu_vf(row)) {
        if claimed.contains(&vf.address) {
            continue;
        }
        let Some(parent_addr) = vf.physfn.as_deref() else {
            continue;
        };
        let Some(parent) = found
            .iter()
            .find(|dev| dev.address == parent_addr && dev.family == "nvidia")
            .cloned()
        else {
            continue;
        };
        let index = devices
            .iter()
            .find(|row| row.address == parent.address)
            .and_then(|row| {
                row.virtfns
                    .iter()
                    .find(|(_, addr)| addr == &vf.address)
                    .map(|(index, _)| *index)
            })
            .unwrap_or_else(|| vf_function(vf));
        let slice = build_slice(&parent, vf, index, devices);
        for addr in &slice.functions {
            claimed.insert(addr.clone());
        }
        claimed.insert(slice.address.clone());
        slices.push(slice);
    }
    found.extend(slices);
}

fn vf_function(vf: &PciFn) -> u32 {
    vf.address
        .rsplit_once('.')
        .and_then(|(_, func)| func.parse().ok())
        .unwrap_or(0)
}

fn build_slice(parent: &GpuDevice, vf: &PciFn, index: u32, devices: &[PciFn]) -> GpuDevice {
    let (mut assignable, mut blocked_reason, functions, group) = assignment(vf, devices);
    let (profile, profiles) = slice_profile(vf, devices);
    if assignable && vf.mdev && vf.vgpu_types.is_empty() && vf.vgpu_current.is_none() {
        assignable = false;
        blocked_reason = "mdev vGPU is not supported; install vGPU 17 or later".into();
    } else if assignable && parent.driver != "nvidia" {
        assignable = false;
        let driver = if parent.driver.is_empty() {
            "unbound"
        } else {
            parent.driver.as_str()
        };
        blocked_reason = format!("parent {} driver is {driver}, not nvidia", parent.address);
    } else if assignable && vf.driver == "vfio-pci" {
        assignable = false;
        blocked_reason =
            "bound to vfio-pci; a vGPU slice stays on nvidia or nvidia-vgpu-vfio".into();
    } else if assignable && !slice_driver_ok(vf) {
        assignable = false;
        let driver = if vf.driver.is_empty() {
            "unbound"
        } else {
            vf.driver.as_str()
        };
        blocked_reason = if driver == "nvidia" {
            "bound to nvidia but this function has no vGPU types".into()
        } else {
            format!("bound to {driver}; a vGPU slice must use nvidia or nvidia-vgpu-vfio")
        };
    }
    let mut parts = vec![
        "purpose=ai".to_string(),
        format!("parent={}", parent.name),
        format!("class=0x{:06x}", vf.class & 0xffffff),
        format!("vendor={:04x}:{:04x}", vf.vendor, vf.device),
    ];
    if !profile.is_empty() {
        parts.push(format!("profile={profile}"));
    }
    if !profiles.is_empty() {
        parts.push(format!("profiles={}", profiles.join(",")));
    }
    if !vf.driver.is_empty() {
        parts.push(format!("driver={}", vf.driver));
    }
    GpuDevice {
        name: format!("{}/{index}", parent.name),
        family: "nvidia".into(),
        model: format!("{} VF", parent.model),
        address: vf.address.clone(),
        functions,
        iommu_group: group,
        assignable,
        blocked_reason,
        kind: "vgpu".into(),
        role: "slice".into(),
        class_code: format!("0x{:06x}", vf.class & 0xffffff),
        characteristics: parts.join(" "),
        driver: vf.driver.clone(),
        parent: parent.name.clone(),
        profile,
        profiles,
    }
}

fn slice_profile(vf: &PciFn, devices: &[PciFn]) -> (String, Vec<String>) {
    let profiles: Vec<String> = vf.vgpu_types.iter().map(|(_, name)| name.clone()).collect();
    let profile = match vf.vgpu_current {
        Some(id) if id != 0 => devices
            .iter()
            .find_map(|dev| {
                dev.vgpu_types
                    .iter()
                    .find(|(type_id, _)| *type_id == id)
                    .map(|(_, name)| name.clone())
            })
            .unwrap_or_else(|| id.to_string()),
        _ => String::new(),
    };
    (profile, profiles)
}

fn slices_of<'a>(inventory: &'a [GpuDevice], card: &str) -> Vec<&'a GpuDevice> {
    let mut slices: Vec<&GpuDevice> = inventory
        .iter()
        .filter(|dev| dev.kind == "vgpu" && dev.parent == card)
        .collect();
    slices.sort_by_key(|slice| slice_index(&slice.name));
    slices
}

fn slice_index(name: &str) -> u32 {
    name.rsplit_once('/')
        .and_then(|(_, index)| index.parse().ok())
        .unwrap_or(0)
}

fn card_lock(slices: &[&GpuDevice], taken: &HashSet<String>) -> Result<Option<String>, String> {
    let mut profiles = Vec::new();
    for slice in slices {
        if !slice.profile.is_empty() {
            profiles.push(slice.profile.clone());
        }
        for binding in taken {
            if binding_device_name(binding) == slice.name {
                if let Some(profile) = binding_profile(binding) {
                    profiles.push(profile.to_string());
                }
            }
        }
    }
    profiles.sort();
    profiles.dedup();
    match profiles.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        many => Err(format!(
            "vGPU card {} has more than one profile ({})",
            slices.first().map(|dev| dev.parent.as_str()).unwrap_or("?"),
            many.join(", ")
        )),
    }
}

fn pick_profile<'a>(
    inventory: &'a [GpuDevice],
    profile: &str,
    taken: &HashSet<String>,
    claimed: &HashSet<String>,
) -> Result<&'a GpuDevice, String> {
    let mut cards: Vec<&GpuDevice> = inventory
        .iter()
        .filter(|dev| dev.role == "vgpu-host")
        .collect();
    cards.sort_by(|a, b| a.address.cmp(&b.address));
    let mut running = Vec::new();
    let mut empty = Vec::new();
    for card in cards {
        let slices = slices_of(inventory, &card.name);
        let lock = card_lock(&slices, taken)?;
        match lock.as_deref() {
            Some(current) if current == profile => running.push(card),
            Some(_) => {}
            None if slices
                .iter()
                .any(|slice| slice.profiles.iter().any(|name| name == profile)) =>
            {
                empty.push(card);
            }
            None => {}
        }
    }
    for card in running.into_iter().chain(empty) {
        if let Some(slice) = free_slice(inventory, &card.name, profile, taken, claimed)? {
            return Ok(slice);
        }
    }
    Err(format!("no free {profile} slice on this node"))
}

fn pick_card<'a>(
    inventory: &'a [GpuDevice],
    card: &str,
    profile: &str,
    taken: &HashSet<String>,
    claimed: &HashSet<String>,
) -> Result<&'a GpuDevice, String> {
    let host = inventory
        .iter()
        .find(|dev| dev.name == card && dev.role == "vgpu-host")
        .ok_or_else(|| format!("vGPU card {card} was not found on this node"))?;
    let slices = slices_of(inventory, &host.name);
    if let Some(lock) = card_lock(&slices, taken)? {
        if lock != profile {
            return Err(format!("vGPU card {card} is running {lock}, not {profile}"));
        }
    }
    free_slice(inventory, card, profile, taken, claimed)?
        .ok_or_else(|| format!("no free {profile} slice on {card}"))
}

fn pick_slice<'a>(
    inventory: &'a [GpuDevice],
    name: &str,
    profile: &str,
    taken: &HashSet<String>,
    claimed: &HashSet<String>,
) -> Result<&'a GpuDevice, String> {
    let slice = inventory
        .iter()
        .find(|dev| dev.name == name && dev.kind == "vgpu")
        .ok_or_else(|| format!("vGPU slice {name} was not found on this node"))?;
    if claimed.contains(&slice.name) {
        return Err(format!(
            "vGPU slice {name} is already assigned on this node"
        ));
    }
    if !slice.assignable {
        return Err(format!(
            "vGPU slice {name} is not assignable: {}",
            slice.blocked_reason
        ));
    }
    if !slice.profiles.iter().any(|have| have == profile) && slice.profile != profile {
        return Err(format!("vGPU slice {name} cannot take {profile}"));
    }
    let slices = slices_of(inventory, &slice.parent);
    if let Some(lock) = card_lock(&slices, taken)? {
        if lock != profile {
            return Err(format!(
                "vGPU card {} is running {lock}, not {profile}",
                slice.parent
            ));
        }
    }
    Ok(slice)
}

fn free_slice<'a>(
    inventory: &'a [GpuDevice],
    card: &str,
    profile: &str,
    taken: &HashSet<String>,
    claimed: &HashSet<String>,
) -> Result<Option<&'a GpuDevice>, String> {
    let slices = slices_of(inventory, card);
    if let Some(lock) = card_lock(&slices, taken)? {
        if lock != profile {
            return Ok(None);
        }
    }
    Ok(slices.into_iter().find(|slice| {
        slice.assignable
            && !claimed.contains(&slice.name)
            && (slice.profile.is_empty() || slice.profile == profile)
            && (slice.profiles.iter().any(|name| name == profile) || slice.profile == profile)
    }))
}

fn link_bdf(path: &Path) -> Option<String> {
    let target = fs::read_link(path).ok()?;
    let name = target.file_name()?.to_str()?;
    normalize_bdf(name)
}

fn read_u32_file(path: &Path) -> Option<u32> {
    let text = fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}

fn virtfns_of(device_dir: &Path) -> Vec<(u32, String)> {
    let Ok(entries) = fs::read_dir(device_dir) else {
        return Vec::new();
    };
    let mut virtfns = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(index) = name.strip_prefix("virtfn") else {
            continue;
        };
        let Ok(index) = index.parse::<u32>() else {
            continue;
        };
        if let Some(address) = link_bdf(&entry.path()) {
            virtfns.push((index, address));
        }
    }
    virtfns.sort_by_key(|(index, _)| *index);
    virtfns
}

fn read_vgpu_types(path: &Path) -> Vec<(u32, String)> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut types = Vec::new();
    for line in text.lines() {
        let Some((id, name)) = line.split_once(':') else {
            continue;
        };
        let Ok(id) = id.trim().parse::<u32>() else {
            continue;
        };
        let mut name = name.trim();
        if let Some(rest) = name.strip_prefix("NVIDIA ") {
            name = rest;
        }
        if name.is_empty() {
            continue;
        }
        types.push((id, name.to_string()));
    }
    types
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
            parent: String::new(),
            profile: String::new(),
            profiles: Vec::new(),
        }
    }

    fn link_driver(root: &std::path::Path, address: &str, driver: &str) {
        let drivers = root.join("drivers").join(driver);
        fs::create_dir_all(&drivers).unwrap();
        symlink(&drivers, root.join(address).join("driver")).unwrap();
    }

    fn write_sriov_card(root: &std::path::Path, card: &str, vfs: &[&str]) {
        fs::write(
            root.join(card).join("sriov_totalvfs"),
            vfs.len().to_string(),
        )
        .unwrap();
        fs::write(root.join(card).join("sriov_numvfs"), vfs.len().to_string()).unwrap();
        for (index, vf) in vfs.iter().enumerate() {
            symlink(
                root.join(vf),
                root.join(card).join(format!("virtfn{index}")),
            )
            .unwrap();
            symlink(root.join(card), root.join(vf).join("physfn")).unwrap();
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

    #[test]
    fn sriov_card_is_one_host_and_named_slices() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:af:00.0",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("42"),
        );
        link_driver(root, "0000:af:00.0", "nvidia");
        for (index, addr) in [
            "0000:af:00.4",
            "0000:af:00.5",
            "0000:af:00.6",
            "0000:af:00.7",
        ]
        .iter()
        .enumerate()
        {
            write_fn(
                root,
                addr,
                "0x10de",
                "0x25b6",
                "0x030200",
                Some(&format!("{}", 43 + index)),
            );
            link_driver(root, addr, NVIDIA_VFIO_DRIVER);
            fs::create_dir_all(root.join(addr).join("nvidia")).unwrap();
            fs::write(
                root.join(addr).join("nvidia").join("creatable_vgpu_types"),
                "468 : NVIDIA A2-4C\n469 : NVIDIA A2-8C\n",
            )
            .unwrap();
            fs::write(
                root.join(addr).join("nvidia").join("current_vgpu_type"),
                "0",
            )
            .unwrap();
        }
        write_sriov_card(
            root,
            "0000:af:00.0",
            &[
                "0000:af:00.4",
                "0000:af:00.5",
                "0000:af:00.6",
                "0000:af:00.7",
            ],
        );
        let rows = scan_pci(root).unwrap();
        let hosts: Vec<_> = rows.iter().filter(|dev| dev.role == "vgpu-host").collect();
        let slices: Vec<_> = rows.iter().filter(|dev| dev.role == "slice").collect();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "nvidia0");
        assert!(!hosts[0].assignable);
        assert!(hosts[0].blocked_reason.contains("--vgpu"));
        assert!(hosts[0].characteristics.contains("sriov=pf"));
        assert_eq!(slices.len(), 4);
        assert_eq!(slices[0].name, "nvidia0/0");
        assert_eq!(slices[0].kind, "vgpu");
        assert_eq!(slices[0].parent, "nvidia0");
        assert!(slices[0].model.ends_with(" VF"));
        assert!(slices[0].assignable, "{}", slices[0].blocked_reason);
        assert!(slices[0].profiles.iter().any(|name| name == "A2-4C"));
        assert!(rows.iter().all(|dev| dev.name != "nvidia1"));
    }

    #[test]
    fn programmed_slice_reports_its_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:af:00.0",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("42"),
        );
        link_driver(root, "0000:af:00.0", "nvidia");
        write_fn(
            root,
            "0000:af:00.4",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("43"),
        );
        link_driver(root, "0000:af:00.4", NVIDIA_VFIO_DRIVER);
        fs::create_dir_all(root.join("0000:af:00.4").join("nvidia")).unwrap();
        fs::write(
            root.join("0000:af:00.4")
                .join("nvidia")
                .join("creatable_vgpu_types"),
            "468 : NVIDIA A2-4C\n",
        )
        .unwrap();
        fs::write(
            root.join("0000:af:00.4")
                .join("nvidia")
                .join("current_vgpu_type"),
            "468",
        )
        .unwrap();
        write_sriov_card(root, "0000:af:00.0", &["0000:af:00.4"]);
        let rows = scan_pci(root).unwrap();
        let slice = rows.iter().find(|dev| dev.role == "slice").unwrap();
        assert_eq!(slice.profile, "A2-4C");
    }

    #[test]
    fn programmed_slice_borrows_the_type_name_from_a_free_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:af:00.0",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("42"),
        );
        link_driver(root, "0000:af:00.0", "nvidia");
        for (addr, current, types) in [
            ("0000:af:00.4", "754", "ID    : vGPU Name\n"),
            ("0000:af:00.5", "0", "754 : NVIDIA A2-4C\n"),
        ] {
            write_fn(root, addr, "0x10de", "0x25b6", "0x030200", Some("43"));
            link_driver(root, addr, "nvidia");
            fs::create_dir_all(root.join(addr).join("nvidia")).unwrap();
            fs::write(
                root.join(addr).join("nvidia").join("creatable_vgpu_types"),
                types,
            )
            .unwrap();
            fs::write(
                root.join(addr).join("nvidia").join("current_vgpu_type"),
                current,
            )
            .unwrap();
        }
        write_sriov_card(root, "0000:af:00.0", &["0000:af:00.4", "0000:af:00.5"]);
        let rows = scan_pci(root).unwrap();
        let programmed = rows
            .iter()
            .find(|dev| dev.address == "0000:af:00.4")
            .unwrap();
        assert_eq!(programmed.profile, "A2-4C");
    }

    #[test]
    fn slice_on_vfio_pci_is_blocked_and_names_the_driver() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:af:00.0",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("42"),
        );
        link_driver(root, "0000:af:00.0", "nvidia");
        write_fn(
            root,
            "0000:af:00.4",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("43"),
        );
        link_driver(root, "0000:af:00.4", "vfio-pci");
        fs::create_dir_all(root.join("0000:af:00.4").join("nvidia")).unwrap();
        fs::write(
            root.join("0000:af:00.4")
                .join("nvidia")
                .join("creatable_vgpu_types"),
            "468 : NVIDIA A2-4C\n",
        )
        .unwrap();
        fs::write(
            root.join("0000:af:00.4")
                .join("nvidia")
                .join("current_vgpu_type"),
            "0",
        )
        .unwrap();
        write_sriov_card(root, "0000:af:00.0", &["0000:af:00.4"]);
        let rows = scan_pci(root).unwrap();
        let slice = rows.iter().find(|dev| dev.role == "slice").unwrap();
        assert!(!slice.assignable);
        assert!(
            slice.blocked_reason.contains("vfio-pci"),
            "{}",
            slice.blocked_reason
        );
    }

    #[test]
    fn nvidia_bound_function_with_types_is_a_slice() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:af:00.0",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("42"),
        );
        link_driver(root, "0000:af:00.0", "nvidia");
        write_fn(
            root,
            "0000:af:00.4",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("43"),
        );
        link_driver(root, "0000:af:00.4", "nvidia");
        fs::create_dir_all(root.join("0000:af:00.4").join("nvidia")).unwrap();
        fs::write(
            root.join("0000:af:00.4")
                .join("nvidia")
                .join("creatable_vgpu_types"),
            "754 : NVIDIA A2-4C\n",
        )
        .unwrap();
        fs::write(
            root.join("0000:af:00.4")
                .join("nvidia")
                .join("current_vgpu_type"),
            "0",
        )
        .unwrap();
        write_sriov_card(root, "0000:af:00.0", &["0000:af:00.4"]);
        let rows = scan_pci(root).unwrap();
        let slice = rows.iter().find(|dev| dev.role == "slice").unwrap();
        assert!(slice.assignable, "{}", slice.blocked_reason);
        assert!(slice.profiles.iter().any(|name| name == "A2-4C"));
        assert_eq!(slice.driver, "nvidia");
    }

    #[test]
    fn nvidia_bound_function_without_types_stays_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:af:00.0",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("42"),
        );
        link_driver(root, "0000:af:00.0", "nvidia");
        write_fn(
            root,
            "0000:af:00.4",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("43"),
        );
        link_driver(root, "0000:af:00.4", "nvidia");
        write_sriov_card(root, "0000:af:00.0", &["0000:af:00.4"]);
        let rows = scan_pci(root).unwrap();
        let slice = rows.iter().find(|dev| dev.role == "slice").unwrap();
        assert!(!slice.assignable);
        assert!(
            slice.blocked_reason.contains("no vGPU types"),
            "{}",
            slice.blocked_reason
        );
    }

    #[test]
    fn hosts_file_marks_a_card_before_sriov_manage() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:af:00.0",
            "0x10de",
            "0x25b6",
            "0x030200",
            Some("42"),
        );
        link_driver(root, "0000:af:00.0", "nvidia");
        fs::write(root.join("0000:af:00.0").join("sriov_numvfs"), "0").unwrap();
        fs::write(root.join("0000:af:00.0").join("sriov_totalvfs"), "16").unwrap();
        let rows = scan_pci_with(root, &["0000:af:00.0".into()]).unwrap();
        let host = rows.iter().find(|dev| dev.name == "nvidia0").unwrap();
        assert_eq!(host.role, "vgpu-host");
        assert!(!host.assignable);
    }

    #[test]
    fn mdev_slice_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:af:00.0",
            "0x10de",
            "0x1eb8",
            "0x030200",
            Some("42"),
        );
        link_driver(root, "0000:af:00.0", "nvidia");
        write_fn(
            root,
            "0000:af:00.4",
            "0x10de",
            "0x1eb8",
            "0x030200",
            Some("43"),
        );
        link_driver(root, "0000:af:00.4", "vfio-pci");
        fs::create_dir_all(root.join("0000:af:00.4").join("mdev_supported_types")).unwrap();
        write_sriov_card(root, "0000:af:00.0", &["0000:af:00.4"]);
        let rows = scan_pci(root).unwrap();
        let slice = rows.iter().find(|dev| dev.role == "slice").unwrap();
        assert!(
            slice.blocked_reason.contains("mdev"),
            "{}",
            slice.blocked_reason
        );
    }

    #[test]
    fn nic_virtual_function_stays_a_nic() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_fn(
            root,
            "0000:01:00.0",
            "0x8086",
            "0x1572",
            "0x020000",
            Some("3"),
        );
        write_fn(
            root,
            "0000:01:10.0",
            "0x8086",
            "0x1572",
            "0x020000",
            Some("9"),
        );
        symlink(
            root.join("0000:01:00.0"),
            root.join("0000:01:10.0").join("physfn"),
        )
        .unwrap();
        let rows = scan_pci(root).unwrap();
        assert!(rows
            .iter()
            .any(|dev| dev.kind == "nic" && dev.address == "0000:01:10.0"));
        assert!(rows.iter().all(|dev| dev.kind != "vgpu"));
    }

    #[test]
    fn parses_vgpu_requests() {
        assert_eq!(
            parse_vgpu_selector("A2-4C").unwrap(),
            VgpuSelector::Profile("A2-4C".into())
        );
        assert_eq!(
            parse_vgpu_selector("nvidia0=A2-4C").unwrap(),
            VgpuSelector::Card {
                card: "nvidia0".into(),
                profile: "A2-4C".into(),
            }
        );
        assert_eq!(
            parse_vgpu_selector("nvidia0/3=A2-4C").unwrap(),
            VgpuSelector::Slice {
                name: "nvidia0/3".into(),
                profile: "A2-4C".into(),
            }
        );
        assert!(parse_vgpu_selector("nvidia0/3").is_err());
        assert!(family_of_name("nvidia0/3=A2-4C").is_none());
    }

    #[test]
    fn profile_selector_prefers_a_card_already_running_it() {
        let mut host_a = sample("nvidia0", "nvidia", "0000:af:00.0");
        host_a.role = "vgpu-host".into();
        host_a.assignable = false;
        host_a.blocked_reason = "vGPU host card; request a slice with --vgpu".into();
        let mut host_b = sample("nvidia1", "nvidia", "0000:c1:00.0");
        host_b.role = "vgpu-host".into();
        host_b.assignable = false;
        let mut running = sample("nvidia0/0", "nvidia", "0000:af:00.4");
        running.kind = "vgpu".into();
        running.role = "slice".into();
        running.parent = "nvidia0".into();
        running.profile = "A2-4C".into();
        running.profiles = vec!["A2-4C".into(), "A2-8C".into()];
        let mut free_on_running = sample("nvidia0/1", "nvidia", "0000:af:00.5");
        free_on_running.kind = "vgpu".into();
        free_on_running.role = "slice".into();
        free_on_running.parent = "nvidia0".into();
        free_on_running.profiles = vec!["A2-4C".into(), "A2-8C".into()];
        let mut empty = sample("nvidia1/0", "nvidia", "0000:c1:00.4");
        empty.kind = "vgpu".into();
        empty.role = "slice".into();
        empty.parent = "nvidia1".into();
        empty.profiles = vec!["A2-4C".into(), "A2-8C".into()];
        let mut locked = sample("nvidia1/1", "nvidia", "0000:c1:00.5");
        locked.kind = "vgpu".into();
        locked.role = "slice".into();
        locked.parent = "nvidia1".into();
        locked.profile = "A2-8C".into();
        locked.profiles = vec!["A2-4C".into(), "A2-8C".into()];
        let inventory = [host_a, host_b, running, free_on_running, empty, locked];
        let taken = HashSet::from(["nvidia0/0=A2-4C".to_string()]);
        let picked =
            select_vgpus(&inventory, &[VgpuSelector::Profile("A2-4C".into())], &taken).unwrap();
        assert_eq!(picked[0].0.name, "nvidia0/1");
        assert_eq!(picked[0].1, "A2-4C");
        let err = select_vgpus(
            &inventory,
            &[VgpuSelector::Card {
                card: "nvidia1".into(),
                profile: "A2-4C".into(),
            }],
            &taken,
        )
        .unwrap_err();
        assert!(err.contains("A2-8C"), "{err}");
        let refused = select_gpus(
            &inventory,
            &[GpuSelector::Exact("nvidia0".into())],
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(refused.contains("--vgpu"), "{refused}");
    }
}
