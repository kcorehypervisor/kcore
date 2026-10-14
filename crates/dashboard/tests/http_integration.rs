//! End-to-end HTTP tests: Axum + Leptos SSR + server functions against a mock gRPC controller.
//!
//! Uses `OnceLock` for config, so all assertions live in a single `#[tokio::test]`.

mod support;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use kcore_dashboard::app_server::dashboard_router;
use kcore_dashboard::config::DashboardConfig;
use kcore_dashboard::state::set_dashboard_config;
use leptos::config::get_configuration;
use tower::ServiceExt;

async fn fetch(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    let status = res.status();
    let body = to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn all_dashboard_pages_against_mock_controller() {
    let grpc = support::spawn_mock_controller().await;
    let ctrl_addr = format!("127.0.0.1:{}", grpc.port());
    set_dashboard_config(DashboardConfig::insecure_on(ctrl_addr))
        .expect("set_dashboard_config (one integration test per process)");

    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let conf = get_configuration(Some(manifest)).expect("leptos config");
    let app = dashboard_router(conf.leptos_options);

    // ── CSS ────────────────────────────────────────────────────────────
    let (status, css) = fetch(&app, "/dashboard.css").await;
    assert_eq!(status, StatusCode::OK, "CSS status");
    assert!(
        css.contains("kcore dashboard"),
        "CSS must contain identifying comment"
    );
    assert!(css.contains("--bg:"), "CSS must contain custom properties");
    assert!(css.contains("DM Sans"), "CSS must reference DM Sans font");

    // ── Homepage ──────────────────────────────────────────────────────
    let (status, home) = fetch(&app, "/").await;
    assert_eq!(status, StatusCode::OK, "/ status");
    assert!(home.contains("Declarative virtualization"));
    assert!(home.contains(r#"href="/compliance""#));
    assert!(home.contains(r#"href="/vms""#));
    assert!(home.contains(r#"href="/networks""#));
    assert!(home.contains(r#"href="/storage""#));
    assert!(home.contains(r#"href="/volumes""#));
    assert!(home.contains(r#"href="/operations""#));
    assert!(home.contains(r#"href="/ceph""#));
    assert!(home.contains(r#"href="/kds""#));
    assert!(home.contains(r#"href="/k8s""#));
    assert!(home.contains("kcore Database Service"));
    assert!(home.contains("Kubernetes clusters, nodes, and health."));
    assert!(home.contains(r#"href="/audit""#));
    assert!(home.contains("kcore"));
    assert!(home.contains("Dashboard"));
    assert!(home.contains("kcorehypervisor.com"));
    assert!(
        home.contains("Cluster is healthy"),
        "home must show the health headline"
    );
    assert!(home.contains(r#"href="/health""#));
    assert!(home.contains(r#"href="/nodes""#));
    assert!(home.contains(r#"href="/containers""#));

    // ── Compliance ────────────────────────────────────────────────────
    let (status, compliance) = fetch(&app, "/compliance").await;
    assert_eq!(status, StatusCode::OK, "/compliance status");
    assert!(
        compliance.contains("mock-controller-0.1-test"),
        "compliance must show controller version"
    );
    assert!(
        compliance.contains("rustls (integration test)"),
        "compliance must show crypto library"
    );
    assert!(
        compliance.contains("TEST_SUITE_A"),
        "compliance must show TLS 1.3 suites"
    );
    assert!(
        compliance.contains("TEST_KX"),
        "compliance must show key exchange groups"
    );
    assert!(
        compliance.contains("enabled"),
        "compliance must show mTLS enabled"
    );
    assert!(
        compliance.contains("ListVms"),
        "compliance must show access control entries"
    );
    assert!(
        compliance.contains("role:node"),
        "compliance must show allowed identities"
    );
    assert!(
        compliance.contains("Compliance report"),
        "compliance must have page title"
    );
    assert!(
        compliance.contains("Recent audit events"),
        "compliance must have audit section"
    );
    assert!(
        compliance.contains("CreateVm"),
        "compliance must show mock audit action"
    );
    assert!(
        compliance.contains("vm/web-01"),
        "compliance must show mock audit resource"
    );
    assert!(
        compliance.contains(r#"href="/audit""#),
        "compliance must link to the searchable audit trail"
    );
    assert!(
        compliance.contains("CMVP-0000"),
        "compliance must show the FIPS certificate"
    );
    assert!(
        compliance.contains("CycloneDX"),
        "compliance must show the SBOM format"
    );
    assert!(
        compliance.contains("kctl get sbom -o"),
        "compliance must point at the full SBOM command"
    );
    assert!(
        compliance.contains("laptop"),
        "compliance must preview SSH key names"
    );

    // Replication section (now from ControllerAdmin mock)
    assert!(
        compliance.contains("Replication resilience"),
        "compliance must have replication section"
    );
    assert!(
        compliance.contains("healthy"),
        "replication SLO must show healthy"
    );
    assert!(
        compliance.contains("dc-west"),
        "replication must show outgoing peer"
    );
    assert!(
        compliance.contains("Outgoing peers"),
        "replication must have outgoing peers table"
    );
    assert!(
        compliance.contains("Incoming peers"),
        "replication must have incoming peers table"
    );
    assert!(
        compliance.contains("10.0.1.1:9090"),
        "replication must show incoming peer endpoint"
    );

    // Conflicts section
    assert!(
        compliance.contains("Replication conflicts"),
        "compliance must have conflicts section"
    );
    assert!(
        compliance.contains("No unresolved replication conflicts"),
        "empty conflict list must show no-conflict message"
    );

    // ── VMs page 1 ───────────────────────────────────────────────────
    let (status, vms) = fetch(&app, "/vms").await;
    assert_eq!(status, StatusCode::OK, "/vms status");
    assert!(vms.contains("mock-vm-alpha"), "VMs must show VM name");
    assert!(
        vms.contains("Page 1 of 1"),
        "single-VM data must show page 1 of 1"
    );
    assert!(vms.contains("1 VMs"), "total count must be 1");
    assert!(vms.contains("vm-mock-1"), "VMs must show VM ID");
    assert!(vms.contains("node-mock-a"), "VMs must show node ID");
    assert!(vms.contains("Running"), "VMs must show VM state badge");
    assert!(vms.contains("512 MiB"), "VMs must show formatted memory");
    assert!(
        vms.contains(r#"/vms/mock-vm-alpha/console"#),
        "VMs must link to serial console"
    );
    assert!(vms.contains(">Console<"), "VMs must show Console action");
    assert!(vms.contains("LVM"), "VMs must show storage backend");

    // ── Serial console page ──────────────────────────────────────────
    let (status, console) = fetch(&app, "/vms/mock-vm-alpha/console").await;
    assert_eq!(status, StatusCode::OK, "/vms/.../console status");
    assert!(console.contains("xterm"), "console page must load xterm.js");
    assert!(
        console.contains("/api/vms/") && console.contains("/console"),
        "console page must open WebSocket to API path"
    );
    assert!(
        console.contains("encodeURIComponent"),
        "console page must URL-encode the VM id for WebSocket"
    );
    assert!(
        console.contains("Serial console"),
        "console page must show title"
    );

    // ── VMs pagination (page=2 with single VM returns page 1) ────────
    let (status, vms_p2) = fetch(&app, "/vms?page=2").await;
    assert_eq!(status, StatusCode::OK, "/vms?page=2 status");
    assert!(
        vms_p2.contains("Page 1 of 1") || vms_p2.contains("Page 2 of 1"),
        "out-of-bounds page must still render gracefully"
    );

    // ── VMs pagination (page=0 normalised to 1) ─────────────────────
    let (status, vms_p0) = fetch(&app, "/vms?page=0").await;
    assert_eq!(status, StatusCode::OK, "/vms?page=0 status");
    assert!(
        vms_p0.contains("Page 1 of 1"),
        "page=0 should be normalised to page 1"
    );

    // ── Networks ─────────────────────────────────────────────────────
    let (status, nets) = fetch(&app, "/networks").await;
    assert_eq!(status, StatusCode::OK, "/networks status");

    // Host networking section
    assert!(
        nets.contains("Host networking"),
        "networks must have host networking section"
    );
    assert!(
        nets.contains("Default network config"),
        "networks must show default network config card"
    );
    assert!(
        nets.contains("203.0.113.10"),
        "networks must show default external IP"
    );
    assert!(
        nets.contains("eno1"),
        "networks must show gateway interface"
    );
    assert!(
        nets.contains("mock-host-alpha"),
        "networks must show node hostname"
    );
    assert!(nets.contains("node-mock-a"), "networks must show node ID");
    assert!(
        nets.contains("aa:bb:cc:dd:ee:01"),
        "networks must show interface MAC"
    );
    assert!(
        nets.contains("br-default"),
        "networks must show bridge interface"
    );
    assert!(nets.contains("tap-vm1"), "networks must show TAP interface");
    assert!(
        nets.contains("10.0.0.10/24"),
        "networks must show interface addresses"
    );
    assert!(
        nets.contains("bridge"),
        "networks must show interface type badge"
    );

    // SDN section
    assert!(
        nets.contains("Software-defined networks"),
        "networks must have SDN section"
    );
    assert!(nets.contains("mock-net-stub"), "SDN must show name");
    assert!(nets.contains("203.0.113.1"), "SDN must show external IP");
    assert!(nets.contains("10.0.0.1"), "SDN must show gateway");
    assert!(nets.contains("nat"), "SDN must show network type");
    assert!(
        nets.contains("VXLAN overlays"),
        "networks must summarize VXLAN"
    );
    assert!(
        nets.contains("mock-overlay"),
        "networks must show the VXLAN network"
    );
    assert!(
        nets.contains("fd00:10::/64"),
        "VXLAN network must show its IPv6 prefix"
    );
    assert!(
        nets.contains("vxlan100"),
        "host view must list VXLAN interfaces"
    );
    assert!(
        nets.contains("tcp 22"),
        "VXLAN network must show forwarded ports"
    );

    // ── Storage ──────────────────────────────────────────────────────
    let (status, storage) = fetch(&app, "/storage").await;
    assert_eq!(status, StatusCode::OK, "/storage status");
    assert!(
        storage.contains("Cluster"),
        "storage must show cluster section"
    );
    assert!(
        storage.contains("VM / data storage type"),
        "storage must show backend section"
    );
    assert!(
        storage.contains("mock-host-alpha"),
        "storage must show node hostname"
    );
    assert!(storage.contains("node-mock-a"), "storage must show node id");
    assert!(
        storage.contains("LVM"),
        "storage must show LVM backend count or label"
    );
    assert!(
        storage.contains("/dev/nvme0n1"),
        "storage must list disk path from mock"
    );
    assert!(
        storage.contains("Data (kcore)"),
        "storage must show mount hint for kcore path"
    );
    assert!(
        storage.contains(">Ceph<"),
        "storage must show a Ceph backend stat"
    );

    // ── Volumes ──────────────────────────────────────────────────────
    let (status, volumes) = fetch(&app, "/volumes").await;
    assert_eq!(status, StatusCode::OK, "/volumes status");
    assert!(volumes.contains("Volumes"), "volumes page heading");
    assert!(
        volumes.contains("mock-pgdata"),
        "volumes must show mock volume name"
    );
    assert!(
        volumes.contains("kcore-vms"),
        "volumes must show the Ceph pool"
    );
    assert!(
        volumes.contains("Ceph"),
        "volumes must show the Ceph backend"
    );
    assert!(
        volumes.contains("kcore-kdata"),
        "volumes must show LVM logical volumes from node inventory"
    );
    assert!(
        volumes.contains("vg_kcore"),
        "volumes must show the LVM volume group"
    );
    assert!(
        volumes.contains("tank-data"),
        "volumes must show registered ZFS volumes"
    );
    assert!(volumes.contains("ZFS"), "volumes must show the ZFS backend");
    assert!(
        volumes.contains(r#"href="/volumes/mock-pgdata?node=node-mock-a""#),
        "each volume must open its detail page"
    );
    assert!(
        volumes.contains(r#"href="/snapshots""#),
        "volumes must link to snapshots"
    );

    // ── Operations ───────────────────────────────────────────────────
    let (status, operations) = fetch(&app, "/operations").await;
    assert_eq!(status, StatusCode::OK, "/operations status");
    assert!(
        operations.contains("VM operations") || operations.contains("Operations"),
        "operations page heading"
    );
    assert!(
        operations.contains("op-mock-1") || operations.contains("live_migrate"),
        "operations must show mock operation"
    );
    assert!(
        operations.contains("node-mock-b"),
        "operations must show the migration target"
    );
    assert!(
        operations.contains("Recent API activity"),
        "operations must show audit activity when VM jobs are the only long-running records"
    );
    assert!(
        operations.contains("CreateVm"),
        "operations activity must include audit events"
    );

    let (status, ops_search) = fetch(&app, "/operations?q=CreateVm").await;
    assert_eq!(status, StatusCode::OK, "/operations?q=CreateVm status");
    assert!(
        ops_search.contains("CreateVm"),
        "operations search must keep matching audit rows"
    );
    assert!(
        ops_search.contains("No VM jobs match this search."),
        "operations search must hide unrelated VM jobs"
    );

    // ── Audit trail ──────────────────────────────────────────────────
    let (status, audit) = fetch(&app, "/audit").await;
    assert_eq!(status, StatusCode::OK, "/audit status");
    assert!(audit.contains("Audit trail"), "audit page heading");
    assert!(audit.contains("CreateVm"), "audit must list CreateVm");
    assert!(
        audit.contains("RegisterNode"),
        "audit must list RegisterNode"
    );

    let (status, audit_q) = fetch(&app, "/audit?q=web-01").await;
    assert_eq!(status, StatusCode::OK, "/audit?q=web-01 status");
    assert!(
        audit_q.contains("vm/web-01"),
        "audit search must match the resource"
    );
    assert!(
        !audit_q.contains("RegisterNode"),
        "audit search must drop non-matching events"
    );

    let (status, audit_action) = fetch(&app, "/audit?action=RegisterNode").await;
    assert_eq!(status, StatusCode::OK, "/audit?action=RegisterNode status");
    assert!(
        audit_action.contains("node/node-mock-a"),
        "exact action filter must keep RegisterNode"
    );
    assert!(
        !audit_action.contains("vm/web-01"),
        "exact action filter must drop other actions"
    );

    let (status, audit_since) = fetch(&app, "/audit?since=2026-08-03").await;
    assert_eq!(status, StatusCode::OK, "/audit?since status");
    assert!(
        audit_since.contains("CreateVm"),
        "since filter must keep later events"
    );
    assert!(
        !audit_since.contains("RegisterNode"),
        "since filter must drop earlier events"
    );

    let (status, audit_miss) = fetch(&app, "/audit?q=no-such-event-xyz").await;
    assert_eq!(status, StatusCode::OK, "/audit miss status");
    assert!(
        audit_miss.contains("No audit events match this search."),
        "empty search must say so"
    );

    // ── Ceph ─────────────────────────────────────────────────────────
    let (status, ceph) = fetch(&app, "/ceph").await;
    assert_eq!(status, StatusCode::OK, "/ceph status");
    assert!(
        ceph.contains("mock-ceph"),
        "ceph page must show the cluster"
    );
    assert!(ceph.contains("HEALTH_OK"), "ceph page must show health");
    assert!(
        ceph.contains("10.20.0.0/24"),
        "ceph page must show the public network"
    );
    assert!(ceph.contains("/dev/sdb"), "ceph page must show OSD devices");
    assert!(ceph.contains("mock-home"), "ceph page must show CephFS");
    assert!(
        ceph.contains("mock-s3"),
        "ceph page must show the object store"
    );
    assert!(ceph.contains("7480"), "object store must show its port");

    // ── KDS ──────────────────────────────────────────────────────────
    let (status, kds) = fetch(&app, "/kds").await;
    assert_eq!(status, StatusCode::OK, "/kds status");
    assert!(kds.contains("kcore Database Service"), "KDS page heading");
    assert!(kds.contains("app-db"), "KDS must list app-db");
    assert!(kds.contains("analytics"), "KDS must list analytics");
    assert!(kds.contains("PostgreSQL"), "KDS must show the engine");
    assert!(
        kds.contains(r#"href="/kds/app-db""#),
        "each DBMS row must link to its detail page"
    );
    assert!(
        kds.contains(r#"href="/kds/analytics""#),
        "each DBMS row must link to its detail page"
    );

    let (status, db) = fetch(&app, "/kds/app-db").await;
    assert_eq!(status, StatusCode::OK, "/kds/app-db status");
    assert!(
        db.contains("postgresql_16"),
        "DBMS detail must show the package"
    );
    assert!(db.contains("node-107"), "DBMS detail must show the node");
    assert!(
        db.contains("Local Unix socket"),
        "DBMS detail must show how clients connect"
    );
    assert!(
        db.contains(r#"href="/kds""#),
        "DBMS detail must link back to the list"
    );

    let (status, missing_db) = fetch(&app, "/kds/missing-db").await;
    assert_eq!(status, StatusCode::OK, "/kds/missing-db status");
    assert!(
        missing_db.contains("not found"),
        "missing DBMS must say it was not found"
    );

    // ── Kubernetes ───────────────────────────────────────────────────
    let (status, k8s) = fetch(&app, "/k8s").await;
    assert_eq!(status, StatusCode::OK, "/k8s status");
    assert!(k8s.contains(">Kubernetes<"), "Kubernetes page heading");
    assert!(k8s.contains("lab"), "Kubernetes page must list lab");
    assert!(k8s.contains("edge"), "Kubernetes page must list edge");
    assert!(
        k8s.contains("192.168.40.220:6443"),
        "Kubernetes page must show the API endpoint"
    );
    assert!(
        k8s.contains(r#"href="/k8s/lab""#),
        "each cluster row must link to its detail page"
    );
    assert!(
        k8s.contains(r#"href="/k8s/edge""#),
        "each cluster row must link to its detail page"
    );

    let (status, cluster) = fetch(&app, "/k8s/lab").await;
    assert_eq!(status, StatusCode::OK, "/k8s/lab status");
    assert!(
        cluster.contains("native-routing"),
        "cluster detail must show Cilium mode"
    );
    assert!(
        cluster.contains("keepalived"),
        "cluster detail must show the HA method"
    );
    assert!(
        cluster.contains("node-119"),
        "cluster detail must list the worker"
    );
    assert!(
        cluster.contains("Healthy"),
        "cluster detail must show health"
    );
    assert!(
        cluster.contains(r#"href="/k8s""#),
        "cluster detail must link back to the list"
    );
    assert!(
        cluster.contains("Control-plane nodes accept workloads."),
        "lab cluster must say the control plane accepts workloads"
    );
    assert!(
        cluster.contains("kdata"),
        "lab cluster must list the host volume"
    );
    assert!(
        cluster.contains("/mnt/kcore/lab/kdata"),
        "lab cluster must show where the volume is mounted"
    );

    let (status, missing_cluster) = fetch(&app, "/k8s/missing-cluster").await;
    assert_eq!(status, StatusCode::OK, "/k8s/missing-cluster status");
    assert!(
        missing_cluster.contains("not found"),
        "missing cluster must say it was not found"
    );

    // ── Detail pages for the rest of the inventory ───────────────────
    let (status, health) = fetch(&app, "/health").await;
    assert_eq!(status, StatusCode::OK, "/health status");
    assert!(
        health.contains("Cluster is healthy"),
        "health page must use a plain headline"
    );
    assert!(
        health.contains("Nothing needs attention."),
        "a healthy cluster must say so"
    );

    let (status, nodes) = fetch(&app, "/nodes").await;
    assert_eq!(status, StatusCode::OK, "/nodes status");
    assert!(
        nodes.contains("mock-host-alpha"),
        "nodes must list the host"
    );
    assert!(
        nodes.contains(r#"href="/nodes/node-mock-a""#),
        "each node must open its detail page"
    );

    let (status, node) = fetch(&app, "/nodes/node-mock-a").await;
    assert_eq!(status, StatusCode::OK, "/nodes/node-mock-a status");
    assert!(node.contains("radeon0"), "node detail must list the GPU");
    assert!(
        node.contains("AI compute"),
        "GPU role must be plain language"
    );
    assert!(
        node.contains("Free"),
        "an unused device must say it is free"
    );

    let (status, vm) = fetch(&app, "/vms/mock-vm-alpha").await;
    assert_eq!(status, StatusCode::OK, "/vms/mock-vm-alpha status");
    assert!(vm.contains("10.0.0.50"), "VM detail must show the address");
    assert!(
        vm.contains("radeon0"),
        "VM detail must show the named device"
    );
    assert!(
        vm.contains(r#"href="/vms/mock-vm-alpha/console""#),
        "VM detail must keep the console as its own link"
    );
    assert!(
        vm.contains(r#"href="/networks/node-mock-a/mock-net-stub""#),
        "a NIC must open the network it is attached to"
    );
    assert!(
        vm.contains("no receive session"),
        "VM detail must say when migration has nothing waiting"
    );

    let (status, volume) = fetch(&app, "/volumes/mock-pgdata?node=node-mock-a").await;
    assert_eq!(status, StatusCode::OK, "volume detail status");
    assert!(
        volume.contains("abc123"),
        "volume detail must show the serial"
    );
    assert!(
        volume.contains("Not checked"),
        "an unknown guest size must say it was not checked"
    );
    assert!(
        volume.contains(r#"href="/vms/mock-vm-alpha""#),
        "volume detail must open the attached VM"
    );

    let (status, inventory) = fetch(&app, "/volumes/kcore-kdata?node=node-mock-a").await;
    assert_eq!(status, StatusCode::OK, "inventory volume status");
    assert!(
        inventory.contains("not a registered kcore volume"),
        "an LVM-only row must explain that it is not registered"
    );

    let (status, snaps) = fetch(&app, "/snapshots?volume=mock-pgdata").await;
    assert_eq!(status, StatusCode::OK, "/snapshots status");
    assert!(snaps.contains("nightly"), "snapshots must list nightly");
    assert!(
        snaps.contains(r#"href="/snapshots/nightly""#),
        "each snapshot must open its detail page"
    );

    let (status, snap) = fetch(&app, "/snapshots/nightly").await;
    assert_eq!(status, StatusCode::OK, "/snapshots/nightly status");
    assert!(
        snap.contains("mock-pgdata"),
        "snapshot detail must name the volume"
    );

    let (status, policies) = fetch(&app, "/snapshots/policies").await;
    assert_eq!(status, StatusCode::OK, "/snapshots/policies status");
    assert!(
        policies.contains("Every day"),
        "a daily schedule must be plain language"
    );

    let (status, policy) = fetch(&app, "/snapshots/policies/daily-pg").await;
    assert_eq!(status, StatusCode::OK, "snapshot policy status");
    assert!(
        policy.contains("Volume mock-pgdata"),
        "the schedule must say which volume it covers"
    );

    let (status, fs) = fetch(&app, "/ceph/fs/mock-home").await;
    assert_eq!(status, StatusCode::OK, "/ceph/fs/mock-home status");
    assert!(fs.contains("mock-home"), "CephFS detail must show the name");

    let (status, store) = fetch(&app, "/ceph/rgw/mock-s3").await;
    assert_eq!(status, StatusCode::OK, "/ceph/rgw/mock-s3 status");
    assert!(
        store.contains("7480"),
        "object store detail must show the port"
    );

    let (status, op) = fetch(&app, "/operations/op-mock-1").await;
    assert_eq!(status, StatusCode::OK, "/operations/op-mock-1 status");
    assert!(op.contains("copy"), "operation detail must show the step");
    assert!(
        op.contains(r#"href="/vms/vm-mock-1""#),
        "operation detail must open the virtual machine"
    );

    let (status, net) = fetch(&app, "/networks/node-mock-a/mock-overlay").await;
    assert_eq!(status, StatusCode::OK, "network detail status");
    assert!(
        net.contains("Virtual machines cannot talk to each other"),
        "east-west firewall must be a sentence"
    );
    assert!(net.contains(">None<"), "VLAN 0 must read as none");

    let (status, containers) = fetch(&app, "/containers").await;
    assert_eq!(status, StatusCode::OK, "/containers status");
    assert!(
        containers.contains("mock-web"),
        "containers must list mock-web"
    );
    assert!(
        containers.contains(r#"href="/containers/mock-web?node=node-mock-a""#),
        "each container must open its detail page"
    );

    let (status, container) = fetch(&app, "/containers/mock-web?node=node-mock-a").await;
    assert_eq!(status, StatusCode::OK, "container detail status");
    assert!(
        container.contains("nginx:1.27"),
        "container detail must show the image"
    );
    assert!(
        container.contains("SITE=lab"),
        "container detail must show environment"
    );

    let (status, class) = fetch(&app, "/storage/classes/lvm").await;
    assert_eq!(status, StatusCode::OK, "storage class status");
    assert!(
        class.contains("VM disks are LVM logical volumes."),
        "storage class must explain itself"
    );
    assert!(
        class.contains("mock-host-alpha"),
        "storage class must list the nodes that use it"
    );

    let (status, layout) = fetch(&app, "/storage/layouts/node-a-os").await;
    assert_eq!(status, StatusCode::OK, "disk layout status");
    assert!(
        layout.contains("Applied"),
        "disk layout must show its phase"
    );
    assert!(
        layout.contains("disk.sda"),
        "disk layout must show the Nix expression"
    );

    let (status, certs) = fetch(&app, "/compliance/certificates").await;
    assert_eq!(status, StatusCode::OK, "/compliance/certificates status");
    assert!(certs.contains("0A1B"), "certificates must show the serial");
    assert!(certs.contains("Active"), "certificates must show status");
    assert!(
        !certs.contains("BEGIN CERTIFICATE"),
        "the page must not dump certificate PEM"
    );

    let (status, keys) = fetch(&app, "/ssh-keys").await;
    assert_eq!(status, StatusCode::OK, "/ssh-keys status");
    assert!(keys.contains("laptop"), "SSH keys must list the name");
    assert!(
        !keys.contains("ssh-ed25519"),
        "the list must not show the public key"
    );

    let (status, key) = fetch(&app, "/ssh-keys/laptop").await;
    assert_eq!(status, StatusCode::OK, "/ssh-keys/laptop status");
    assert!(
        key.contains("ssh-ed25519"),
        "SSH key detail must show the public key"
    );

    assert!(
        storage.contains("ZFS datasets are not scanned"),
        "storage must explain why ZFS datasets are absent"
    );
    assert!(
        storage.contains(r#"href="/storage/classes/lvm""#),
        "storage must link each class"
    );
    assert!(
        operations.contains(r#"href="/operations/op-mock-1""#),
        "each operation must open its detail page"
    );
    assert!(
        nets.contains(r#"href="/networks/node-mock-a/mock-overlay""#),
        "each network must open its detail page"
    );

    // ── 404 for unknown path ─────────────────────────────────────────
    let (status, body404) = fetch(&app, "/nonexistent-page-xyz").await;
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::OK,
        "unknown path must return 404 or 200-with-fallback; got {status}"
    );
    assert!(
        body404.contains("Not found") || body404.contains("not found"),
        "unknown path must show Not found: {:?}",
        body404.chars().take(300).collect::<String>()
    );

    // ── Nav structure on every page ──────────────────────────────────
    for (page, body) in [
        ("/", &home),
        ("/compliance", &compliance),
        ("/vms", &vms),
        ("/networks", &nets),
        ("/storage", &storage),
        ("/volumes", &volumes),
        ("/operations", &operations),
        ("/audit", &audit),
        ("/ceph", &ceph),
        ("/kds", &kds),
        ("/k8s", &k8s),
        ("/kds/app-db", &db),
        ("/k8s/lab", &cluster),
    ] {
        assert!(body.contains("nav"), "{page} must have navigation element");
        assert!(body.contains("kcore"), "{page} must show brand name");
        assert!(
            body.contains(r#"href="/dashboard.css""#),
            "{page} must link to CSS"
        );
    }

    // ── Head contains expected meta tags ─────────────────────────────
    assert!(
        home.contains(r#"charset="utf-8""#),
        "homepage must have charset meta"
    );
    assert!(
        home.contains("viewport"),
        "homepage must have viewport meta"
    );
    assert!(
        home.contains("kcore — declarative virtualization dashboard"),
        "homepage must have description meta"
    );
}
