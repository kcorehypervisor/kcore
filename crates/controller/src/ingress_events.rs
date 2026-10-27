//! Webhook events for the ingress gateway.
//!
//! WAF detections can arrive by the thousand, so `ingress.waf.blocked` and
//! `ingress.waf.detected` are aggregated: one event per route and action per
//! window, carrying the count, the top rules and the top clients.
//! `ingress.vip.moved` fires when the node holding a gateway's VIP changes.

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};

use serde_json::{json, Value};

use crate::webhooks::{
    Dispatcher, EVENT_INGRESS_VIP_MOVED, EVENT_INGRESS_WAF_BLOCKED, EVENT_INGRESS_WAF_DETECTED,
};

/// How long events for one route and action are collected before one webhook.
pub const WINDOW_SECS: i64 = 60;
const TOP_N: usize = 5;
/// A window stops growing its distinct-key maps past this, so a flood of
/// spoofed addresses cannot grow memory without bound.
const MAX_DISTINCT: usize = 1024;

#[derive(Debug, Default)]
struct Window {
    start: i64,
    gateway: String,
    count: u64,
    rules: BTreeMap<i32, u64>,
    clients: BTreeMap<String, u64>,
}

/// A window ready to be sent.
#[derive(Debug, Clone, PartialEq)]
pub struct WafSummary {
    pub event_type: &'static str,
    pub route: String,
    pub data: Value,
}

#[derive(Debug, Default)]
pub struct WafAggregator {
    windows: HashMap<(String, bool), Window>,
}

impl WafAggregator {
    /// Counts one event. `blocked` selects `ingress.waf.blocked`, otherwise
    /// `ingress.waf.detected`.
    pub fn record(
        &mut self,
        gateway: &str,
        route: &str,
        blocked: bool,
        rule_id: i32,
        client: &str,
        now: i64,
    ) {
        let w = self
            .windows
            .entry((route.to_string(), blocked))
            .or_insert_with(|| Window {
                start: now,
                gateway: gateway.to_string(),
                ..Default::default()
            });
        w.count += 1;
        if w.rules.contains_key(&rule_id) || w.rules.len() < MAX_DISTINCT {
            *w.rules.entry(rule_id).or_default() += 1;
        }
        if w.clients.contains_key(client) || w.clients.len() < MAX_DISTINCT {
            *w.clients.entry(client.to_string()).or_default() += 1;
        }
    }

    /// Removes and returns the windows that are at least [`WINDOW_SECS`] old.
    pub fn drain(&mut self, now: i64) -> Vec<WafSummary> {
        let ready: Vec<(String, bool)> = self
            .windows
            .iter()
            .filter(|(_, w)| now - w.start >= WINDOW_SECS)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out: Vec<WafSummary> = ready
            .into_iter()
            .filter_map(|k| self.windows.remove(&k).map(|w| (k, w)))
            .map(|((route, blocked), w)| WafSummary {
                event_type: if blocked {
                    EVENT_INGRESS_WAF_BLOCKED
                } else {
                    EVENT_INGRESS_WAF_DETECTED
                },
                data: json!({
                    "gateway": w.gateway,
                    "route": route,
                    "count": w.count,
                    "windowSeconds": now - w.start,
                    "topRules": top(&w.rules, |id| json!(id), "ruleId"),
                    "topClients": top(&w.clients, |c| json!(c), "clientIp"),
                }),
                route,
            })
            .collect();
        out.sort_by(|a, b| (&a.route, a.event_type).cmp(&(&b.route, b.event_type)));
        out
    }
}

fn top<K: Ord, F: Fn(&K) -> Value>(m: &BTreeMap<K, u64>, key: F, name: &str) -> Vec<Value> {
    let mut items: Vec<(&K, &u64)> = m.iter().collect();
    items.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    items
        .into_iter()
        .take(TOP_N)
        .map(|(k, n)| json!({ name: key(k), "count": n }))
        .collect()
}

/// Process-wide aggregator: the gRPC handler records, the reconciler drains.
static AGGREGATOR: LazyLock<Mutex<WafAggregator>> = LazyLock::new(Default::default);

pub fn record_waf_event(gateway: &str, route: &str, blocked: bool, rule_id: i32, client: &str) {
    if route.is_empty() {
        return;
    }
    let mut agg = AGGREGATOR.lock().unwrap_or_else(|e| e.into_inner());
    agg.record(gateway, route, blocked, rule_id, client, now_unix());
}

/// Sends every window that has closed.
pub fn flush_waf_events(webhooks: &Dispatcher) {
    let ready = AGGREGATOR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(now_unix());
    for s in ready {
        webhooks.emit(s.event_type, format!("ingress-route/{}", s.route), s.data);
    }
}

/// The node that holds the VIP, from per-node status. `None` when nobody does.
pub fn vip_holder(nodes: &[crate::ingress_reconciler::NodeStatusJson]) -> Option<(String, String)> {
    nodes
        .iter()
        .find(|n| !n.vip_holder.is_empty())
        .map(|n| (n.node_id.clone(), n.vip_holder.clone()))
}

/// Emits `ingress.vip.moved` when the holder changed from one node to another.
/// A first observation (no previous holder) and a VIP nobody holds are not moves.
pub fn emit_vip_move(
    webhooks: &Dispatcher,
    gateway: &str,
    previous: Option<&(String, String)>,
    current: Option<&(String, String)>,
) -> bool {
    let (Some((from, _)), Some((to, address))) = (previous, current) else {
        return false;
    };
    if from == to {
        return false;
    }
    webhooks.emit(
        EVENT_INGRESS_VIP_MOVED,
        format!("ingress-gateway/{gateway}"),
        json!({"gateway": gateway, "from": from, "to": to, "address": address}),
    );
    true
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress_reconciler::NodeStatusJson;

    #[test]
    fn events_are_aggregated_per_route_and_action() {
        let mut a = WafAggregator::default();
        for i in 0..10 {
            a.record("edge", "shop", true, 942100, "198.51.100.7", 1000 + i);
        }
        a.record("edge", "shop", true, 941100, "203.0.113.9", 1010);
        a.record("edge", "shop", false, 920350, "198.51.100.7", 1011);
        a.record("edge", "blog", true, 942100, "198.51.100.8", 1012);
        // Nothing is ready before the window closes.
        assert!(a.drain(1030).is_empty());
        let out = a.drain(1100);
        assert_eq!(out.len(), 3);
        let blocked_shop = out
            .iter()
            .find(|s| s.route == "shop" && s.event_type == EVENT_INGRESS_WAF_BLOCKED)
            .unwrap();
        assert_eq!(blocked_shop.data["count"], 11);
        assert_eq!(blocked_shop.data["gateway"], "edge");
        assert_eq!(
            blocked_shop.data["topRules"][0],
            json!({"ruleId": 942100, "count": 10})
        );
        assert_eq!(
            blocked_shop.data["topClients"][0]["clientIp"],
            "198.51.100.7"
        );
        assert!(out
            .iter()
            .any(|s| s.route == "shop" && s.event_type == EVENT_INGRESS_WAF_DETECTED));
        // Drained windows are gone; a new event starts a new window.
        assert!(a.drain(2000).is_empty());
        a.record("edge", "shop", true, 1, "x", 2000);
        assert!(a.drain(2000).is_empty());
        assert_eq!(a.drain(2060).len(), 1);
    }

    #[test]
    fn distinct_keys_are_bounded() {
        let mut a = WafAggregator::default();
        for i in 0..(MAX_DISTINCT as i32 + 500) {
            a.record(
                "edge",
                "shop",
                true,
                i,
                &format!("10.0.{}.{}", i / 250, i % 250),
                0,
            );
        }
        let out = a.drain(WINDOW_SECS);
        assert_eq!(out[0].data["count"], MAX_DISTINCT as u64 + 500);
        assert_eq!(out[0].data["topRules"].as_array().unwrap().len(), TOP_N);
    }

    fn node(id: &str, holder: &str) -> NodeStatusJson {
        NodeStatusJson {
            node_id: id.into(),
            vip_holder: holder.into(),
            ..Default::default()
        }
    }

    #[test]
    fn vip_holder_is_the_node_serving_the_address() {
        assert_eq!(
            vip_holder(&[node("a", ""), node("b", "203.0.113.10")]),
            Some(("b".into(), "203.0.113.10".into()))
        );
        assert_eq!(vip_holder(&[node("a", "")]), None);
    }

    #[test]
    fn only_a_change_between_two_holders_is_a_move() {
        let d = Dispatcher::noop();
        let a = Some(("a".to_string(), "203.0.113.10".to_string()));
        let b = Some(("b".to_string(), "203.0.113.10".to_string()));
        assert!(emit_vip_move(&d, "edge", a.as_ref(), b.as_ref()));
        assert!(!emit_vip_move(&d, "edge", a.as_ref(), a.as_ref()));
        assert!(!emit_vip_move(&d, "edge", None, b.as_ref()));
        assert!(!emit_vip_move(&d, "edge", a.as_ref(), None));
    }
}
