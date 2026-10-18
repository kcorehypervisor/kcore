use anyhow::{anyhow, bail, Result};

use crate::client;
use crate::config::ConnectionInfo;

pub async fn list(info: &ConnectionInfo, node: Option<&str>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_gpus(client::controller_proto::ListGpusRequest {
            node_id: node.unwrap_or_default().to_string(),
        })
        .await?
        .into_inner();
    if resp.gpus.is_empty() {
        println!("No GPUs reported. Node agents publish inventory on heartbeat.");
        return Ok(());
    }
    print_gpu_table(&finish(resp.gpus.iter().map(proto_to_view).collect()));
    Ok(())
}

pub async fn list_pci(info: &ConnectionInfo, node: Option<&str>, kind: Option<&str>) -> Result<()> {
    let kind = normalize_kind(kind)?;
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_pci_devices(client::controller_proto::ListGpusRequest {
            node_id: node.unwrap_or_default().to_string(),
        })
        .await?
        .into_inner();
    let rows = finish(
        resp.gpus
            .iter()
            .map(proto_to_view)
            .filter(|row| kind.as_deref().is_none_or(|want| row.kind == want))
            .collect(),
    );
    if rows.is_empty() {
        println!("No PCI devices reported. Node agents publish inventory on heartbeat.");
        return Ok(());
    }
    print_pci_table(&rows);
    Ok(())
}

fn scan_local() -> Result<Vec<kcore_gpu::GpuDevice>> {
    let hosts = kcore_gpu::load_vgpu_hosts(std::path::Path::new("/etc/kcore/vgpu-hosts"));
    kcore_gpu::scan_pci_with(std::path::Path::new("/sys/bus/pci/devices"), &hosts)
        .map_err(|err| anyhow!(err))
}

pub fn list_local() -> Result<()> {
    let gpus = scan_local()?
        .into_iter()
        .filter(|dev| dev.kind == "gpu" || dev.kind == "vgpu")
        .collect::<Vec<_>>();
    if gpus.is_empty() {
        println!("No display controllers or accelerators on this machine.");
        return Ok(());
    }
    print_gpu_table(&finish(gpus.iter().map(local_to_view).collect()));
    Ok(())
}

pub fn list_pci_local(kind: Option<&str>) -> Result<()> {
    let kind = normalize_kind(kind)?;
    let devices = scan_local()?;
    let rows = finish(
        devices
            .iter()
            .map(local_to_view)
            .filter(|row| kind.as_deref().is_none_or(|want| row.kind == want))
            .collect(),
    );
    if rows.is_empty() {
        println!("No matching PCI devices on this machine.");
        return Ok(());
    }
    print_pci_table(&rows);
    Ok(())
}

fn normalize_kind(kind: Option<&str>) -> Result<Option<String>> {
    let Some(kind) = kind.map(str::trim).filter(|kind| !kind.is_empty()) else {
        return Ok(None);
    };
    let kind = kind.to_ascii_lowercase();
    if !matches!(kind.as_str(), "gpu" | "vgpu" | "nic" | "nvme" | "raw") {
        bail!("--type must be gpu, vgpu, nic, nvme, or raw");
    }
    Ok(Some(kind))
}

struct GpuView {
    node: String,
    kind: String,
    name: String,
    role: String,
    model: String,
    address: String,
    class_code: String,
    group: i32,
    state: String,
    detail: String,
    parent: String,
    profile: String,
    profiles: String,
    characteristics: String,
}

fn proto_to_view(gpu: &client::controller_proto::GpuInfo) -> GpuView {
    let (state, detail) = state_detail(
        &gpu.assigned_vm,
        gpu.assignable,
        &gpu.blocked_reason,
        &gpu.characteristics,
    );
    GpuView {
        node: gpu.node_id.clone(),
        kind: if gpu.kind.is_empty() {
            "gpu".into()
        } else {
            gpu.kind.clone()
        },
        name: gpu.name.clone(),
        role: gpu.role.clone(),
        model: gpu.model.clone(),
        address: gpu.address.clone(),
        class_code: gpu.class_code.clone(),
        group: gpu.iommu_group,
        state,
        detail,
        parent: gpu.parent.clone(),
        profile: gpu.profile.clone(),
        profiles: gpu.profiles.join(","),
        characteristics: gpu.characteristics.clone(),
    }
}

fn local_to_view(gpu: &kcore_gpu::GpuDevice) -> GpuView {
    let (state, detail) = state_detail(
        "",
        gpu.assignable,
        &gpu.blocked_reason,
        &gpu.characteristics,
    );
    GpuView {
        node: "local".into(),
        kind: gpu.kind.clone(),
        name: gpu.name.clone(),
        role: gpu.role.clone(),
        model: gpu.model.clone(),
        address: gpu.address.clone(),
        class_code: gpu.class_code.clone(),
        group: gpu.iommu_group,
        state,
        detail,
        parent: gpu.parent.clone(),
        profile: gpu.profile.clone(),
        profiles: gpu.profiles.join(","),
        characteristics: gpu.characteristics.clone(),
    }
}

fn finish(mut rows: Vec<GpuView>) -> Vec<GpuView> {
    apply_vgpu_detail(&mut rows);
    rows
}

fn apply_vgpu_detail(rows: &mut [GpuView]) {
    let slices: Vec<(String, String, bool)> = rows
        .iter()
        .filter(|row| row.kind == "vgpu")
        .map(|row| {
            (
                row.parent.clone(),
                row.profile.clone(),
                row.state == "in-use",
            )
        })
        .collect();
    for row in rows.iter_mut() {
        if row.role == "vgpu-host" {
            let created = slices
                .iter()
                .filter(|(parent, _, _)| parent == &row.name)
                .count();
            let used = slices
                .iter()
                .filter(|(parent, _, in_use)| parent == &row.name && *in_use)
                .count();
            let mut profiles: Vec<&str> = slices
                .iter()
                .filter(|(parent, profile, _)| parent == &row.name && !profile.is_empty())
                .map(|(_, profile, _)| profile.as_str())
                .collect();
            profiles.sort_unstable();
            profiles.dedup();
            let vfs = vfs_count(&row.characteristics).unwrap_or(created as u32);
            let mut detail = format!("vGPU host: {vfs} VFs, slices={used}/{created}");
            if !profiles.is_empty() {
                detail.push_str(" profile=");
                detail.push_str(&profiles.join(","));
            }
            row.state = "host".into();
            row.detail = detail;
        } else if row.kind == "vgpu" && row.state == "in-use" && !row.profile.is_empty() {
            row.detail = format!("{} profile={}", row.detail, row.profile);
        } else if row.kind == "vgpu" && row.state == "free" {
            let parent = if row.parent.is_empty() {
                "-".to_string()
            } else {
                row.parent.clone()
            };
            let profiles = if row.profiles.is_empty() {
                "-".to_string()
            } else {
                row.profiles.clone()
            };
            row.detail = format!("parent={parent} profiles={profiles}");
        }
    }
}

fn vfs_count(characteristics: &str) -> Option<u32> {
    characteristics.split_whitespace().find_map(|part| {
        let value = part.strip_prefix("vfs=")?;
        value.parse().ok()
    })
}

fn state_detail(
    assigned_vm: &str,
    assignable: bool,
    blocked_reason: &str,
    characteristics: &str,
) -> (String, String) {
    if !assigned_vm.is_empty() {
        ("in-use".into(), format!("vm {assigned_vm}"))
    } else if assignable {
        ("free".into(), characteristics.to_string())
    } else {
        let detail = if blocked_reason.is_empty() {
            characteristics.to_string()
        } else if characteristics.is_empty() {
            blocked_reason.to_string()
        } else {
            format!("{blocked_reason}; {characteristics}")
        };
        ("blocked".into(), detail)
    }
}

fn print_gpu_table(rows: &[GpuView]) {
    println!(
        "{:<16}  {:<10}  {:<10}  {:<16}  {:<14}  {:>5}  {:<8}  DETAIL",
        "NODE", "GPU", "ROLE", "MODEL", "ADDRESS", "GROUP", "STATE"
    );
    for row in rows {
        println!(
            "{:<16}  {:<10}  {:<10}  {:<16}  {:<14}  {:>5}  {:<8}  {}",
            truncate(&row.node, 16),
            row.name,
            if row.role.is_empty() { "-" } else { &row.role },
            truncate(&row.model, 16),
            row.address,
            group_cell(row.group),
            row.state,
            row.detail
        );
    }
}

fn print_pci_table(rows: &[GpuView]) {
    println!(
        "{:<16}  {:<6}  {:<14}  {:<10}  {:<10}  {:<14}  {:>5}  {:<8}  DETAIL",
        "NODE", "KIND", "NAME", "ROLE", "CLASS", "ADDRESS", "GROUP", "STATE"
    );
    for row in rows {
        println!(
            "{:<16}  {:<6}  {:<14}  {:<10}  {:<10}  {:<14}  {:>5}  {:<8}  {}",
            truncate(&row.node, 16),
            row.kind,
            truncate(&row.name, 14),
            if row.role.is_empty() { "-" } else { &row.role },
            if row.class_code.is_empty() {
                "-"
            } else {
                &row.class_code
            },
            row.address,
            group_cell(row.group),
            row.state,
            row.detail
        );
    }
}

fn group_cell(group: i32) -> String {
    if group < 0 {
        "-".into()
    } else {
        group.to_string()
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        kind: &str,
        name: &str,
        role: &str,
        state: &str,
        parent: &str,
        profile: &str,
        profiles: &str,
        characteristics: &str,
    ) -> GpuView {
        GpuView {
            node: "node-a".into(),
            kind: kind.into(),
            name: name.into(),
            role: role.into(),
            model: String::new(),
            address: String::new(),
            class_code: String::new(),
            group: 0,
            state: state.into(),
            detail: if state == "in-use" {
                "vm ai-1".into()
            } else {
                String::new()
            },
            parent: parent.into(),
            profile: profile.into(),
            profiles: profiles.into(),
            characteristics: characteristics.into(),
        }
    }

    #[test]
    fn vgpu_table_detail_names_the_host_and_the_slice() {
        let mut rows = vec![
            row(
                "gpu",
                "nvidia0",
                "vgpu-host",
                "blocked",
                "",
                "",
                "",
                "sriov=pf vfs=16",
            ),
            row(
                "vgpu",
                "nvidia0/0",
                "slice",
                "in-use",
                "nvidia0",
                "A2-4C",
                "A2-4C",
                "",
            ),
            row(
                "vgpu",
                "nvidia0/1",
                "slice",
                "free",
                "nvidia0",
                "",
                "A2-4C,A2-8C",
                "",
            ),
        ];
        apply_vgpu_detail(&mut rows);
        assert_eq!(rows[0].state, "host");
        assert_eq!(
            rows[0].detail,
            "vGPU host: 16 VFs, slices=1/2 profile=A2-4C"
        );
        assert_eq!(rows[1].detail, "vm ai-1 profile=A2-4C");
        assert_eq!(rows[2].detail, "parent=nvidia0 profiles=A2-4C,A2-8C");
    }
}
