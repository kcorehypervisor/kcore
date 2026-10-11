//! Ratatui layout: overview, network, storage, diagnostics, help.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::{AppState, Page};
use crate::inventory::api::ApiStatus;
use crate::inventory::system::Health;
use crate::theme;

fn health_color(h: &Health) -> Color {
    match h {
        Health::Ok => theme::good(),
        Health::Degraded | Health::Unknown => theme::warn(),
        Health::Critical => theme::bad(),
    }
}

fn hlabel(h: &Health) -> &'static str {
    match h {
        Health::Ok => "OK",
        Health::Degraded => "Degraded",
        Health::Unknown => "Unknown",
        Health::Critical => "Critical",
    }
}

pub fn draw(f: &mut Frame<'_>, app: &AppState) {
    // Paint the full frame black first so stray tty/journal noise never shows through.
    f.render_widget(
        Block::default().style(Style::default().bg(theme::bg())),
        f.area(),
    );

    let area = f.area();
    let ch = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // brand
            Constraint::Length(1), // tabs
            Constraint::Length(1), // accent rule under tabs
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);
    if ch.len() < 5 {
        return;
    }
    let brand = ch[0];
    let tabbar = ch[1];
    let rule = ch[2];
    let body = ch[3];
    let foot = ch[4];

    draw_brand(f, brand, app);
    draw_tabs(f, tabbar, app);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(rule.width as usize),
            Style::default().fg(theme::accent()).bg(theme::bg()),
        )))
        .style(Style::default().bg(theme::bg())),
        rule,
    );

    match app.page {
        Page::Overview => draw_overview(f, body, app),
        Page::Network => draw_network(f, app, body),
        Page::Storage => draw_storage(f, app, body),
        Page::Diagnostics => draw_diagnostics(f, body, app),
        Page::Help => draw_help(f, body, app.dev),
    }

    draw_footer(f, foot, app);
}

fn draw_brand(f: &mut Frame<'_>, area: Rect, app: &AppState) {
    let m = &app.snapshot.meta;
    let hc = health_color(&m.health);
    let health = format!(" {} ", hlabel(&m.health));
    let pad = " ".repeat(2);
    let left = Line::from(vec![
        Span::styled(" ", Style::default().bg(theme::bg())),
        Span::styled(
            "KCORE",
            Style::default()
                .fg(theme::silver())
                .bg(theme::bg())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  HYPERVISOR",
            Style::default().fg(theme::accent()).bg(theme::bg()),
        ),
        Span::styled(
            format!("  v{}  ", m.version),
            Style::default().fg(theme::muted()).bg(theme::bg()),
        ),
        Span::styled(
            m.hostname.clone(),
            Style::default().fg(theme::text()).bg(theme::bg()),
        ),
    ]);
    // Right-aligned health badge via a two-chunk layout.
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(health.len() as u16 + 2),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(left).style(Style::default().bg(theme::bg())),
        parts[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(pad, Style::default().bg(theme::bg())),
            Span::styled(
                health,
                Style::default()
                    .fg(theme::bg())
                    .bg(hc)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .style(Style::default().bg(theme::bg())),
        parts[1],
    );
}

fn draw_tabs(f: &mut Frame<'_>, area: Rect, app: &AppState) {
    let labels = [
        " Overview ",
        " Network ",
        " Storage ",
        " Diagnostics ",
        " Help ",
    ];
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(" ", Style::default().bg(theme::bg())));
    for (i, label) in labels.iter().enumerate() {
        let active = i == app.page as usize;
        let style = if active {
            theme::tab_active()
        } else {
            theme::tab_inactive()
        };
        spans.push(Span::styled((*label).to_string(), style));
        spans.push(Span::styled(
            " ",
            Style::default().bg(theme::bg()).fg(theme::bg()),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::bg())),
        area,
    );
}

fn draw_footer(f: &mut Frame<'_>, area: Rect, app: &AppState) {
    let mut spans = vec![
        Span::styled(" Tab/←/→ pages ", theme::muted()),
        Span::styled("↑↓ select ", theme::muted()),
        Span::styled("r refresh ", theme::muted()),
        Span::styled("? help ", theme::muted()),
    ];
    if app.dev {
        spans.push(Span::styled("q quit ", theme::warn()));
    }
    if let Some(msg) = &app.last_msg {
        spans.push(Span::styled(" │ ", theme::muted()));
        spans.push(Span::styled(msg.clone(), theme::warn()));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(theme::bg()).fg(theme::muted())),
        area,
    );
}

fn panel_block(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(theme::title_style())
        .border_style(Style::default().fg(theme::accent()))
        .style(Style::default().bg(theme::panel_bg()))
}

fn draw_overview(f: &mut Frame<'_>, area: Rect, app: &AppState) {
    let m = &app.snapshot.meta;
    let hc = health_color(&m.health);
    let api_status = match &m.api_status {
        ApiStatus::Unavailable => ("unavailable", theme::warn()),
        ApiStatus::Reachable { .. } => ("available", theme::good()),
    };

    // Status card height: enough for the key facts without crowding the log.
    let status_h = if area.height >= 18 { 11 } else { 9 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(status_h), Constraint::Min(4)])
        .split(area);

    let status_lines = vec![
        Line::from(vec![
            Span::styled("Node      ", theme::muted()),
            Span::styled(m.hostname.clone(), theme::text()),
            Span::styled("    Role  ", theme::muted()),
            Span::styled(m.node_role.clone(), theme::text()),
            Span::styled("    Cluster  ", theme::muted()),
            Span::styled(m.cluster_name.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Version   ", theme::muted()),
            Span::styled(m.version.clone(), theme::text()),
            Span::styled("    Build ", theme::muted()),
            Span::styled(m.build_id.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Uptime    ", theme::muted()),
            Span::styled(m.uptime_str.clone(), theme::text()),
            Span::styled("    Time  ", theme::muted()),
            Span::styled(m.local_time.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Health    ", theme::muted()),
            Span::styled(
                hlabel(&m.health).to_string(),
                Style::default().fg(hc).add_modifier(Modifier::BOLD),
            ),
            Span::styled("    API   ", theme::muted()),
            Span::styled(m.api_endpoint.clone(), theme::text()),
            Span::styled("  (", theme::muted()),
            Span::styled(api_status.0.to_string(), Style::default().fg(api_status.1)),
            Span::styled(")", theme::muted()),
        ]),
        Line::from(vec![
            Span::styled("Manage    ", theme::muted()),
            Span::styled(m.management_url.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Remote    ", theme::muted()),
            Span::styled(m.remote_hint.clone(), Style::default().fg(theme::accent())),
        ]),
        Line::from(vec![
            Span::styled("Local login  ", theme::muted()),
            Span::styled(
                m.local_login.to_string(),
                Style::default()
                    .fg(theme::warn())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  — administration is remote-only on this console",
                theme::muted(),
            ),
        ]),
    ];
    f.render_widget(
        Paragraph::new(status_lines)
            .block(panel_block(" Node status "))
            .style(Style::default().bg(theme::panel_bg())),
        chunks[0],
    );

    let log_lines: Vec<Line> = if app.snapshot.logs.is_empty() {
        vec![Line::from(Span::styled(
            "No recent journal lines.",
            theme::muted(),
        ))]
    } else {
        // Show the newest lines at the bottom of the panel.
        let visible = chunks[1].height.saturating_sub(2) as usize;
        let logs = &app.snapshot.logs;
        let start = logs.len().saturating_sub(visible);
        logs[start..]
            .iter()
            .map(|l| Line::from(Span::styled(l.text.clone(), theme::text())))
            .collect()
    };
    f.render_widget(
        Paragraph::new(log_lines)
            .block(panel_block(" System log "))
            .style(Style::default().bg(theme::panel_bg())),
        chunks[1],
    );
}

fn draw_network(f: &mut Frame<'_>, app: &AppState, area: Rect) {
    use super::cols::{net_columns_for_width, net_iface_shows_master_suffix, NetColumn};

    let show_detail = area.height >= 8;
    let chunks = if show_detail {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0)])
            .split(area)
    };
    let table_area = chunks[0];

    // Inner width ≈ area minus border columns.
    let inner_w = table_area.width.saturating_sub(2);
    let cols = net_columns_for_width(inner_w);
    let narrow_flags = inner_w < 110;
    let master_suffix = net_iface_shows_master_suffix(cols);
    let constraints: Vec<Constraint> = cols.iter().map(|c| c.constraint(narrow_flags)).collect();

    let header = Row::new(cols.iter().map(|c| {
        let h = if narrow_flags && matches!(c, NetColumn::Def | NetColumn::Mgmt) {
            match c {
                NetColumn::Def => "d",
                NetColumn::Mgmt => "m",
                _ => c.header(),
            }
        } else {
            c.header()
        };
        Cell::from(h)
    }))
    .style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(theme::accent()),
    )
    .height(1);

    let flag = |on: bool| -> &'static str {
        if narrow_flags {
            if on {
                "•"
            } else {
                " "
            }
        } else if on {
            "•"
        } else {
            "—"
        }
    };

    let rows: Vec<Row<'_>> = if app.snapshot.nics.is_empty() {
        vec![Row::new(cols.iter().map(|_| Cell::from("—")).collect::<Vec<_>>()).height(1)]
    } else {
        app.snapshot
            .nics
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let style = if i == app.network_sel {
                    Style::default().fg(theme::text()).bg(theme::selection_bg())
                } else {
                    Style::default().fg(theme::text())
                };
                let (ipv6_primary, ipv6_extra) = n.primary_ipv6_display();
                let ipv6_cell = if ipv6_extra > 0 {
                    format!("{ipv6_primary} +{ipv6_extra}")
                } else {
                    ipv6_primary
                };
                let cells: Vec<Cell<'_>> = cols
                    .iter()
                    .map(|c| {
                        let text = match c {
                            NetColumn::Iface => n.iface_label(master_suffix),
                            NetColumn::State => n.oper_state.clone(),
                            NetColumn::Mac => n.mac.clone(),
                            NetColumn::Ipv4 => n.ipv4.clone(),
                            NetColumn::Ipv6 => ipv6_cell.clone(),
                            NetColumn::Mtu => n.mtu.clone(),
                            NetColumn::Speed => n.speed.clone(),
                            NetColumn::Driver => n.driver.clone(),
                            NetColumn::Master => {
                                if n.master.is_empty() {
                                    "—".into()
                                } else {
                                    n.master.clone()
                                }
                            }
                            NetColumn::Def => flag(n.default_route).into(),
                            NetColumn::Mgmt => flag(n.management).into(),
                        };
                        Cell::from(text)
                    })
                    .collect();
                Row::new(cells).style(style).height(1)
            })
            .collect()
    };

    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(1)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Network ")
                .title_style(theme::title_style())
                .border_style(Style::default().fg(theme::accent()))
                .style(Style::default().bg(theme::panel_bg())),
        );
    f.render_widget(table, table_area);

    if show_detail {
        let detail = network_detail_line(app);
        f.render_widget(
            Paragraph::new(detail)
                .block(panel_block(" Detail "))
                .style(Style::default().bg(theme::panel_bg())),
            chunks[1],
        );
    }
}

fn network_detail_line(app: &AppState) -> Line<'static> {
    let Some(n) = app.snapshot.nics.get(app.network_sel) else {
        return Line::from(Span::styled("No interface selected", theme::muted()));
    };
    let master = if n.master.is_empty() {
        String::new()
    } else {
        format!("  master={}", n.master)
    };
    Line::from(vec![
        Span::styled(n.name.clone(), theme::text()),
        Span::styled(master, theme::muted()),
        Span::styled("  IPv6: ", theme::muted()),
        Span::styled(n.ipv6.clone(), theme::text()),
    ])
}

fn draw_storage(f: &mut Frame<'_>, app: &AppState, area: Rect) {
    use super::cols::{disk_columns_for_width, DiskColumn};

    let inner_w = area.width.saturating_sub(2);
    let cols = disk_columns_for_width(inner_w);
    let constraints: Vec<Constraint> = cols.iter().map(|c| c.constraint()).collect();

    let header = Row::new(cols.iter().map(|c| Cell::from(c.header())))
        .style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(theme::accent()),
        )
        .height(1);

    let rows: Vec<Row<'_>> = if app.snapshot.disks.is_empty() {
        vec![Row::new(cols.iter().map(|_| Cell::from("—")).collect::<Vec<_>>()).height(1)]
    } else {
        app.snapshot
            .disks
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let style = if i == app.storage_sel {
                    Style::default().fg(theme::text()).bg(theme::selection_bg())
                } else {
                    Style::default().fg(theme::text())
                };
                let cells: Vec<Cell<'_>> = cols
                    .iter()
                    .map(|c| {
                        let text = match c {
                            DiskColumn::Name => d.name.clone(),
                            DiskColumn::Path => d.path.clone(),
                            DiskColumn::Model => d.model.clone(),
                            DiskColumn::Serial => d.serial.clone(),
                            DiskColumn::Size => d.size_text.clone(),
                            DiskColumn::Kind => d.kind.clone(),
                            DiskColumn::Ro => d.ro.clone(),
                            DiskColumn::Mounts => d.mountpoints.clone(),
                            DiskColumn::Health => d.health.clone(),
                            DiskColumn::Role => d.usage_role.clone(),
                        };
                        Cell::from(text)
                    })
                    .collect();
                Row::new(cells).style(style).height(1)
            })
            .collect()
    };

    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(1)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Storage ")
                .title_style(theme::title_style())
                .border_style(Style::default().fg(theme::accent()))
                .style(Style::default().bg(theme::panel_bg())),
        );
    f.render_widget(table, area);
}

fn draw_diagnostics(f: &mut Frame<'_>, area: Rect, app: &AppState) {
    let lines: Vec<Line> = if app.snapshot.diag.is_empty() {
        vec![Line::from(Span::styled(
            "No local services reported.",
            theme::muted(),
        ))]
    } else {
        app.snapshot
            .diag
            .iter()
            .map(|s| {
                let ok = s.status == "healthy";
                Line::from(vec![
                    Span::styled(format!("  {:<22}", s.name), theme::text()),
                    Span::styled(
                        s.status.clone(),
                        if ok {
                            Style::default().fg(theme::good())
                        } else {
                            Style::default().fg(theme::warn())
                        },
                    ),
                ])
            })
            .collect()
    };
    f.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" Services "))
            .style(Style::default().bg(theme::panel_bg())),
        area,
    );
}

fn draw_help(f: &mut Frame<'_>, area: Rect, _dev: bool) {
    let t = "KCORE hypervisor appliance — local display is read-only.\
        \nUse the management URL and kcorectl from a trusted machine.\
        \nReboot, shutdown, and root shell are intentionally unavailable here.\
        \n\nTabs: Overview · Network · Storage · Diagnostics · Help\
        \nThe active tab is highlighted. Arrow keys and Tab switch pages.\
        \n\nNetwork: tables adapt to console width. IPv4 is never truncated.\
        \nIPv6 shows the first global address (else link-local) and +N when more\
        \nexist; the full list is on the Detail line under the table.\
        \nBridge members show as \"iface -> br0\" (or a master column when wide).\
        \n\nOverview: node status above, host system log below.\
        \n\nSecurity: protect boot (UEFI, GRUB), mask extra getty, prefer SSH.\
        \nSee: docs/appliance-console.md in the kcore source tree.\n";
    f.render_widget(
        Paragraph::new(t)
            .block(panel_block(" Help "))
            .style(Style::default().bg(theme::panel_bg())),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, Page};
    use crate::inventory::logs::LogLine;
    use crate::inventory::network::Nic;
    use crate::inventory::system::{Health, Meta};
    use crate::inventory::Snapshot;
    use crate::ui::visual::{buffer_contains_on_one_line, buffer_plain, buffer_svg};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn sample_snapshot() -> Snapshot {
        let meta = Meta {
            hostname: "node-103".into(),
            version: "0.3.0".into(),
            build_id: "deadbeef".into(),
            uptime_str: "4h 12m".into(),
            local_time: "2026-10-10 20:00:00".into(),
            cluster_name: "garage".into(),
            node_role: "compute".into(),
            health: Health::Ok,
            management_url: "https://192.168.40.103:8443".into(),
            api_endpoint: "192.168.40.103:9091".into(),
            remote_hint: "kcorectl login https://192.168.40.103:8443".into(),
            ..Meta::default()
        };

        Snapshot {
            meta,
            nics: vec![
                Nic {
                    name: "eno1".into(),
                    mac: "aa:bb:cc:dd:ee:01".into(),
                    oper_state: "UP".into(),
                    ipv4: "—".into(),
                    ipv6: "—".into(),
                    mtu: "1500".into(),
                    speed: "1000 Mbps".into(),
                    driver: "igb".into(),
                    default_route: false,
                    management: false,
                    master: "br0".into(),
                },
                Nic {
                    name: "br0".into(),
                    mac: "aa:bb:cc:dd:ee:01".into(),
                    oper_state: "UP".into(),
                    ipv4: "192.168.40.143/24".into(),
                    ipv6: "2a02:ef8:1000:1d00:7d1d:ca86:5156:8dd8/64, fe80::7d1d:ca86:5156:8dd8/64"
                        .into(),
                    mtu: "1500".into(),
                    speed: "—".into(),
                    driver: "bridge".into(),
                    default_route: true,
                    management: true,
                    master: String::new(),
                },
                Nic {
                    name: "enp6s0d1".into(),
                    mac: "11:22:33:44:55:66".into(),
                    oper_state: "UP".into(),
                    ipv4: "192.168.47.3/24".into(),
                    ipv6: "—".into(),
                    mtu: "9000".into(),
                    speed: "10000 Mbps".into(),
                    driver: "mlx5_core".into(),
                    default_route: false,
                    management: false,
                    master: String::new(),
                },
            ],
            logs: vec![
                LogLine {
                    text: "2026-10-10T19:58:01+01:00 node-103 kernel: Bridge br0 up".into(),
                },
                LogLine {
                    text: "2026-10-10T19:59:12+01:00 node-103 kcore-node-agent[412]: heartbeat ok"
                        .into(),
                },
                LogLine {
                    text: "2026-10-10T20:00:00+01:00 node-103 systemd[1]: Started kcore-console"
                        .into(),
                },
            ],
            ..Snapshot::default()
        }
    }

    fn render_page(page: Page, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut app = AppState::new(true, sample_snapshot());
        app.page = page;
        if page == Page::Network {
            app.network_sel = 1;
        }
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &app)).expect("draw");
        terminal.backend().buffer().clone()
    }

    fn visual_dir() -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/console-visual");
        std::fs::create_dir_all(&root).expect("visual dir");
        root
    }

    fn write_visual(name: &str, buf: &ratatui::buffer::Buffer) {
        let dir = visual_dir();
        std::fs::write(dir.join(format!("{name}.txt")), buffer_plain(buf)).expect("txt");
        std::fs::write(dir.join(format!("{name}.svg")), buffer_svg(buf, name)).expect("svg");
    }

    #[test]
    fn active_tab_is_highlighted_and_visible() {
        let buf = render_page(Page::Network, 128, 48);
        let plain = buffer_plain(&buf);
        assert!(plain.contains(" Network "), "Network tab label must render");
        assert!(plain.contains(" Overview "), "all tabs remain visible");
        assert!(
            buffer_contains_on_one_line(&buf, " Network "),
            "active Network panel title"
        );
        write_visual("network-128x48", &buf);
    }

    #[test]
    fn overview_splits_status_and_system_log() {
        let buf = render_page(Page::Overview, 128, 48);
        assert!(buffer_contains_on_one_line(&buf, "Node status"));
        assert!(buffer_contains_on_one_line(&buf, "System log"));
        assert!(buffer_contains_on_one_line(&buf, "node-103"));
        assert!(buffer_contains_on_one_line(&buf, "heartbeat ok"));
        let plain = buffer_plain(&buf);
        let status_at = plain.find("Node status").expect("status");
        let log_at = plain.find("System log").expect("log");
        assert!(status_at < log_at, "status panel above system log");
        write_visual("overview-128x48", &buf);
    }

    #[test]
    fn overview_fits_80x25_without_panic() {
        let buf = render_page(Page::Overview, 80, 25);
        assert!(buffer_contains_on_one_line(&buf, " Overview "));
        assert!(buffer_contains_on_one_line(&buf, "Node status"));
        write_visual("overview-80x25", &buf);
    }

    #[test]
    fn network_ipv4_complete_at_200x56() {
        let buf = render_page(Page::Network, 200, 56);
        assert!(
            buffer_contains_on_one_line(&buf, "192.168.40.143/24"),
            "IPv4 must appear intact on one row at 200x56"
        );
        write_visual("network-200x56", &buf);
    }

    #[test]
    fn network_ipv4_complete_at_128x48() {
        let buf = render_page(Page::Network, 128, 48);
        assert!(
            buffer_contains_on_one_line(&buf, "192.168.40.143/24"),
            "IPv4 must appear intact on one row at 128x48"
        );
        assert!(
            buffer_contains_on_one_line(&buf, "eno1 -> br0")
                || buffer_contains_on_one_line(&buf, "master"),
            "bridge membership must be visible at 128x48"
        );
    }

    #[test]
    fn network_ipv4_complete_at_80x25() {
        let buf = render_page(Page::Network, 80, 25);
        assert!(
            buffer_contains_on_one_line(&buf, "192.168.40.143/24"),
            "IPv4 must appear intact on one row at 80x25"
        );
        write_visual("network-80x25", &buf);
    }
}
