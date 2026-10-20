//! Detail and inventory pages. Lists stay on the existing screens; a name opens the rest.

use crate::api::{
    get_cluster_health_dto, get_container_dto, get_crl_summary_dto, get_crypto_config_dto,
    get_disk_layout_dto, get_network_dto, get_node_dto, get_object_store_dto, get_operation_dto,
    get_pki_status_dto, get_sbom_summaries_dto, get_shared_filesystem_dto, get_snapshot_dto,
    get_snapshot_policy_dto, get_ssh_key_dto, get_storage_class_dto, get_vm_detail_dto,
    get_volume_detail_dto, list_certificates_dto, list_containers_dto, list_disk_layouts_dto,
    list_nodes_dto, list_snapshot_policies_dto, list_snapshots_dto, list_ssh_keys_dto,
    list_storage_classes_dto,
};
use crate::dto::{
    CertificateRowDto, ClusterHealthDto, ContainerDetailDto, ContainerRowDto, CrlSummaryDto,
    CryptoConfigDto, DeviceDto, DiskLayoutDetailDto, DiskLayoutRowDto, MigrateNodeDto,
    NetworkRowDto, NodeDetailDto, NodeRowDto, ObjectStoreDetailDto, OperationDetailDto,
    PkiStatusDto, SbomSummaryDto, SharedFilesystemDetailDto, SnapshotPolicyDto, SnapshotRowDto,
    SshKeyDetailDto, SshKeyRowDto, StorageClassDetailDto, StorageClassDto, VmDetailDto, VmDiskDto,
    VmNicDto, VolumeDetailDto,
};
use leptos::prelude::*;
use leptos_router::hooks::{use_params_map, use_query_map};

fn dash(value: &str) -> String {
    if value.trim().is_empty() {
        "—".to_string()
    } else {
        value.to_string()
    }
}

fn yn(value: bool) -> &'static str {
    if value {
        "Yes"
    } else {
        "No"
    }
}

fn enc(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn badge(phase: &str) -> String {
    match phase.to_ascii_lowercase().as_str() {
        "ready" | "healthy" | "running" | "applied" | "active" => "badge badge-run".to_string(),
        "degraded" | "bootstrapping" | "scaling" | "pending" | "paused" | "created" => {
            "badge badge-warn".to_string()
        }
        _ => "badge badge-stop".to_string(),
    }
}

fn facts<L: Into<String>>(rows: Vec<(L, String)>) -> impl IntoView {
    let rows: Vec<(String, String)> = rows
        .into_iter()
        .map(|(label, value)| (label.into(), value))
        .collect();
    view! {
        <div class="card">
            <dl class="kv">
                {rows.into_iter().map(|(label, value)| view! {
                    <dt>{label}</dt><dd>{value}</dd>
                }).collect_view()}
            </dl>
        </div>
    }
}

fn load_error(
    back_label: &'static str,
    back: &'static str,
    title: &'static str,
    err: String,
) -> impl IntoView {
    view! {
        <section class="hero">
            <p class="crumb"><a href=back>{back_label}</a></p>
            <h1>{title}</h1>
        </section>
        <p class="err">{err}</p>
    }
}

#[component]
pub fn HealthBanner() -> impl IntoView {
    let res = Resource::new(|| (), |_| get_cluster_health_dto());
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Checking cluster health…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(health) => health_banner(health).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn health_banner(health: ClusterHealthDto) -> impl IntoView {
    let class = badge(&health.status);
    let headline = health.headline.clone();
    let first = health.conditions.first().cloned().unwrap_or_default();
    view! {
        <a href="/health" class="card health-banner" style="text-decoration: none; color: inherit; margin-bottom: 1.25rem; display: block;">
            <span class=class>{headline}</span>
            {if first.is_empty() {
                view! { <p class="muted" style="margin: 0.5rem 0 0;">"Open health for nodes, certificates, and replication."</p> }.into_any()
            } else {
                view! { <p style="margin: 0.5rem 0 0;">{first}</p> }.into_any()
            }}
        </a>
    }
}

#[component]
pub fn HealthPage() -> impl IntoView {
    let res = Resource::new(|| (), |_| get_cluster_health_dto());
    view! {
        <section class="hero">
            <h1>"Health"</h1>
            <p class="muted">"Whether the cluster can run workloads right now."</p>
        </section>
        <Suspense fallback=move || view! { <p class="muted">"Loading health…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(health) => health_view(health).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn health_view(health: ClusterHealthDto) -> impl IntoView {
    let class = badge(&health.status);
    let conditions = health.conditions.clone();
    view! {
        <p><span class=class>{health.headline.clone()}</span></p>
        <div class="stat-row">
            <a class="stat" href="/nodes"><div class="label">"Ready nodes"</div><div class="value">{health.ready_nodes}</div></a>
            <a class="stat" href="/nodes"><div class="label">"Not ready"</div><div class="value">{health.not_ready_nodes}</div></a>
            <a class="stat" href="/nodes"><div class="label">"Unschedulable"</div><div class="value">{health.unschedulable_nodes}</div></a>
            <a class="stat" href="/nodes"><div class="label">"Waiting for approval"</div><div class="value">{health.pending_nodes}</div></a>
            <a class="stat" href="/vms"><div class="label">"Virtual machines"</div><div class="value">{health.vm_count}</div></a>
            <a class="stat" href="/vms"><div class="label">"VMs on a not-ready node"</div><div class="value">{health.vms_on_not_ready_nodes}</div></a>
            <a class="stat" href="/compliance"><div class="label">"Replication conflicts"</div><div class="value">{health.replication_conflicts}</div></a>
            <a class="stat" href="/compliance/certificates"><div class="label">"Certificates expiring soon"</div><div class="value">{health.certificates_expiring_soon}</div></a>
            <a class="stat" href="/vms"><div class="label">"Host failures tolerated"</div><div class="value">{health.host_failures_tolerated}</div></a>
            <a class="stat" href="/vms"><div class="label">"Failover capacity"</div><div class="value">{dash(&health.failover_capacity)}</div></a>
            <a class="stat" href="/nodes"><div class="label">"HA lease holder"</div><div class="value">{dash(&health.ha_lease_holder)}</div></a>
            <a class="stat" href="/vms"><div class="label">"Unprotected VMs on lost nodes"</div><div class="value">{health.unprotected_on_lost_nodes}</div></a>
        </div>
        <h2 class="section-title">"What needs attention"</h2>
        {if conditions.is_empty() {
            view! { <div class="empty-note">"Nothing needs attention."</div> }.into_any()
        } else {
            view! {
                <div class="card">
                    <ul>
                        {conditions.into_iter().map(|item| view! { <li>{item}</li> }).collect_view()}
                    </ul>
                </div>
            }.into_any()
        }}
    }
}

#[component]
pub fn NodesPage() -> impl IntoView {
    let res = Resource::new(|| (), |_| list_nodes_dto());
    view! {
        <section class="hero">
            <h1>"Nodes"</h1>
            <p class="muted">"Machines that run virtual machines. Open a node for capacity, labels, and devices."</p>
        </section>
        <Suspense fallback=move || view! { <p class="muted">"Loading nodes…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(rows) => nodes_table(rows).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn nodes_table(rows: Vec<NodeRowDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty-note">"No nodes have registered."</div> }.into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead>
                    <tr>
                        <th>"Host"</th>
                        <th>"Status"</th>
                        <th>"Approval"</th>
                        <th>"CPU"</th>
                        <th>"Memory"</th>
                        <th>"Storage"</th>
                        <th>"Certificate"</th>
                    </tr>
                </thead>
                <tbody>
                    {rows.into_iter().map(|node| {
                        let href = format!("/nodes/{}", enc(&node.id));
                        let mark = badge(&node.status);
                        view! {
                            <tr>
                                <td><a class="cell-link" href=href.clone()><strong>{dash(&node.hostname)}</strong></a><div class="muted" style="font-size: 0.75rem;"><code class="inline">{node.id.clone()}</code></div></td>
                                <td><a class="cell-link" href=href.clone()><span class=mark>{node.status.clone()}</span></a></td>
                                <td><a class="cell-link" href=href.clone()>{dash(&node.approval)}</a></td>
                                <td><a class="cell-link" href=href.clone()>{node.cpu_used.clone()}" / "{node.cpu.clone()}</a></td>
                                <td><a class="cell-link" href=href.clone()>{node.memory_used.clone()}" / "{node.memory.clone()}</a></td>
                                <td><a class="cell-link" href=href.clone()>{node.backend.clone()}</a></td>
                                <td><a class="cell-link" href=href>{node.cert.clone()}</a></td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }
    .into_any()
}

#[component]
pub fn NodeDetailPage() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.get().get("id").unwrap_or_default());
    let res = Resource::new(move || id.get(), get_node_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading node…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => node_detail_view(detail).into_any(),
                    Err(e) => load_error("← Nodes", "/nodes", "Node", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn node_detail_view(detail: NodeDetailDto) -> impl IntoView {
    let node = detail.node;
    let devices = detail.devices;
    let devices_error = detail.devices_error;
    let mark = badge(&node.status);
    let href_class = format!(
        "/storage/classes/{}",
        enc(&node.backend.to_ascii_lowercase())
    );
    view! {
        <section class="hero">
            <p class="crumb"><a href="/nodes">"← Nodes"</a></p>
            <h1>{dash(&node.hostname)}</h1>
            <p class="muted">
                <span class=mark>{node.status.clone()}</span>
                " · "<code class="inline">{node.id.clone()}</code>
            </p>
        </section>
        {facts(vec![
            ("Address", dash(&node.address)),
            ("Datacenter", dash(&node.dc)),
            ("Approval", dash(&node.approval)),
            ("CPU in use", format!("{} of {} cores", node.cpu_used, node.cpu)),
            ("Memory in use", format!("{} of {}", node.memory_used, node.memory)),
            ("Storage", node.backend.clone()),
            ("Disk encryption", node.luks.clone()),
            ("Certificate", node.cert.clone()),
            ("Last heartbeat", node.heartbeat.clone()),
            ("Labels", node.labels.clone()),
            ("Overlay network", node.vxlan.clone()),
        ])}
        <p class="muted"><a href=href_class>"Nodes that use this storage type"</a></p>
        <h2 class="section-title">"Devices"</h2>
        {device_table(devices, devices_error)}
    }
}

fn device_table(devices: Vec<DeviceDto>, error: String) -> impl IntoView {
    if !error.is_empty() {
        return view! { <p class="err">{error}</p> }.into_any();
    }
    if devices.is_empty() {
        return view! { <div class="empty-note">"This node has not reported PCI devices."</div> }
            .into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"Kind"</th>
                        <th>"Role"</th>
                        <th>"Model"</th>
                        <th>"Address"</th>
                        <th>"Availability"</th>
                        <th>"Virtual machine"</th>
                    </tr>
                </thead>
                <tbody>
                    {devices.into_iter().map(|device| {
                        let why = if device.blocked_reason.is_empty() {
                            device.availability.clone()
                        } else {
                            format!("{} ({})", device.availability, device.blocked_reason)
                        };
                        view! {
                            <tr>
                                <td><strong>{device.name.clone()}</strong></td>
                                <td>{device.kind.clone()}</td>
                                <td>{device.role.clone()}</td>
                                <td>{dash(&device.model)}</td>
                                <td><code class="inline">{dash(&device.address)}</code></td>
                                <td>{why}</td>
                                <td>{dash(&device.assigned_vm)}</td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }
    .into_any()
}

#[component]
pub fn VmDetailPage() -> impl IntoView {
    let params = use_params_map();
    let name = Memo::new(move |_| params.get().get("name").unwrap_or_default());
    let res = Resource::new(move || name.get(), get_vm_detail_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading virtual machine…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => vm_detail_view(detail).into_any(),
                    Err(e) => load_error("← Virtual machines", "/vms", "Virtual machine", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn vm_detail_view(detail: VmDetailDto) -> impl IntoView {
    let console_name = if detail.name.is_empty() {
        detail.id.clone()
    } else {
        detail.name.clone()
    };
    let console = format!("/vms/{}/console", enc(&console_name));
    let mark = badge(&detail.state);
    let disks = detail.disks.clone();
    let nics = detail.nics.clone();
    let gpus = detail.gpus.clone();
    let pci = detail.pci.clone();
    let migrate = detail.migrate.clone();
    let migrate_error = detail.migrate_error.clone();
    view! {
        <section class="hero">
            <p class="crumb"><a href="/vms">"← Virtual machines"</a></p>
            <h1>{dash(&detail.name)}</h1>
            <p class="muted">
                <span class=mark>{detail.state.clone()}</span>
                " · wants "{detail.desired.clone()}
                " · "<code class="inline">{detail.id.clone()}</code>
            </p>
            <p><a href=console>"Open serial console"</a></p>
        </section>
        {facts(vec![
            ("Node", dash(&detail.node_id)),
            ("Address", dash(&detail.assigned_ip)),
            ("vCPU", detail.cpu.to_string()),
            ("Memory", detail.memory.clone()),
            ("Disk", detail.disk_size.clone()),
            ("Storage", detail.backend.clone()),
            ("HA", dash(&detail.ha_protection)),
            ("Restart priority", dash(&detail.restart_priority)),
            ("Replica node", dash(&detail.replica_node)),
            ("Guest monitoring", dash(&detail.vm_monitoring)),
            ("Created", detail.created.clone()),
            ("Updated", detail.updated.clone()),
        ])}
        <h2 class="section-title">"Disks"</h2>
        {vm_disks(disks)}
        <h2 class="section-title">"Network interfaces"</h2>
        {vm_nics(nics, detail.node_id.clone())}
        <h2 class="section-title">"Passed-through devices"</h2>
        {vm_devices(gpus, pci)}
        <h2 class="section-title">"Live migration"</h2>
        <p class="muted">"A receive session is a copy waiting on a node. An empty list means nothing is waiting."</p>
        {migrate_view(migrate, migrate_error)}
    }
}

fn vm_disks(disks: Vec<VmDiskDto>) -> impl IntoView {
    if disks.is_empty() {
        return view! { <div class="empty-note">"No extra disks. The root disk size is in the summary above."</div> }.into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Name"</th><th>"Path"</th><th>"Bus"</th><th>"Device"</th></tr></thead>
                <tbody>
                    {disks.into_iter().map(|disk| view! {
                        <tr>
                            <td>{dash(&disk.name)}</td>
                            <td><code class="inline">{dash(&disk.path)}</code></td>
                            <td>{dash(&disk.bus)}</td>
                            <td>{dash(&disk.device)}</td>
                        </tr>
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }.into_any()
}

fn vm_nics(nics: Vec<VmNicDto>, node_id: String) -> impl IntoView {
    if nics.is_empty() {
        return view! { <div class="empty-note">"No network interfaces recorded."</div> }
            .into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Network"</th><th>"Model"</th><th>"MAC"</th></tr></thead>
                <tbody>
                    {nics.into_iter().map(|nic| {
                        let href = if node_id.is_empty() || nic.network.is_empty() {
                            "/networks".to_string()
                        } else {
                            format!("/networks/{}/{}", enc(&node_id), enc(&nic.network))
                        };
                        view! {
                            <tr>
                                <td><a href=href>{dash(&nic.network)}</a></td>
                                <td>{dash(&nic.model)}</td>
                                <td><code class="inline">{dash(&nic.mac)}</code></td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }
    .into_any()
}

fn vm_devices(gpus: Vec<String>, pci: Vec<String>) -> impl IntoView {
    if gpus.is_empty() && pci.is_empty() {
        return view! { <div class="empty-note">"No GPU, NIC, NVMe, or raw PCI device is attached."</div> }.into_any();
    }
    view! {
        <div class="card">
            <dl class="kv">
                <dt>"Named devices"</dt><dd>{if gpus.is_empty() { "—".to_string() } else { gpus.join(", ") }}</dd>
                <dt>"PCI functions"</dt><dd>{if pci.is_empty() { "—".to_string() } else { pci.join(", ") }}</dd>
            </dl>
        </div>
    }.into_any()
}

fn migrate_view(nodes: Vec<MigrateNodeDto>, error: String) -> impl IntoView {
    if !error.is_empty() {
        return view! { <p class="err">{error}</p> }.into_any();
    }
    if nodes.is_empty() {
        return view! { <div class="empty-note">"No receive session reported."</div> }.into_any();
    }
    view! {
        <div class="card">
            <ul>
                {nodes.into_iter().map(|node| {
                    let line = if !node.reachable {
                        format!("{} could not be reached. {}", node.node_id, node.error)
                    } else if !node.has_session {
                        format!("{}: no receive session", node.node_id)
                    } else {
                        let owns = if node.owns_vm { " This node owns the virtual machine." } else { "" };
                        let summary = if node.summary.is_empty() { "session present".to_string() } else { node.summary.clone() };
                        format!("{}: {summary}. Port {}. VMM running: {}.{}", node.node_id, node.port, yn(node.vmm_alive), owns)
                    };
                    view! { <li>{line}</li> }
                }).collect_view()}
            </ul>
        </div>
    }.into_any()
}

#[component]
pub fn VolumeDetailPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let key = Memo::new(move |_| {
        (
            params.get().get("name").unwrap_or_default(),
            query.get().get("node").unwrap_or_default(),
        )
    });
    let res = Resource::new(
        move || key.get(),
        |(name, node)| get_volume_detail_dto(name, node),
    );
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading volume…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => volume_detail_view(detail).into_any(),
                    Err(e) => load_error("← Volumes", "/volumes", "Volume", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn volume_detail_view(detail: VolumeDetailDto) -> impl IntoView {
    let snaps = format!("/snapshots?volume={}", enc(&detail.name));
    let vm_href = if detail.vm.is_empty() {
        String::new()
    } else {
        format!("/vms/{}", enc(&detail.vm))
    };
    let note = detail.note.clone();
    view! {
        <section class="hero">
            <p class="crumb"><a href="/volumes">"← Volumes"</a></p>
            <h1>{detail.name.clone()}</h1>
            <p class="muted">{detail.backend.clone()}" · "{dash(&detail.node_id)}</p>
            <p><a href=snaps>"Snapshots of this volume"</a></p>
        </section>
        {if note.is_empty() {
            view! { <span></span> }.into_any()
        } else {
            view! { <p class="muted">{note}</p> }.into_any()
        }}
        {facts(vec![
            ("Role", dash(&detail.role)),
            ("Attached", dash(&detail.attach_state)),
            ("Size", detail.size.clone()),
            ("Guest sees", detail.guest_size.clone()),
            ("Virtual machine", dash(&detail.vm)),
            ("Encrypted", yn(detail.encrypted).to_string()),
            ("Pool", dash(&detail.pool)),
            ("Image", dash(&detail.image)),
            ("Format", dash(&detail.image_format)),
            ("Path", dash(&detail.path)),
            ("Filesystem", dash(&detail.fs_type)),
            ("Mount", dash(&detail.mount_point)),
            ("Serial", dash(&detail.serial)),
            ("Slot", dash(&detail.slot)),
            ("Id", dash(&detail.id)),
        ])}
        {if vm_href.is_empty() {
            view! { <span></span> }.into_any()
        } else {
            view! { <p><a href=vm_href>"Open the virtual machine"</a></p> }.into_any()
        }}
    }
}

#[component]
pub fn SnapshotsPage() -> impl IntoView {
    let query = use_query_map();
    let volume = Memo::new(move |_| query.get().get("volume").unwrap_or_default());
    let res = Resource::new(move || volume.get(), list_snapshots_dto);
    view! {
        <section class="hero">
            <p class="crumb"><a href="/volumes">"← Volumes"</a></p>
            <h1>"Snapshots"</h1>
            <p class="muted">"Point-in-time copies of a volume. "<a href="/snapshots/policies">"Snapshot schedules"</a></p>
        </section>
        <Suspense fallback=move || view! { <p class="muted">"Loading snapshots…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(rows) => snapshots_table(rows).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn snapshots_table(rows: Vec<SnapshotRowDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty-note">"No snapshots. Create one with "<code class="inline">"kctl create snapshot"</code>"."</div> }.into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Name"</th><th>"Volume"</th><th>"Size"</th><th>"Taken"</th><th>"Protected"</th><th>"Consistency"</th></tr></thead>
                <tbody>
                    {rows.into_iter().map(|snap| {
                        let href = format!("/snapshots/{}", enc(&snap.name));
                        let vol = format!("/volumes/{}", enc(&snap.volume_name));
                        view! {
                            <tr>
                                <td><a class="cell-link" href=href><strong>{snap.name.clone()}</strong></a></td>
                                <td><a href=vol>{dash(&snap.volume_name)}</a></td>
                                <td>{snap.size.clone()}</td>
                                <td>{snap.created.clone()}</td>
                                <td>{yn(snap.protected)}</td>
                                <td>{dash(&snap.consistency)}</td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }.into_any()
}

#[component]
pub fn SnapshotDetailPage() -> impl IntoView {
    let params = use_params_map();
    let name = Memo::new(move |_| params.get().get("name").unwrap_or_default());
    let res = Resource::new(move || name.get(), get_snapshot_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading snapshot…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(row) => {
                        let vol = format!("/volumes/{}", enc(&row.volume_name));
                        view! {
                            <section class="hero">
                                <p class="crumb"><a href="/snapshots">"← Snapshots"</a></p>
                                <h1>{row.name.clone()}</h1>
                            </section>
                            {facts(vec![
                                ("Volume", dash(&row.volume_name)),
                                ("Size", row.size.clone()),
                                ("Taken", row.created.clone()),
                                ("Protected", yn(row.protected).to_string()),
                                ("Consistency", dash(&row.consistency)),
                                ("Id", dash(&row.id)),
                            ])}
                            <p><a href=vol>"Open the volume"</a></p>
                        }.into_any()
                    }
                    Err(e) => load_error("← Snapshots", "/snapshots", "Snapshot", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

#[component]
pub fn SnapshotPoliciesPage() -> impl IntoView {
    let res = Resource::new(|| (), |_| list_snapshot_policies_dto());
    view! {
        <section class="hero">
            <p class="crumb"><a href="/volumes">"← Volumes"</a></p>
            <h1>"Snapshot schedules"</h1>
            <p class="muted">"How often kcore takes snapshots, and how many it keeps."</p>
        </section>
        <Suspense fallback=move || view! { <p class="muted">"Loading schedules…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(rows) => policies_table(rows).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn policies_table(rows: Vec<SnapshotPolicyDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty-note">"No snapshot schedules."</div> }.into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Name"</th><th>"Applies to"</th><th>"When"</th><th>"Keep"</th><th>"On"</th><th>"Last run"</th></tr></thead>
                <tbody>
                    {rows.into_iter().map(|policy| {
                        let href = format!("/snapshots/policies/{}", enc(&policy.name));
                        view! {
                            <tr>
                                <td><a class="cell-link" href=href.clone()><strong>{policy.name.clone()}</strong></a></td>
                                <td><a class="cell-link" href=href.clone()>{policy.applies_to.clone()}</a></td>
                                <td><a class="cell-link" href=href.clone()>{policy.schedule.clone()}</a></td>
                                <td><a class="cell-link" href=href.clone()>{policy.keep}</a></td>
                                <td><a class="cell-link" href=href.clone()>{yn(policy.enabled)}</a></td>
                                <td><a class="cell-link" href=href>{dash(&policy.last_run)}</a></td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }.into_any()
}

#[component]
pub fn SnapshotPolicyPage() -> impl IntoView {
    let params = use_params_map();
    let name = Memo::new(move |_| params.get().get("name").unwrap_or_default());
    let res = Resource::new(move || name.get(), get_snapshot_policy_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading schedule…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(policy) => view! {
                        <section class="hero">
                            <p class="crumb"><a href="/snapshots/policies">"← Snapshot schedules"</a></p>
                            <h1>{policy.name.clone()}</h1>
                        </section>
                        {facts(vec![
                            ("Applies to", policy.applies_to.clone()),
                            ("When", policy.schedule.clone()),
                            ("Keep", policy.keep.to_string()),
                            ("On", yn(policy.enabled).to_string()),
                            ("Last run", dash(&policy.last_run)),
                            ("Last result", dash(&policy.last_message)),
                        ])}
                    }.into_any(),
                    Err(e) => load_error("← Snapshot schedules", "/snapshots/policies", "Snapshot schedule", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

#[component]
pub fn FilesystemDetailPage() -> impl IntoView {
    let params = use_params_map();
    let name = Memo::new(move |_| params.get().get("name").unwrap_or_default());
    let res = Resource::new(move || name.get(), get_shared_filesystem_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading shared filesystem…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => filesystem_view(detail).into_any(),
                    Err(e) => load_error("← Ceph", "/ceph", "Shared filesystem", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn filesystem_view(detail: SharedFilesystemDetailDto) -> impl IntoView {
    let cluster = format!("/ceph/{}", enc(&detail.ceph_cluster));
    let clients = if detail.clients.is_empty() {
        "No clients".to_string()
    } else {
        detail.clients.join(", ")
    };
    let mark = badge(&detail.phase);
    view! {
        <section class="hero">
            <p class="crumb"><a href="/ceph">"← Ceph"</a></p>
            <h1>{detail.name.clone()}</h1>
            <p class="muted"><span class=mark>{detail.phase.clone()}</span>" · CephFS"</p>
        </section>
        {facts(vec![
            ("Cluster", dash(&detail.ceph_cluster)),
            ("Filesystem", detail.fs_name.clone()),
            ("Health", dash(&detail.health_message)),
            ("Quota", detail.quota.clone()),
            ("Generation", detail.generation.to_string()),
            ("Clients", clients),
        ])}
        <p><a href=cluster>"Open the Ceph cluster"</a></p>
    }
}

#[component]
pub fn ObjectStoreDetailPage() -> impl IntoView {
    let params = use_params_map();
    let name = Memo::new(move |_| params.get().get("name").unwrap_or_default());
    let res = Resource::new(move || name.get(), get_object_store_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading object store…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => object_store_view(detail).into_any(),
                    Err(e) => load_error("← Ceph", "/ceph", "Object store", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn object_store_view(detail: ObjectStoreDetailDto) -> impl IntoView {
    let cluster = format!("/ceph/{}", enc(&detail.ceph_cluster));
    let members = if detail.members.is_empty() {
        "—".to_string()
    } else {
        detail.members.join(", ")
    };
    let mark = badge(&detail.phase);
    view! {
        <section class="hero">
            <p class="crumb"><a href="/ceph">"← Ceph"</a></p>
            <h1>{detail.name.clone()}</h1>
            <p class="muted"><span class=mark>{detail.phase.clone()}</span>" · RGW"</p>
        </section>
        {facts(vec![
            ("Cluster", dash(&detail.ceph_cluster)),
            ("Instance", detail.rgw_id.clone()),
            ("Health", dash(&detail.health_message)),
            ("Port", detail.port.to_string()),
            ("TLS", yn(detail.tls).to_string()),
            ("Generation", detail.generation.to_string()),
            ("Members", members),
        ])}
        <p><a href=cluster>"Open the Ceph cluster"</a></p>
    }
}

#[component]
pub fn OperationDetailPage() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.get().get("id").unwrap_or_default());
    let res = Resource::new(move || id.get(), get_operation_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading operation…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => operation_view(detail).into_any(),
                    Err(e) => load_error("← Operations", "/operations", "Operation", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn operation_view(detail: OperationDetailDto) -> impl IntoView {
    let row = detail.row;
    let lines = detail.detail_lines;
    let vm = format!("/vms/{}", enc(&row.vm));
    view! {
        <section class="hero">
            <p class="crumb"><a href="/operations">"← Operations"</a></p>
            <h1>{dash(&row.kind)}</h1>
            <p class="muted"><code class="inline">{row.id.clone()}</code>" · "{dash(&row.phase)}</p>
        </section>
        {facts(vec![
            ("Virtual machine", dash(&row.vm)),
            ("From", dash(&row.source_node)),
            ("To", dash(&row.target_node)),
            ("Started", row.started_at.clone()),
            ("Updated", detail.updated_at.clone()),
            ("Finished", row.finished_at.clone()),
            ("Data sent", yn(row.send_succeeded).to_string()),
            ("Cancel requested", yn(row.cancel_requested).to_string()),
        ])}
        <p><a href=vm>"Open the virtual machine"</a></p>
        <h2 class="section-title">"Details"</h2>
        {if lines.is_empty() {
            view! { <div class="empty-note">"No extra detail."</div> }.into_any()
        } else {
            facts(lines).into_any()
        }}
    }
}

#[component]
pub fn NetworkDetailPage() -> impl IntoView {
    let params = use_params_map();
    let key = Memo::new(move |_| {
        (
            params.get().get("node").unwrap_or_default(),
            params.get().get("name").unwrap_or_default(),
        )
    });
    let res = Resource::new(
        move || key.get(),
        |(node, name)| get_network_dto(node, name),
    );
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading network…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(row) => network_view(row).into_any(),
                    Err(e) => load_error("← Networks", "/networks", "Network", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn network_view(row: NetworkRowDto) -> impl IntoView {
    let kind = match row.network_type.as_str() {
        "nat" => "NAT",
        "bridge" => "Bridge",
        "vxlan" => "VXLAN overlay",
        other => other,
    };
    let nat = if row.enable_outbound_nat {
        "Virtual machines can reach the internet"
    } else {
        "Virtual machines stay on this network"
    };
    let east = if row.east_west_firewall {
        "Virtual machines cannot talk to each other"
    } else {
        "Virtual machines can talk to each other"
    };
    let vlan = if row.vlan_id <= 0 {
        "None".to_string()
    } else {
        row.vlan_id.to_string()
    };
    let node = format!("/nodes/{}", enc(&row.node_id));
    view! {
        <section class="hero">
            <p class="crumb"><a href="/networks">"← Networks"</a></p>
            <h1>{row.name.clone()}</h1>
            <p class="muted">{kind.to_string()}" on "<code class="inline">{dash(&row.node_id)}</code></p>
        </section>
        {facts(vec![
            ("Type", kind.to_string()),
            ("External address", dash(&row.external_ip)),
            ("Gateway", dash(&row.gateway_ip)),
            ("Netmask", dash(&row.internal_netmask)),
            ("IPv6", dash(&row.ipv6)),
            ("VLAN", vlan),
            ("Internet", nat.to_string()),
            ("Between VMs", east.to_string()),
            ("Forwarded ports", dash(&row.forwarded_ports)),
        ])}
        <p><a href=node>"Open the node"</a></p>
    }
}

#[component]
pub fn ContainersPage() -> impl IntoView {
    let res = Resource::new(|| (), |_| list_containers_dto());
    view! {
        <section class="hero">
            <h1>"Containers"</h1>
            <p class="muted">"Containers running on cluster nodes."</p>
        </section>
        <Suspense fallback=move || view! { <p class="muted">"Loading containers…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(rows) => containers_table(rows).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn containers_table(rows: Vec<ContainerRowDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty-note">"No containers. Create one with "<code class="inline">"kctl create container"</code>"."</div> }.into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Name"</th><th>"Image"</th><th>"State"</th><th>"Node"</th><th>"Status"</th></tr></thead>
                <tbody>
                    {rows.into_iter().map(|row| {
                        let href = format!("/containers/{}?node={}", enc(&row.name), enc(&row.node_id));
                        let mark = badge(&row.state);
                        view! {
                            <tr>
                                <td><a class="cell-link" href=href.clone()><strong>{dash(&row.name)}</strong></a></td>
                                <td><a class="cell-link" href=href.clone()><code class="inline">{dash(&row.image)}</code></a></td>
                                <td><a class="cell-link" href=href.clone()><span class=mark>{row.state.clone()}</span></a></td>
                                <td><a class="cell-link" href=href.clone()>{dash(&row.node_id)}</a></td>
                                <td><a class="cell-link" href=href>{dash(&row.status)}</a></td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }.into_any()
}

#[component]
pub fn ContainerDetailPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let key = Memo::new(move |_| {
        (
            params.get().get("name").unwrap_or_default(),
            query.get().get("node").unwrap_or_default(),
        )
    });
    let res = Resource::new(
        move || key.get(),
        |(name, node)| get_container_dto(name, node),
    );
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading container…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => container_view(detail).into_any(),
                    Err(e) => load_error("← Containers", "/containers", "Container", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn container_view(detail: ContainerDetailDto) -> impl IntoView {
    let env = if detail.env.is_empty() {
        "—".to_string()
    } else {
        detail.env.join("\n")
    };
    let mark = badge(&detail.state);
    view! {
        <section class="hero">
            <p class="crumb"><a href="/containers">"← Containers"</a></p>
            <h1>{dash(&detail.name)}</h1>
            <p class="muted"><span class=mark>{detail.state.clone()}</span>" · wants "{detail.desired.clone()}</p>
        </section>
        {facts(vec![
            ("Image", dash(&detail.image)),
            ("Status", dash(&detail.status)),
            ("Node", dash(&detail.node_id)),
            ("Address", dash(&detail.assigned_ip)),
            ("Network", dash(&detail.network)),
            ("Command", dash(&detail.command)),
            ("Ports", dash(&detail.ports)),
            ("Storage", detail.backend.clone()),
            ("Disk", detail.disk_size.clone()),
            ("Mount", dash(&detail.mount)),
            ("Created", detail.created.clone()),
            ("Environment", env),
        ])}
    }
}

#[component]
pub fn StorageClassesSection() -> impl IntoView {
    let res = Resource::new(|| (), |_| list_storage_classes_dto());
    view! {
        <h2 id="classes" class="section-title">"Storage classes"</h2>
        <p class="muted">"Where virtual machine disks live on each node."</p>
        <Suspense fallback=move || view! { <p class="muted">"Loading storage classes…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(rows) => classes_table(rows).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn classes_table(rows: Vec<StorageClassDto>) -> impl IntoView {
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Class"</th><th>"What it means"</th><th>"Nodes"</th></tr></thead>
                <tbody>
                    {rows.into_iter().map(|row| {
                        let href = format!("/storage/classes/{}", enc(&row.name));
                        view! {
                            <tr>
                                <td><a class="cell-link" href=href.clone()><strong>{row.name.clone()}</strong></a></td>
                                <td><a class="cell-link" href=href.clone()>{row.blurb.clone()}</a></td>
                                <td><a class="cell-link" href=href>{row.nodes}</a></td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }
}

#[component]
pub fn StorageClassPage() -> impl IntoView {
    let params = use_params_map();
    let name = Memo::new(move |_| params.get().get("name").unwrap_or_default());
    let res = Resource::new(move || name.get(), get_storage_class_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading storage class…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => class_view(detail).into_any(),
                    Err(e) => load_error("← Storage", "/storage", "Storage class", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn class_view(detail: StorageClassDetailDto) -> impl IntoView {
    let nodes = detail.nodes;
    view! {
        <section class="hero">
            <p class="crumb"><a href="/storage#classes">"← Storage"</a></p>
            <h1>{detail.name.clone()}</h1>
            <p class="muted">{detail.blurb.clone()}</p>
        </section>
        {if nodes.is_empty() {
            view! { <div class="empty-note">"No nodes use this storage type."</div> }.into_any()
        } else {
            nodes_table(nodes).into_any()
        }}
    }
}

#[component]
pub fn DiskLayoutsSection() -> impl IntoView {
    let res = Resource::new(|| (), |_| list_disk_layouts_dto());
    view! {
        <h2 id="layouts" class="section-title">"Disk layouts"</h2>
        <p class="muted">"How a node's disks are partitioned. ZFS datasets are not scanned here; registered ZFS volumes are on "<a href="/volumes">"Volumes"</a>"."</p>
        <Suspense fallback=move || view! { <p class="muted">"Loading disk layouts…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(rows) => layouts_table(rows).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn layouts_table(rows: Vec<DiskLayoutRowDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty-note">"No disk layouts."</div> }.into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Name"</th><th>"Node"</th><th>"Phase"</th><th>"Generation"</th><th>"Evacuate first"</th><th>"Message"</th></tr></thead>
                <tbody>
                    {rows.into_iter().map(|row| {
                        let href = format!("/storage/layouts/{}", enc(&row.name));
                        let mark = badge(&row.phase);
                        view! {
                            <tr>
                                <td><a class="cell-link" href=href.clone()><strong>{row.name.clone()}</strong></a></td>
                                <td><a href=format!("/nodes/{}", enc(&row.node_id))>{dash(&row.node_id)}</a></td>
                                <td><span class=mark>{row.phase.clone()}</span></td>
                                <td>{row.observed_generation}" / "{row.generation}</td>
                                <td>{yn(row.evacuate)}</td>
                                <td><a href=href>{dash(&row.message)}</a></td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }.into_any()
}

#[component]
pub fn DiskLayoutPage() -> impl IntoView {
    let params = use_params_map();
    let name = Memo::new(move |_| params.get().get("name").unwrap_or_default());
    let res = Resource::new(move || name.get(), get_disk_layout_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading disk layout…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => layout_view(detail).into_any(),
                    Err(e) => load_error("← Storage", "/storage#layouts", "Disk layout", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn layout_view(detail: DiskLayoutDetailDto) -> impl IntoView {
    let row = detail.row;
    let nix = detail.layout_nix;
    let mark = badge(&row.phase);
    view! {
        <section class="hero">
            <p class="crumb"><a href="/storage#layouts">"← Storage"</a></p>
            <h1>{row.name.clone()}</h1>
            <p class="muted"><span class=mark>{row.phase.clone()}</span></p>
        </section>
        {facts(vec![
            ("Node", dash(&row.node_id)),
            ("Generation", format!("{} observed, {} requested", row.observed_generation, row.generation)),
            ("Move VMs off the node first", yn(row.evacuate).to_string()),
            ("Message", dash(&row.message)),
            ("Refusal", dash(&detail.refusal_reason)),
            ("Updated", detail.updated.clone()),
        ])}
        <h2 class="section-title">"Layout"</h2>
        <pre class="code-block">{nix}</pre>
    }
}

#[component]
pub fn CertificatesPage() -> impl IntoView {
    let certs = Resource::new(|| (), |_| list_certificates_dto());
    let pki = Resource::new(|| (), |_| get_pki_status_dto());
    let crl = Resource::new(|| (), |_| get_crl_summary_dto());
    view! {
        <section class="hero">
            <p class="crumb"><a href="/compliance">"← Compliance"</a></p>
            <h1>"Certificates"</h1>
            <p class="muted">"Who the cluster trusts, and when those certificates expire."</p>
        </section>
        <Suspense fallback=move || view! { <p class="muted">"Loading certificate health…"</p> }>
            {move || Suspend::new(async move {
                match pki.await {
                    Ok(status) => pki_facts(status).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
        <Suspense fallback=move || view! { <p class="muted">"Loading revocation list…"</p> }>
            {move || Suspend::new(async move {
                match crl.await {
                    Ok(summary) => crl_facts(summary).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
        <h2 class="section-title">"Inventory"</h2>
        <Suspense fallback=move || view! { <p class="muted">"Loading certificates…"</p> }>
            {move || Suspend::new(async move {
                match certs.await {
                    Ok(rows) => certificates_table(rows).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn pki_facts(status: PkiStatusDto) -> impl IntoView {
    view! {
        <div class="stat-row">
            <div class="stat"><div class="label">"Active"</div><div class="value">{status.active}</div></div>
            <div class="stat"><div class="label">"Rotated"</div><div class="value">{status.rotated}</div></div>
            <div class="stat"><div class="label">"Revoked"</div><div class="value">{status.revoked}</div></div>
            <div class="stat"><div class="label">"Expired"</div><div class="value">{status.expired}</div></div>
            <div class="stat"><div class="label">"Expiring soon"</div><div class="value">{status.expiring_soon}</div></div>
        </div>
        {facts(vec![
            ("Warn before", format!("{} days", status.warn_before_days)),
            ("Renew before", format!("{} days", status.renew_before_days)),
            ("Automatic renewal", yn(status.rotation_enabled).to_string()),
            ("If revocation cannot be checked", dash(&status.revocation_mode)),
            ("Sub-CA present", yn(status.sub_ca_available).to_string()),
            ("Revocation list number", status.crl_number.to_string()),
            ("PKI address", dash(&status.pki_url)),
        ])}
    }
}

fn crl_facts(summary: CrlSummaryDto) -> impl IntoView {
    view! {
        <h2 class="section-title">"Revocation list"</h2>
        {facts(vec![
            ("Published", yn(summary.available).to_string()),
            ("Number", summary.number.to_string()),
            ("Revoked certificates", summary.revoked.to_string()),
            ("This update", summary.this_update.clone()),
            ("Next update", summary.next_update.clone()),
            ("Note", dash(&summary.message)),
        ])}
    }
}

fn certificates_table(rows: Vec<CertificateRowDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty-note">"No certificates in the inventory."</div> }
            .into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Subject"</th><th>"Kind"</th><th>"Node"</th><th>"Status"</th><th>"Expires"</th><th>"Reason"</th><th>"Serial"</th></tr></thead>
                <tbody>
                    {rows.into_iter().map(|row| {
                        let node = if row.node_id.is_empty() {
                            view! { <span>"—"</span> }.into_any()
                        } else {
                            let href = format!("/nodes/{}", enc(&row.node_id));
                            view! { <a href=href>{row.node_id.clone()}</a> }.into_any()
                        };
                        view! {
                            <tr>
                                <td><strong>{dash(&row.subject)}</strong></td>
                                <td>{row.kind.clone()}</td>
                                <td>{node}</td>
                                <td>{row.status.clone()}</td>
                                <td>{row.expires.clone()}</td>
                                <td>{dash(&row.reason)}</td>
                                <td><code class="inline">{dash(&row.serial)}</code></td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }.into_any()
}

#[component]
pub fn SshKeysPage() -> impl IntoView {
    let res = Resource::new(|| (), |_| list_ssh_keys_dto());
    view! {
        <section class="hero">
            <p class="crumb"><a href="/compliance">"← Compliance"</a></p>
            <h1>"SSH keys"</h1>
            <p class="muted">"Public keys kcore can place in a virtual machine."</p>
        </section>
        <Suspense fallback=move || view! { <p class="muted">"Loading SSH keys…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(rows) => ssh_table(rows).into_any(),
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

fn ssh_table(rows: Vec<SshKeyRowDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty-note">"No SSH keys. Add one with "<code class="inline">"kctl create ssh-key"</code>"."</div> }.into_any();
    }
    view! {
        <div class="table-wrap">
            <table class="data">
                <thead><tr><th>"Name"</th><th>"Added"</th></tr></thead>
                <tbody>
                    {rows.into_iter().map(|row| {
                        let href = format!("/ssh-keys/{}", enc(&row.name));
                        view! {
                            <tr>
                                <td><a class="cell-link" href=href.clone()><strong>{row.name.clone()}</strong></a></td>
                                <td><a class="cell-link" href=href>{dash(&row.created)}</a></td>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }.into_any()
}

#[component]
pub fn SshKeyPage() -> impl IntoView {
    let params = use_params_map();
    let name = Memo::new(move |_| params.get().get("name").unwrap_or_default());
    let res = Resource::new(move || name.get(), get_ssh_key_dto);
    view! {
        <Suspense fallback=move || view! { <p class="muted">"Loading SSH key…"</p> }>
            {move || Suspend::new(async move {
                match res.await {
                    Ok(detail) => ssh_view(detail).into_any(),
                    Err(e) => load_error("← SSH keys", "/ssh-keys", "SSH key", e.to_string()).into_any(),
                }
            })}
        </Suspense>
    }
}

fn ssh_view(detail: SshKeyDetailDto) -> impl IntoView {
    view! {
        <section class="hero">
            <p class="crumb"><a href="/ssh-keys">"← SSH keys"</a></p>
            <h1>{detail.name.clone()}</h1>
            <p class="muted">"Added "{dash(&detail.created)}</p>
        </section>
        <h2 class="section-title">"Public key"</h2>
        <pre class="code-block">{detail.public_key}</pre>
    }
}

#[component]
pub fn CryptoCard() -> impl IntoView {
    let res = Resource::new(|| (), |_| get_crypto_config_dto());
    view! {
        <section class="card" style="margin-top: 1rem;">
            <h2>"Release and limits"</h2>
            <Suspense fallback=move || view! { <p class="muted">"Loading cryptographic configuration…"</p> }>
                {move || Suspend::new(async move {
                    match res.await {
                        Ok(cfg) => crypto_facts(cfg).into_any(),
                        Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                    }
                })}
            </Suspense>
        </section>
    }
}

fn crypto_facts(cfg: CryptoConfigDto) -> impl IntoView {
    facts(vec![
        ("FIPS certificate", dash(&cfg.fips_certificate)),
        ("Request limit", cfg.rate_limit.clone()),
        (
            "If revocation cannot be checked",
            dash(&cfg.revocation_mode),
        ),
        ("Release signing", dash(&cfg.signing_scheme)),
    ])
}

#[component]
pub fn SbomSection() -> impl IntoView {
    let res = Resource::new(|| (), |_| get_sbom_summaries_dto());
    view! {
        <section class="card" id="sbom" style="margin-top: 1rem;">
            <h2>"Software bill of materials"</h2>
            <p class="muted">"What this controller was built from. The full document stays in "<code class="inline">"kctl get sbom -o"</code>"."</p>
            <Suspense fallback=move || view! { <p class="muted">"Loading software inventory…"</p> }>
                {move || Suspend::new(async move {
                    match res.await {
                        Ok(rows) => view! {
                            {rows.into_iter().map(sbom_card).collect_view()}
                        }.into_any(),
                        Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                    }
                })}
            </Suspense>
        </section>
    }
}

fn sbom_card(row: SbomSummaryDto) -> impl IntoView {
    if !row.error.is_empty() {
        return view! {
            <h3>{row.title.clone()}</h3>
            <p class="err">{row.error.clone()}</p>
        }
        .into_any();
    }
    view! {
        <h3>{row.title.clone()}</h3>
        {facts(vec![
            ("Format", dash(&row.format)),
            ("Spec", dash(&row.spec_version)),
            ("File", dash(&row.filename)),
            ("SHA-256", dash(&row.sha256)),
            ("Source", dash(&row.source)),
            ("Generator", dash(&row.generator)),
            ("Signed", yn(row.signed).to_string()),
            ("Signing", dash(&row.signing_scheme)),
        ])}
    }
    .into_any()
}

#[component]
pub fn SshKeysPreview() -> impl IntoView {
    let res = Resource::new(|| (), |_| list_ssh_keys_dto());
    view! {
        <section class="card" style="margin-top: 1rem;">
            <h2>"SSH keys"</h2>
            <p class="muted"><a href="/ssh-keys">"Open the key list"</a></p>
            <Suspense fallback=move || view! { <p class="muted">"Loading SSH keys…"</p> }>
                {move || Suspend::new(async move {
                    match res.await {
                        Ok(rows) => ssh_table(rows).into_any(),
                        Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                    }
                })}
            </Suspense>
        </section>
    }
}
