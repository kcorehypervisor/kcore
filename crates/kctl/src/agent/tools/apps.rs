//! MCP Apps (`io.modelcontextprotocol/ui`, spec 2026-01-26): HTML the host can show next to a tool result.

pub const MCP_APP_MIME: &str = "text/html;profile=mcp-app";
pub const PLAN_APP_URI: &str = "ui://kcore/plan";
pub const DATA_APP_URI: &str = "ui://kcore/data";

/// The plan view. It makes no network requests and loads nothing from outside: the host serves it
/// with a CSP that blocks everything but inline script and style, so the only way in or out is
/// postMessage to the host. Every value from the plan is put on the page with textContent.
pub const PLAN_APP_HTML: &str = include_str!("../../../assets/agent/plan-app.html");

/// Tables, status pills and detail rows for what the controller returned (nodes, VMs, volumes and
/// so on). Same sandbox rules as the plan view.
pub const DATA_APP_HTML: &str = include_str!("../../../assets/agent/data-app.html");

/// The app a tool result can be shown with, if any.
pub fn app_for_tool(name: &str) -> Option<&'static str> {
    match name {
        "kcore_plan" => Some(PLAN_APP_URI),
        "kcore_read" | "kcore_rpc" => Some(DATA_APP_URI),
        _ => None,
    }
}

/// HTML for an app URI this build serves.
pub fn read_app(uri: &str) -> Option<&'static str> {
    match uri {
        PLAN_APP_URI => Some(PLAN_APP_HTML),
        DATA_APP_URI => Some(DATA_APP_HTML),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn the_apps_talk_only_to_their_host_and_load_nothing() {
        let forbidden = Regex::new(r"<script[^>]*\ssrc=|<link|<img|<iframe|fetch\(|XMLHttpRequest|WebSocket|innerHTML|eval\(").unwrap();
        let urls = Regex::new(r"https?://").unwrap();
        for html in [PLAN_APP_HTML, DATA_APP_HTML] {
            assert!(!urls.is_match(html));
            assert!(!forbidden.is_match(html));
            assert!(html.contains("ui/initialize"));
            assert!(html.contains("ui/notifications/initialized"));
            assert!(html.contains("2026-01-26"));
        }
    }

    #[test]
    fn plan_and_read_tools_have_apps_and_changes_do_not() {
        assert_eq!(app_for_tool("kcore_plan"), Some(PLAN_APP_URI));
        assert_eq!(app_for_tool("kcore_read"), Some(DATA_APP_URI));
        assert_eq!(app_for_tool("kcore_rpc"), Some(DATA_APP_URI));
        assert_eq!(app_for_tool("kcore_apply"), None);
        assert_eq!(read_app(PLAN_APP_URI), Some(PLAN_APP_HTML));
        assert_eq!(read_app(DATA_APP_URI), Some(DATA_APP_HTML));
        assert_eq!(read_app("ui://kcore/other"), None);
    }
}
