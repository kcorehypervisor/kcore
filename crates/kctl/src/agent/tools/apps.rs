//! MCP Apps (`io.modelcontextprotocol/ui`, spec 2026-01-26): HTML the host can show next to a tool result.

pub const MCP_APP_MIME: &str = "text/html;profile=mcp-app";
pub const PLAN_APP_URI: &str = "ui://kcore/plan";

/// The plan view. It makes no network requests and loads nothing from outside: the host serves it
/// with a CSP that blocks everything but inline script and style, so the only way in or out is
/// postMessage to the host. Every value from the plan is put on the page with textContent.
pub const PLAN_APP_HTML: &str = include_str!("../../../assets/agent/plan-app.html");

/// The app a tool result can be shown with, if any.
pub fn app_for_tool(name: &str) -> Option<&'static str> {
    (name == "kcore_plan").then_some(PLAN_APP_URI)
}

/// HTML for an app URI this build serves.
pub fn read_app(uri: &str) -> Option<&'static str> {
    (uri == PLAN_APP_URI).then_some(PLAN_APP_HTML)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn the_plan_app_talks_only_to_its_host_and_loads_nothing() {
        assert!(!Regex::new(r"https?://").unwrap().is_match(PLAN_APP_HTML));
        let forbidden = Regex::new(r"<script[^>]*\ssrc=|<link|<img|<iframe|fetch\(|XMLHttpRequest|WebSocket|innerHTML|eval\(").unwrap();
        assert!(!forbidden.is_match(PLAN_APP_HTML));
        assert!(PLAN_APP_HTML.contains("ui/initialize"));
        assert!(PLAN_APP_HTML.contains("ui/notifications/initialized"));
        assert!(PLAN_APP_HTML.contains("2026-01-26"));
    }

    #[test]
    fn only_the_plan_tool_has_an_app() {
        assert_eq!(app_for_tool("kcore_plan"), Some(PLAN_APP_URI));
        assert_eq!(app_for_tool("kcore_apply"), None);
        assert_eq!(read_app(PLAN_APP_URI), Some(PLAN_APP_HTML));
        assert_eq!(read_app("ui://kcore/other"), None);
    }
}
