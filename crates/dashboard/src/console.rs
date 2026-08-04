//! Serial console HTTP + WebSocket endpoints (CDN xterm.js).

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

use crate::controller_client::{self, controller_proto};
use crate::state::dashboard_config;

pub fn console_routes() -> Router {
    Router::new()
        .route("/vms/{id}/console", get(console_page))
        .route("/api/vms/{id}/console", get(console_ws_upgrade))
}

async fn console_page(Path(id): Path<String>) -> impl IntoResponse {
    let safe = html_escape(&id);
    let html = format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>Console — {safe} — kcore</title>
  <link rel="stylesheet" href="/dashboard.css"/>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.min.css"/>
  <style>
    .console-wrap {{ display:flex; flex-direction:column; height:calc(100vh - 4.5rem); padding:1rem 1.75rem 1.5rem; gap:0.75rem; }}
    .console-bar {{ display:flex; align-items:center; justify-content:space-between; gap:1rem; }}
    .console-bar h1 {{ margin:0; font-size:1.15rem; }}
    #term {{ flex:1; min-height:0; background:#0a0c12; border:1px solid var(--border); border-radius:0.5rem; padding:0.5rem; }}
    .console-status {{ font-size:0.85rem; color:var(--muted); }}
  </style>
</head>
<body>
  <header class="top-nav">
    <div class="brand">
      <a href="/" style="text-decoration:none"><span class="brand-mark">kcore</span></a>
      <span class="brand-sub">Console</span>
    </div>
    <nav class="nav-links">
      <a href="/vms">Virtual machines</a>
    </nav>
  </header>
  <div class="console-wrap">
    <div class="console-bar">
      <h1>Serial console: <code class="inline">{safe}</code></h1>
      <span id="status" class="console-status">Connecting…</span>
    </div>
    <div id="term"></div>
  </div>
  <script src="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js"></script>
  <script src="https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js"></script>
  <script>
    (function () {{
      const vmId = {id_json};
      const statusEl = document.getElementById('status');
      const term = new Terminal({{
        cursorBlink: true,
        fontFamily: 'JetBrains Mono, ui-monospace, monospace',
        fontSize: 14,
        theme: {{ background: '#0a0c12', foreground: '#e8eaef', cursor: '#5eead4' }}
      }});
      const fit = new FitAddon.FitAddon();
      term.loadAddon(fit);
      term.open(document.getElementById('term'));
      fit.fit();
      window.addEventListener('resize', () => fit.fit());

      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const ws = new WebSocket(proto + '//' + location.host + '/api/vms/' + encodeURIComponent(vmId) + '/console');
      ws.binaryType = 'arraybuffer';

      ws.onopen = () => {{ statusEl.textContent = 'Connected'; }};
      ws.onclose = () => {{ statusEl.textContent = 'Disconnected'; }};
      ws.onerror = () => {{ statusEl.textContent = 'Error'; }};
      ws.onmessage = (ev) => {{
        if (ev.data instanceof ArrayBuffer) {{
          term.write(new Uint8Array(ev.data));
        }} else if (typeof ev.data === 'string') {{
          term.write(ev.data);
        }}
      }};
      term.onData((data) => {{
        if (ws.readyState === WebSocket.OPEN) {{
          ws.send(new TextEncoder().encode(data));
        }}
      }});
    }})();
  </script>
</body>
</html>
"##,
        safe = safe,
        id_json = js_string_literal(&id),
    );
    Html(html)
}

fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

async fn console_ws_upgrade(Path(id): Path<String>, ws: WebSocketUpgrade) -> Response {
    if id.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "vm id required").into_response();
    }
    ws.on_upgrade(move |socket| handle_console_ws(socket, id))
}

async fn handle_console_ws(mut socket: WebSocket, vm_id: String) {
    let cfg = dashboard_config();
    let channel = match controller_client::connect_channel(cfg).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "console: controller connect failed");
            let _ = socket
                .send(Message::Text(
                    format!("controller connect failed: {e}").into(),
                ))
                .await;
            return;
        }
    };

    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let (to_ctrl_tx, to_ctrl_rx) = mpsc::channel::<controller_proto::ConsoleMessage>(64);
    // Opening message must be queued before the RPC await — the server
    // blocks on the first stream message before returning the response.
    if to_ctrl_tx
        .send(controller_proto::ConsoleMessage {
            vm_name: vm_id.clone(),
            data: Vec::new(),
        })
        .await
        .is_err()
    {
        return;
    }
    let outbound = ReceiverStream::new(to_ctrl_rx);

    let mut from_ctrl = match client.attach_vm_console(outbound).await {
        Ok(r) => r.into_inner(),
        Err(e) => {
            warn!(error = %e, vm = %vm_id, "console: AttachVmConsole failed");
            let _ = socket
                .send(Message::Text(format!("AttachVmConsole failed: {e}").into()))
                .await;
            return;
        }
    };

    info!(vm = %vm_id, "console session opened");

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(bin))) => {
                        if to_ctrl_tx
                            .send(controller_proto::ConsoleMessage {
                                vm_name: String::new(),
                                data: bin.to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if to_ctrl_tx
                            .send(controller_proto::ConsoleMessage {
                                vm_name: String::new(),
                                data: text.as_bytes().to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            grpc = from_ctrl.message() => {
                match grpc {
                    Ok(Some(m)) => {
                        if !m.data.is_empty()
                            && socket
                                .send(Message::Binary(m.data.into()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = socket
                            .send(Message::Text(format!("console stream error: {e}").into()))
                            .await;
                        break;
                    }
                }
            }
        }
    }

    info!(vm = %vm_id, "console session closed");
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
