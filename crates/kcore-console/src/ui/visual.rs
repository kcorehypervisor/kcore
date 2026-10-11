//! Visual helpers: plain-text buffer dumps and SVG screenshots for TUI tests.

use ratatui::buffer::Buffer;
use ratatui::style::Color;

/// Flatten a ratatui buffer into a newline-separated plain string (one row per line).
pub fn buffer_plain(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn color_css(c: Color) -> String {
    match c {
        Color::Reset => "#000000".into(),
        Color::Black => "#000000".into(),
        Color::Red => "#ff6464".into(),
        Color::Green => "#50c878".into(),
        Color::Yellow => "#ffc864".into(),
        Color::Blue => "#535af4".into(),
        Color::Magenta => "#c080ff".into(),
        Color::Cyan => "#60d0d0".into(),
        Color::Gray | Color::DarkGray => "#767884".into(),
        Color::White => "#e4e6eb".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => format!("#{i:02x}{i:02x}{i:02x}"),
        _ => "#e4e6eb".into(),
    }
}

fn xml_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\0' => "".into(),
            c => c.to_string(),
        })
        .collect()
}

/// Render the buffer as an SVG screenshot (monospace cells). Good for visual review.
pub fn buffer_svg(buf: &Buffer, title: &str) -> String {
    const CW: u32 = 9;
    const CH: u32 = 18;
    let w = u32::from(buf.area.width) * CW;
    let h = u32::from(buf.area.height) * CH;
    let title_esc = xml_escape(title);
    let mut svg = String::new();
    svg.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n"
    ));
    svg.push_str(&format!("<title>{title_esc}</title>\n"));
    svg.push_str("<rect width=\"100%\" height=\"100%\" fill=\"#000000\"/>\n");
    svg.push_str("<style>\n  text { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 14px; }\n</style>\n");

    for y in 0..buf.area.height {
        let py = u32::from(y) * CH;
        let mut x = 0u16;
        while x < buf.area.width {
            let cell = &buf[(x, y)];
            let bg = color_css(cell.bg);
            let fg = color_css(cell.fg);
            // Merge consecutive cells that share the same colors.
            let mut run = String::new();
            let start_x = x;
            while x < buf.area.width {
                let c = &buf[(x, y)];
                if color_css(c.bg) != bg || color_css(c.fg) != fg {
                    break;
                }
                run.push_str(c.symbol());
                x += 1;
            }
            let px = u32::from(start_x) * CW;
            let run_w = u32::from(x - start_x) * CW;
            if bg != "#000000" {
                svg.push_str(&format!(
                    "<rect x=\"{px}\" y=\"{py}\" width=\"{run_w}\" height=\"{CH}\" fill=\"{bg}\"/>\n"
                ));
            }
            let trimmed_end = run.trim_end();
            if !trimmed_end.is_empty() {
                let ty = py + CH - 4;
                let t = xml_escape(trimmed_end);
                svg.push_str(&format!(
                    "<text x=\"{px}\" y=\"{ty}\" fill=\"{fg}\">{t}</text>\n"
                ));
            }
        }
    }
    svg.push_str("</svg>\n");
    svg
}

/// True when `needle` appears contiguously on a single buffer row.
pub fn buffer_contains_on_one_line(buf: &Buffer, needle: &str) -> bool {
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        if line.contains(needle) {
            return true;
        }
    }
    false
}
