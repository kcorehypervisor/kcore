//! Sessions are append-only JSON lines, so a crash loses at most the line being written.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::providers::ChatMessage;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EventBody {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        call_id: String,
        ok: bool,
        text: String,
        ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<AppRef>,
    },
    Form {
        form_id: String,
        kind: String,
        message: String,
        schema: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    FormDone {
        form_id: String,
        action: String,
    },
    Error {
        message: String,
    },
    TurnStart {
        provider: String,
        model: String,
    },
    TurnEnd {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRef {
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub id: u64,
    pub ts: i64,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum TranscriptRecord {
    Meta { created_at: i64, title: String },
    Event { e: AgentEvent },
    Message { m: ChatMessage },
}

pub struct Restored {
    pub id: String,
    pub created_at: i64,
    pub title: String,
    pub events: Vec<AgentEvent>,
    pub messages: Vec<ChatMessage>,
}

const INTERRUPTED: &str =
    "The agent restarted before this call finished. Its result is unknown: check the cluster before retrying.";

/// Rebuilds a session from its jsonl log. A torn last line from a crash is skipped.
pub fn read_transcript(dir: &Path, id: &str) -> Option<Restored> {
    let text = std::fs::read_to_string(dir.join(format!("{id}.jsonl"))).ok()?;
    let mut created_at = 0;
    let mut title = "New session".to_string();
    let mut events = Vec::new();
    let mut messages = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(record) = serde_json::from_str::<TranscriptRecord>(line) else {
            continue;
        };
        match record {
            TranscriptRecord::Meta {
                created_at: at,
                title: t,
            } => {
                created_at = at;
                title = t;
            }
            TranscriptRecord::Event { e } => events.push(e),
            TranscriptRecord::Message { m } => messages.push(m),
        }
    }
    if events.is_empty() && messages.is_empty() {
        return None;
    }
    let created_at = if created_at != 0 {
        created_at
    } else {
        events.first().map_or_else(super::util::now_ms, |e| e.ts)
    };
    Some(Restored {
        id: id.to_string(),
        created_at,
        title,
        events,
        messages: repair_messages(messages),
    })
}

fn is_session_id(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    stem.len() == 36
        && stem
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase() || c == '-')
}

pub fn list_transcripts(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| is_session_id(name))
        .map(|name| name.trim_end_matches(".jsonl").to_string())
        .collect();
    ids.sort();
    ids
}

/// A model API refuses an assistant tool call with no matching result, so close any a crash left open.
pub fn repair_messages(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let message = messages[i].clone();
        i += 1;
        let ChatMessage::Assistant {
            tool_calls: Some(calls),
            ..
        } = &message
        else {
            out.push(message);
            continue;
        };
        let calls = calls.clone();
        out.push(message);
        let mut answered = Vec::new();
        while let Some(ChatMessage::Tool { tool_call_id, .. }) = messages.get(i) {
            answered.push(tool_call_id.clone());
            out.push(messages[i].clone());
            i += 1;
        }
        for call in calls {
            if !answered.contains(&call.id) {
                out.push(ChatMessage::Tool {
                    tool_call_id: call.id,
                    content: INTERRUPTED.to_string(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::providers::{AssistantCall, AssistantFunction};

    #[test]
    fn events_keep_the_flat_wire_shape() {
        let event = AgentEvent {
            id: 3,
            ts: 9,
            body: EventBody::ToolResult {
                call_id: "c1".into(),
                ok: true,
                text: "t".into(),
                ms: 4,
                app: Some(AppRef {
                    uri: "ui://kcore/plan".into(),
                }),
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "id": 3, "ts": 9, "type": "tool_result", "callId": "c1", "ok": true, "text": "t", "ms": 4, "app": { "uri": "ui://kcore/plan" } })
        );
        assert_eq!(serde_json::from_value::<AgentEvent>(value).unwrap(), event);
        let form: AgentEvent = serde_json::from_str(r#"{"id":3,"ts":3,"type":"form","formId":"f4","kind":"approval","message":"Delete?","schema":{"type":"object","properties":{}}}"#).unwrap();
        assert!(
            matches!(form.body, EventBody::Form { ref form_id, call_id: None, .. } if form_id == "f4")
        );
    }

    #[test]
    fn open_tool_calls_are_closed_and_answered_ones_are_left_alone() {
        let call = |id: &str| AssistantCall {
            id: id.into(),
            kind: "function".into(),
            function: AssistantFunction {
                name: "x".into(),
                arguments: "{}".into(),
            },
        };
        let messages = vec![
            ChatMessage::User {
                content: "go".into(),
            },
            ChatMessage::Assistant {
                content: None,
                tool_calls: Some(vec![call("a"), call("b")]),
            },
            ChatMessage::Tool {
                tool_call_id: "a".into(),
                content: "done".into(),
            },
        ];
        let repaired = repair_messages(messages.clone());
        assert_eq!(repaired.len(), 4);
        assert!(
            matches!(&repaired[3], ChatMessage::Tool { tool_call_id, content } if tool_call_id == "b" && content.contains("restarted"))
        );
        assert_eq!(repair_messages(repaired.clone()).len(), repaired.len());
    }

    #[test]
    fn only_uuid_named_jsonl_files_are_sessions() {
        assert!(is_session_id("11111111-2222-3333-4444-555555555555.jsonl"));
        assert!(!is_session_id("ignored.txt"));
        assert!(!is_session_id("../../etc/passwd.jsonl"));
        assert!(!is_session_id("11111111-2222-3333-4444-55555555555G.jsonl"));
    }
}
