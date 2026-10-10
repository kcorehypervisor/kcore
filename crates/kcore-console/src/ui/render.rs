//! Ratatui layout: overview, network, storage, diagnostics, help.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Stylize};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs};
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
    let area = f.area();
    let ch = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    if ch.len() < 4 {
        return;
    }
    let brand = ch[0];
    let tabbar = ch[1];
    let body = ch[2];
    let foot = ch[3];

    let title = Line::from(vec![
        Span::styled(" ", Style::default().bg(theme::bg())),
        Span::styled(
            "KCORE",
            Style::default()
                .fg(theme::silver())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " hypervisor · console ",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("v{}", app.snapshot.meta.version),
            Style::default().fg(theme::muted()),
        ),
    ]);
    f.render_widget(
        Paragraph::new(title).block(Block::new().bg(theme::bg())),
        brand,
    );

    let tab_ix = app.page as usize;
    let tabs = Tabs::new(
        ["Overview", "Network", "Storage", "Diagnostics", "Help"]
            .iter()
            .map(|s| Line::from(Span::styled(*s, Style::default().fg(theme::text()))))
            .collect::<Vec<Line>>(),
    )
    .select(tab_ix)
    .style(Style::default().fg(theme::muted()))
    .highlight_style(
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    )
    .divider(Span::raw(" │ "));
    f.render_widget(
        tabs.block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(theme::accent())),
        ),
        tabbar,
    );

    let (body, logo) = carve_logo_area(body);
    match app.page {
        Page::Overview => draw_overview(f, body, app),
        Page::Network => draw_network(f, app, body),
        Page::Storage => draw_storage(f, app, body),
        Page::Diagnostics => draw_diagnostics(f, body, app),
        Page::Help => draw_help(f, body, app.dev),
    }
    if let Some(logo) = logo {
        draw_logo(f, logo);
    }

    let footer = Line::from(vec![
        Span::styled("Tab/←/→  ", theme::muted()),
        Span::styled("r refresh  ", theme::muted()),
        Span::styled("? help  ", theme::muted()),
        Span::styled(
            if app.dev {
                "q quit (dev) "
            } else {
                "q disabled  "
            },
            theme::muted(),
        ),
    ]);
    f.render_widget(
        Paragraph::new(footer)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme::accent())),
            )
            .style(Style::default().fg(theme::muted())),
        foot,
    );
}

fn carve_logo_area(area: Rect) -> (Rect, Option<Rect>) {
    const LOGO_WIDTH: u16 = 52;
    const LOGO_HEIGHT: u16 = 6;
    // Hide on short/narrow frames (80x25 and similar) so tables keep full height.
    if area.width < 100 || area.height < 24 {
        return (area, None);
    }

    let logo = Rect {
        x: area.x + area.width - LOGO_WIDTH,
        y: area.y + area.height - LOGO_HEIGHT,
        width: LOGO_WIDTH,
        height: LOGO_HEIGHT,
    };
    let content = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - LOGO_HEIGHT,
    };
    (content, Some(logo))
}

fn draw_logo(f: &mut Frame<'_>, area: Rect) {
    /// Row inside a box: `│ … │` with centered word and black padding.
    fn row_centered(word: &str, inner: usize, word_style: Style) -> Line<'_> {
        let word: String = word.chars().take(inner).collect();
        let wc = word.chars().count().min(inner);
        let pad = inner.saturating_sub(wc);
        let left = pad / 2;
        let right = pad - left;
        let pad_style = Style::default().bg(theme::bg()).fg(theme::bg());
        Line::from(vec![
            Span::styled("│", theme::accent()),
            Span::styled(" ".repeat(left), pad_style),
            Span::styled(word, word_style),
            Span::styled(" ".repeat(right), pad_style),
            Span::styled("│", theme::accent()),
        ])
    }

    let w = area.width as usize;
    if w < 12 {
        return;
    }
    let inner = w.saturating_sub(2);
    let rule = "─".repeat(inner);
    let lines = vec![
        Line::from(vec![
            Span::styled("╭", theme::accent()),
            Span::styled(rule.clone(), theme::muted()),
            Span::styled("╮", theme::accent()),
        ]),
        row_centered(
            "KCORE",
            inner,
            Style::default()
                .fg(theme::silver())
                .add_modifier(Modifier::BOLD),
        ),
        row_centered(
            "hypervisor · console",
            inner,
            Style::default().fg(theme::muted()),
        ),
        Line::from(vec![
            Span::styled("╰", theme::accent()),
            Span::styled(rule, theme::muted()),
            Span::styled("╯", theme::accent()),
        ]),
    ];

    f.render_widget(Paragraph::new(lines).bg(theme::bg()), area);
}

fn draw_overview(f: &mut Frame<'_>, area: Rect, app: &AppState) {
    let m = &app.snapshot.meta;
    let hc = health_color(&m.health);
    let lines = vec![
        Line::from(vec![
            Span::styled("Node: ", theme::muted()),
            Span::styled(m.hostname.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Version: ", theme::muted()),
            Span::styled(m.version.clone(), theme::text()),
            Span::raw("  "),
            Span::styled("Build: ", theme::muted()),
            Span::styled(m.build_id.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Uptime: ", theme::muted()),
            Span::styled(m.uptime_str.clone(), theme::text()),
            Span::raw("  "),
            Span::styled("Time: ", theme::muted()),
            Span::styled(m.local_time.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Cluster: ", theme::muted()),
            Span::styled(m.cluster_name.clone(), theme::text()),
            Span::raw("  "),
            Span::styled("Role: ", theme::muted()),
            Span::styled(m.node_role.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Health: ", theme::muted()),
            Span::styled(
                hlabel(&m.health).to_string(),
                Style::default().fg(hc).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("API endpoint: ", theme::muted()),
            Span::styled(m.api_endpoint.clone(), theme::text()),
            Span::raw("  "),
            Span::styled("Status: ", theme::muted()),
            Span::styled(
                match &m.api_status {
                    ApiStatus::Unavailable => "unavailable",
                    ApiStatus::Reachable { .. } => "available",
                }
                .to_string(),
                theme::text(),
            ),
        ]),
        Line::from(vec![
            Span::styled("Management: ", theme::muted()),
            Span::styled(m.management_url.clone(), theme::text()),
        ]),
        Line::from(vec![
            Span::styled("Local login: ", theme::muted()),
            Span::styled(
                m.local_login.to_string(),
                Style::default()
                    .fg(theme::warn())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Remote: ", theme::muted()),
            Span::styled(
                m.remote_hint.clone(),
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Overview ")
                .title_style(theme::title_style())
                .border_style(Style::default().fg(theme::accent())),
        ),
        area,
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
                .title(" Network inventory ")
                .title_style(theme::title_style())
                .border_style(Style::default().fg(theme::accent())),
        );
    f.render_widget(table, table_area);

    if show_detail {
        let detail = network_detail_line(app);
        f.render_widget(
            Paragraph::new(detail).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Selected ")
                    .title_style(theme::title_style())
                    .border_style(Style::default().fg(theme::accent())),
            ),
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
                .title(" Storage inventory ")
                .title_style(theme::title_style())
                .border_style(Style::default().fg(theme::accent())),
        );
    f.render_widget(table, area);
}

fn draw_diagnostics(f: &mut Frame<'_>, area: Rect, app: &AppState) {
    let mut line = String::new();
    for s in &app.snapshot.diag {
        line.push_str(&format!(" {}: {}  │", s.name, s.status));
    }
    f.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" kcore services (local) ")
                .title_style(theme::title_style())
                .border_style(Style::default().fg(theme::accent())),
        ),
        area,
    );
}

fn draw_help(f: &mut Frame<'_>, area: Rect, _dev: bool) {
    let t = "KCORE hypervisor appliance — local display is read-only.\
        \nUse the management URL and kcorectl from a trusted machine.\
        \nReboot, shutdown, and root shell are intentionally unavailable here.\
        \n\nNetwork: tables adapt to console width. IPv4 is never truncated.\
        \nIPv6 shows the first global address (else link-local) and +N when more\
        \nexist; the full list is on the Selected detail line under the table.\
        \nBridge members show as \"iface -> br0\" (or a master column when wide).\
        \n\nSecurity: protect boot (UEFI, GRUB), mask extra getty, prefer SSH.\
        \nSee: docs/appliance-console.md in the kcore source tree.\n";
    f.render_widget(
        Paragraph::new(t).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help ")
                .title_style(theme::title_style())
                .border_style(Style::default().fg(theme::accent())),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, Page};
    use crate::inventory::network::Nic;
    use crate::inventory::Snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn logo_area_is_reserved_at_bottom_right() {
        let area = Rect::new(0, 0, 120, 32);

        let (content, logo) = carve_logo_area(area);

        assert_eq!(content, Rect::new(0, 0, 120, 26));
        assert_eq!(logo, Some(Rect::new(68, 26, 52, 6)));
    }

    #[test]
    fn logo_area_is_skipped_on_small_consoles() {
        let area = Rect::new(0, 0, 63, 20);

        let (content, logo) = carve_logo_area(area);

        assert_eq!(content, area);
        assert_eq!(logo, None);
    }

    #[test]
    fn logo_area_is_skipped_on_80x25_body() {
        // 80-col body is below the width threshold so tables keep height.
        let area = Rect::new(0, 0, 80, 22);

        let (content, logo) = carve_logo_area(area);

        assert_eq!(content, area);
        assert_eq!(logo, None);
    }

    #[test]
    fn logo_area_is_available_on_128x48_body() {
        let area = Rect::new(0, 0, 128, 45);

        let (content, logo) = carve_logo_area(area);

        assert_eq!(content.height, 39);
        assert_eq!(logo, Some(Rect::new(76, 39, 52, 6)));
    }

    fn sample_network_snapshot() -> Snapshot {
        Snapshot {
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
            ..Snapshot::default()
        }
    }

    fn buffer_line(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        let mut s = String::new();
        for x in 0..buf.area.width {
            s.push_str(buf[(x, y)].symbol());
        }
        s
    }

    fn buffer_contains_on_one_line(buf: &ratatui::buffer::Buffer, needle: &str) -> bool {
        for y in 0..buf.area.height {
            if buffer_line(buf, y).contains(needle) {
                return true;
            }
        }
        false
    }

    fn render_network_at(width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut app = AppState::new(true, sample_network_snapshot());
        app.page = Page::Network;
        // Select br0 so the detail line is exercised.
        app.network_sel = 1;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &app)).expect("draw network page");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn network_ipv4_complete_at_200x56() {
        let buf = render_network_at(200, 56);
        assert!(
            buffer_contains_on_one_line(&buf, "192.168.40.143/24"),
            "IPv4 must appear intact on one row at 200x56"
        );
    }

    #[test]
    fn network_ipv4_complete_at_128x48() {
        let buf = render_network_at(128, 48);
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
        let buf = render_network_at(80, 25);
        assert!(
            buffer_contains_on_one_line(&buf, "192.168.40.143/24"),
            "IPv4 must appear intact on one row at 80x25"
        );
    }
}
