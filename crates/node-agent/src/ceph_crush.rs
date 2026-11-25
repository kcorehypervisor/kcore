//! Pure helpers that plan `ceph osd crush …` argv for topology above host.

/// One host's desired CRUSH placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrushHost {
    pub host: String,
    pub rack: String,
    pub room: String,
}

/// Normalized failure domain.
pub fn normalize_failure_domain(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "rack" => "rack",
        "room" => "room",
        _ => "host",
    }
}

pub fn crush_rule_name(failure_domain: &str) -> String {
    let domain = normalize_failure_domain(failure_domain);
    if domain == "host" {
        String::new()
    } else {
        format!("kcore-{domain}")
    }
}

/// Idempotent command plans: add-bucket (ignore exists), move host under rack/room, create rule.
pub fn crush_topology_commands(
    failure_domain: &str,
    hosts: &[CrushHost],
    stretch: bool,
    stretch_tiebreaker: &str,
) -> Result<(String, Vec<Vec<String>>), String> {
    let domain = normalize_failure_domain(failure_domain);
    let mut cmds: Vec<Vec<String>> = Vec::new();
    if domain == "host" && !stretch {
        return Ok((String::new(), cmds));
    }
    let rule = crush_rule_name(domain);
    for h in hosts {
        let host = h.host.trim();
        if host.is_empty() {
            return Err("crush host name is required".into());
        }
        if domain == "rack" || domain == "room" {
            let rack = h.rack.trim();
            if rack.is_empty() && domain == "rack" {
                return Err(format!("rack required for host {host}"));
            }
            if !rack.is_empty() {
                cmds.push(vec![
                    "osd".into(),
                    "crush".into(),
                    "add-bucket".into(),
                    rack.to_string(),
                    "rack".into(),
                ]);
            }
        }
        if domain == "room" {
            let room = h.room.trim();
            if room.is_empty() {
                return Err(format!("room required for host {host}"));
            }
            cmds.push(vec![
                "osd".into(),
                "crush".into(),
                "add-bucket".into(),
                room.to_string(),
                "room".into(),
            ]);
            let rack = h.rack.trim();
            if !rack.is_empty() {
                cmds.push(vec![
                    "osd".into(),
                    "crush".into(),
                    "move".into(),
                    rack.to_string(),
                    format!("room={room}"),
                ]);
                cmds.push(vec![
                    "osd".into(),
                    "crush".into(),
                    "move".into(),
                    host.to_string(),
                    format!("rack={rack}"),
                ]);
            } else {
                cmds.push(vec![
                    "osd".into(),
                    "crush".into(),
                    "move".into(),
                    host.to_string(),
                    format!("room={room}"),
                ]);
            }
        } else if domain == "rack" {
            let rack = h.rack.trim();
            cmds.push(vec![
                "osd".into(),
                "crush".into(),
                "move".into(),
                host.to_string(),
                format!("rack={rack}"),
            ]);
        }
    }
    if !rule.is_empty() {
        cmds.push(vec![
            "osd".into(),
            "crush".into(),
            "rule".into(),
            "create-replicated".into(),
            rule.clone(),
            "default".into(),
            domain.to_string(),
        ]);
    }
    if stretch {
        let tb = stretch_tiebreaker.trim();
        if tb.is_empty() {
            return Err("stretch requires a tiebreaker mon id".into());
        }
        // Ceph stretch: enable after buckets exist; rooms are the dividing buckets.
        cmds.push(vec![
            "mon".into(),
            "enable_stretch_mode".into(),
            tb.to_string(),
            "default".into(),
            "room".into(),
        ]);
    }
    Ok((rule, cmds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_domain_is_noop() {
        let (rule, cmds) = crush_topology_commands(
            "host",
            &[CrushHost {
                host: "n1".into(),
                rack: "r1".into(),
                room: String::new(),
            }],
            false,
            "",
        )
        .unwrap();
        assert!(rule.is_empty());
        assert!(cmds.is_empty());
    }

    #[test]
    fn rack_domain_moves_hosts_and_creates_rule() {
        let (rule, cmds) = crush_topology_commands(
            "rack",
            &[CrushHost {
                host: "n1".into(),
                rack: "rack-a".into(),
                room: String::new(),
            }],
            false,
            "",
        )
        .unwrap();
        assert_eq!(rule, "kcore-rack");
        assert!(cmds
            .iter()
            .any(|c| c.join(" ") == "osd crush add-bucket rack-a rack"));
        assert!(cmds
            .iter()
            .any(|c| c.join(" ") == "osd crush move n1 rack=rack-a"));
        assert!(cmds.iter().any(|c| {
            c.join(" ") == "osd crush rule create-replicated kcore-rack default rack"
        }));
    }

    #[test]
    fn room_domain_nests_rack_under_room() {
        let (rule, cmds) = crush_topology_commands(
            "room",
            &[CrushHost {
                host: "n1".into(),
                rack: "rack-a".into(),
                room: "room-1".into(),
            }],
            false,
            "",
        )
        .unwrap();
        assert_eq!(rule, "kcore-room");
        assert!(cmds
            .iter()
            .any(|c| c.join(" ") == "osd crush move rack-a room=room-1"));
    }
}
