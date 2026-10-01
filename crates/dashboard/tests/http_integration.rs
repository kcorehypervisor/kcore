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
    assert!(home.contains(r#"href="/audit""#));
    assert!(home.contains("kcore"));
    assert!(home.contains("Dashboard"));
    assert!(home.contains("kcorehypervisor.com"));

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
