//! Recent host journal lines for the Overview system-log panel.

use std::process::Command;

/// One journal / dmesg line (already truncated for the TUI).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogLine {
    pub text: String,
}

/// Best-effort recent system log lines (journalctl, then dmesg).
pub fn recent_system_logs(limit: usize) -> Vec<LogLine> {
    let limit = limit.clamp(1, 200);
    if let Some(lines) = journalctl_lines(limit) {
        return lines;
    }
    dmesg_lines(limit)
}

fn journalctl_lines(limit: usize) -> Option<Vec<LogLine>> {
    let out = Command::new("journalctl")
        .args(["-n", &limit.to_string(), "-o", "short-iso", "--no-pager"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<LogLine> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| LogLine {
            text: truncate_log(l, 240),
        })
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines)
    }
}

fn dmesg_lines(limit: usize) -> Vec<LogLine> {
    let out = Command::new("dmesg")
        .args(["-T", "--level", "err,warn,info"])
        .output();
    let Ok(out) = out else {
        return vec![LogLine {
            text: "system log unavailable (no journalctl/dmesg)".into(),
        }];
    };
    if !out.status.success() {
        return vec![LogLine {
            text: "system log unavailable".into(),
        }];
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let collected: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = collected.len().saturating_sub(limit);
    collected[start..]
        .iter()
        .map(|l| LogLine {
            text: truncate_log(l, 240),
        })
        .collect()
}

fn truncate_log(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let mut out: String = t.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_log;

    #[test]
    fn truncate_preserves_short_lines() {
        assert_eq!(truncate_log("  hello  ", 10), "hello");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        let s = truncate_log("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(s.chars().count(), 10);
        assert!(s.ends_with('…'));
    }
}
