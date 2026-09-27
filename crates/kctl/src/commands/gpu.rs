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
    print_gpu_table(&resp.gpus.iter().map(proto_to_view).collect::<Vec<_>>());
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
    let rows: Vec<GpuView> = resp
        .gpus
        .iter()
        .map(proto_to_view)
        .filter(|row| kind.as_deref().is_none_or(|want| row.kind == want))
        .collect();
    if rows.is_empty() {
        println!("No PCI devices reported. Node agents publish inventory on heartbeat.");
        return Ok(());
    }
    print_pci_table(&rows);
    Ok(())
}

pub fn list_local() -> Result<()> {
    let gpus = kcore_gpu::scan_sysfs(std::path::Path::new("/sys/bus/pci/devices"))
        .map_err(|err| anyhow!(err))?;
    if gpus.is_empty() {
        println!("No display controllers or accelerators on this machine.");
        return Ok(());
    }
    print_gpu_table(&gpus.iter().map(local_to_view).collect::<Vec<_>>());
    Ok(())
}

pub fn list_pci_local(kind: Option<&str>) -> Result<()> {
    let kind = normalize_kind(kind)?;
    let devices = kcore_gpu::scan_pci(std::path::Path::new("/sys/bus/pci/devices"))
        .map_err(|err| anyhow!(err))?;
    let rows: Vec<GpuView> = devices
        .iter()
        .map(local_to_view)
        .filter(|row| kind.as_deref().is_none_or(|want| row.kind == want))
        .collect();
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
    if !matches!(kind.as_str(), "gpu" | "nic" | "nvme" | "raw") {
        bail!("--type must be gpu, nic, nvme, or raw");
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
    }
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
