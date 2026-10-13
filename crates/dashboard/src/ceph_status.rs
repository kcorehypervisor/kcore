//! Parse stored `ceph -s` JSON and build simple SVG pie charts for capacity.

use crate::dto::{
    CephCapacityDto, CephCheckDto, CephDeviceClassDto, CephHealthDetailDto, CephPgStateDto,
};
use crate::format;

/// Typed view of the reconciler-cached `ceph -s --format json` blob.
pub fn health_from_status_json(raw: &str) -> CephHealthDetailDto {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
        return CephHealthDetailDto::default();
    };

    let health_status = json
        .pointer("/health/status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let quorum_names: Vec<String> = json
        .get("quorum_names")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let num_mons = json
        .pointer("/monmap/num_mons")
        .and_then(|v| v.as_i64())
        .unwrap_or(quorum_names.len() as i64) as i32;
    let mon_quorum = if quorum_names.is_empty() {
        String::new()
    } else {
        format!(
            "{} ({}/{})",
            quorum_names.join(","),
            quorum_names.len(),
            num_mons
        )
    };

    let osd_total = json
        .pointer("/osdmap/num_osds")
        .or_else(|| json.pointer("/osdmap/osdmap/num_osds"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let osd_up = json
        .pointer("/osdmap/num_up_osds")
        .or_else(|| json.pointer("/osdmap/osdmap/num_up_osds"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let osd_in = json
        .pointer("/osdmap/num_in_osds")
        .or_else(|| json.pointer("/osdmap/osdmap/num_in_osds"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let pgs_total = json
        .pointer("/pgmap/num_pgs")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let mut pgs_by_state = Vec::new();
    if let Some(states) = json
        .pointer("/pgmap/pgs_by_state")
        .and_then(|v| v.as_array())
    {
        for s in states {
            let Some(state_name) = s.get("state_name").and_then(|v| v.as_str()) else {
                continue;
            };
            let count = s.get("count").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            pgs_by_state.push(CephPgStateDto {
                state_name: state_name.to_string(),
                count,
            });
        }
        pgs_by_state.sort_by(|a, b| b.count.cmp(&a.count).then(a.state_name.cmp(&b.state_name)));
    }

    let flags = ceph_flags(&json);
    let noout = flags.iter().any(|f| f == "noout");
    let norebalance = flags.iter().any(|f| f == "norebalance");

    let mut checks = Vec::new();
    if let Some(obj) = json.pointer("/health/checks").and_then(|c| c.as_object()) {
        let mut names: Vec<&String> = obj.keys().collect();
        names.sort();
        for name in names {
            let severity = obj[name]
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let message = obj[name]
                .pointer("/summary/message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if message.is_empty() {
                continue;
            }
            checks.push(CephCheckDto {
                name: name.clone(),
                severity,
                message,
            });
        }
    }

    CephHealthDetailDto {
        health_status,
        mon_quorum,
        osd_total,
        osd_up,
        osd_in,
        pgs_total,
        pgs_by_state,
        noout,
        norebalance,
        flags,
        checks,
    }
}

/// Cluster capacity from `pgmap` bytes when `ceph df` is unavailable.
pub fn capacity_from_status_json(raw: &str) -> CephCapacityDto {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
        return CephCapacityDto::default();
    };
    let total = json
        .pointer("/pgmap/bytes_total")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let used = json
        .pointer("/pgmap/bytes_used")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let avail = json
        .pointer("/pgmap/bytes_avail")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if total <= 0 && used <= 0 && avail <= 0 {
        return CephCapacityDto::default();
    }
    capacity_dto(total, used, avail, Vec::new(), Vec::new(), "pgmap", "")
}

pub fn capacity_dto(
    total_bytes: i64,
    used_bytes: i64,
    avail_bytes: i64,
    by_class: Vec<CephDeviceClassDto>,
    pools: Vec<crate::dto::CephPoolUsageDto>,
    source: &str,
    error: &str,
) -> CephCapacityDto {
    let used_percent = if total_bytes > 0 {
        ((used_bytes as f64) * 100.0 / (total_bytes as f64)).round() as i32
    } else {
        0
    };
    CephCapacityDto {
        total_bytes,
        used_bytes,
        avail_bytes,
        total_human: format::bytes_human(total_bytes),
        used_human: format::bytes_human(used_bytes),
        avail_human: format::bytes_human(avail_bytes),
        used_percent,
        by_class,
        pools,
        source: source.to_string(),
        queried_node: String::new(),
        error: error.to_string(),
    }
}

fn ceph_flags(json: &serde_json::Value) -> Vec<String> {
    let mut flags = Vec::new();
    for pointer in ["/osdmap/flags", "/osdmap/flags_set", "/osdmap/osdmap/flags"] {
        match json.pointer(pointer) {
            Some(serde_json::Value::Array(items)) => {
                flags.extend(items.iter().filter_map(|f| f.as_str()).map(str::to_string));
            }
            Some(serde_json::Value::String(text)) => {
                flags.extend(
                    text.split([',', ' '])
                        .filter(|f| !f.is_empty())
                        .map(str::to_string),
                );
            }
            _ => {}
        }
    }
    flags.sort();
    flags.dedup();
    flags
}

const PIE_COLORS: &[&str] = &[
    "#5eead4", "#60a5fa", "#c084fc", "#fbbf24", "#fb923c", "#f472b6", "#34d399", "#94a3b8",
];

/// SVG pie chart for `(label, value)` slices. Values ≤ 0 are skipped.
pub fn pie_svg(slices: &[(String, i64)], title: &str) -> String {
    let active: Vec<(String, i64, &str)> = slices
        .iter()
        .filter(|(_, v)| *v > 0)
        .enumerate()
        .map(|(i, (label, value))| (label.clone(), *value, PIE_COLORS[i % PIE_COLORS.len()]))
        .collect();
    if active.is_empty() {
        // Avoid `format!` with CSS hex colors — `#` confuses the format parser
        // next to named placeholders in some Rust versions.
        return format!(
            concat!(
                "<svg class=\"ceph-pie\" viewBox=\"0 0 120 120\" role=\"img\" aria-label=\"{}\">",
                "<circle cx=\"60\" cy=\"60\" r=\"54\" fill=\"#151821\" stroke=\"#1e2433\" stroke-width=\"2\"/>",
                "<text x=\"60\" y=\"64\" text-anchor=\"middle\" fill=\"#8b93a7\" font-size=\"11\">n/a</text>",
                "</svg>"
            ),
            html_escape_attr(title)
        );
    }
    let total: f64 = active.iter().map(|(_, v, _)| *v as f64).sum();
    if total <= 0.0 {
        return pie_svg(&[], title);
    }
    let mut paths = String::new();
    let mut angle = -std::f64::consts::FRAC_PI_2;
    for (label, value, color) in &active {
        let sweep = (*value as f64 / total) * std::f64::consts::TAU;
        let end = angle + sweep;
        let large = if sweep > std::f64::consts::PI { 1 } else { 0 };
        let (x1, y1) = polar(60.0, 60.0, 54.0, angle);
        let (x2, y2) = polar(60.0, 60.0, 54.0, end);
        paths.push_str(&format!(
            "<path d=\"M60 60 L{x1:.3} {y1:.3} A54 54 0 {large} 1 {x2:.3} {y2:.3} Z\" fill=\"{color}\" data-label=\"{label}\"/>",
            x1 = x1,
            y1 = y1,
            large = large,
            x2 = x2,
            y2 = y2,
            color = color,
            label = html_escape_attr(label),
        ));
        angle = end;
    }
    format!(
        concat!(
            "<svg class=\"ceph-pie\" viewBox=\"0 0 120 120\" role=\"img\" aria-label=\"{}\">",
            "<circle cx=\"60\" cy=\"60\" r=\"54\" fill=\"#151821\"/>",
            "{}",
            "<circle cx=\"60\" cy=\"60\" r=\"28\" fill=\"#0f1118\"/>",
            "</svg>"
        ),
        html_escape_attr(title),
        paths
    )
}

fn html_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn polar(cx: f64, cy: f64, r: f64, angle: f64) -> (f64, f64) {
    (cx + r * angle.cos(), cy + r * angle.sin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_parses_quorum_osds_and_pgs() {
        let raw = r#"{
          "health": {"status": "HEALTH_WARN", "checks": {
            "OSDMAP_FLAGS": {"severity": "HEALTH_WARN", "summary": {"message": "noout flag(s) set"}}
          }},
          "quorum_names": ["a","b","c"],
          "monmap": {"num_mons": 3},
          "osdmap": {"num_osds": 6, "num_up_osds": 6, "num_in_osds": 6, "flags": ["noout"]},
          "pgmap": {
            "num_pgs": 32,
            "bytes_total": 1000,
            "bytes_used": 250,
            "bytes_avail": 750,
            "pgs_by_state": [
              {"state_name": "active+clean", "count": 30},
              {"state_name": "active+clean+scrubbing", "count": 2}
            ]
          }
        }"#;
        let h = health_from_status_json(raw);
        assert_eq!(h.health_status, "HEALTH_WARN");
        assert_eq!(h.mon_quorum, "a,b,c (3/3)");
        assert_eq!((h.osd_total, h.osd_up, h.osd_in), (6, 6, 6));
        assert_eq!(h.pgs_total, 32);
        assert!(h.noout);
        assert_eq!(h.checks.len(), 1);
        let cap = capacity_from_status_json(raw);
        assert_eq!(cap.total_bytes, 1000);
        assert_eq!(cap.used_percent, 25);
        assert_eq!(cap.source, "pgmap");
    }

    #[test]
    fn pie_svg_handles_empty_and_two_slices() {
        let empty = pie_svg(&[], "empty");
        assert!(empty.contains("n/a"));
        let chart = pie_svg(&[("used".into(), 25), ("avail".into(), 75)], "cap");
        assert!(chart.contains("<path"));
        assert!(chart.contains("#5eead4"));
    }
}
