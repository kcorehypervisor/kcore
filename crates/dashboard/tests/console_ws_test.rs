//! WebSocket serial console smoke test against mock controller (echo).

mod support;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use kcore_dashboard::app_server::dashboard_router;
use kcore_dashboard::config::DashboardConfig;
use kcore_dashboard::state::set_dashboard_config;
use leptos::config::get_configuration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn console_websocket_echoes_bytes() {
    let grpc = support::spawn_mock_controller().await;
    let ctrl_addr = format!("127.0.0.1:{}", grpc.port());
    set_dashboard_config(DashboardConfig::insecure_on(ctrl_addr)).expect("set_dashboard_config");

    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let conf = get_configuration(Some(manifest)).expect("leptos config");
    let app = dashboard_router(conf.leptos_options);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    // Give the server a moment to accept.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("ws://{addr}/api/vms/mock-vm-alpha/console");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    ws.send(Message::Binary(b"hello-console".to_vec().into()))
        .await
        .expect("send");

    let reply = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timeout waiting for echo")
        .expect("stream ended")
        .expect("ws error");

    match reply {
        Message::Binary(b) => assert_eq!(&b[..], b"hello-console"),
        Message::Text(t) => panic!("unexpected text frame: {t}"),
        other => panic!("unexpected frame: {other:?}"),
    }

    let _ = ws.close(None).await;
}
