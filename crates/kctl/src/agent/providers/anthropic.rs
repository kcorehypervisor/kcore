use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

use super::{error_detail, http, ChatMessage, Emit, ModelEvent, Splitter, StreamRequest, ToolCall};

const VERSION: &str = "2023-06-01";
const FALLBACK_MODEL: &str = "claude-sonnet-4-5";

/// Messages API streaming. API key only: Anthropic does not allow Claude.ai logins in third-party apps.
pub struct Anthropic {
    base_url: String,
    api_key: String,
    max_tokens: u32,
    client: reqwest::Client,
}

impl Anthropic {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        Self {
            base_url: base_url
                .unwrap_or("https://api.anthropic.com/v1")
                .trim_end_matches('/')
                .to_string(),
            api_key: api_key.to_string(),
            max_tokens: 8192,
            client: http(),
        }
    }

    fn headers(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", VERSION)
    }

    pub async fn models(&self) -> Vec<String> {
        let fetched = async {
            let response = self
                .headers(
                    self.client
                        .get(format!("{}/models?limit=100", self.base_url)),
                )
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            let body: Value = response.json().await.ok()?;
            let ids: Vec<String> = body["data"]
                .as_array()?
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect();
            (!ids.is_empty()).then_some(ids)
        };
        fetched
            .await
            .unwrap_or_else(|| vec![FALLBACK_MODEL.to_string()])
    }

    pub async fn stream(&self, request: StreamRequest<'_>, emit: Emit<'_>) -> Result<()> {
        let system: Vec<&str> = request
            .messages
            .iter()
            .filter_map(|m| match m {
                ChatMessage::System { content } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        let mut body = json!({
            "model": request.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "messages": to_anthropic_messages(request.messages),
        });
        if !system.is_empty() {
            body["system"] = json!(system.join("\n\n"));
        }
        if !request.tools.is_empty() {
            body["tools"] = request
                .tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters }))
                .collect();
        }
        let response = self
            .headers(self.client.post(format!("{}/messages", self.base_url)))
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(anyhow!(
                "anthropic returned {status}: {}",
                error_detail(response).await
            ));
        }

        let mut blocks: HashMap<i64, (String, String, String)> = HashMap::new();
        let mut events = Splitter::new(b"\n\n");
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            for raw in events.push(&chunk?) {
                let data: String = raw
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim)
                    .collect();
                if data.is_empty() {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                let index = event["index"].as_i64().unwrap_or(0);
                match event["type"].as_str().unwrap_or("") {
                    "content_block_start" if event["content_block"]["type"] == "tool_use" => {
                        blocks.insert(
                            index,
                            (
                                event["content_block"]["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                event["content_block"]["name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                String::new(),
                            ),
                        );
                    }
                    "content_block_delta" => {
                        let delta = &event["delta"];
                        if delta["type"] == "text_delta" {
                            if let Some(text) = delta["text"].as_str().filter(|t| !t.is_empty()) {
                                emit(ModelEvent::Text(text.to_string()));
                            }
                        }
                        if delta["type"] == "input_json_delta" {
                            if let Some(block) = blocks.get_mut(&index) {
                                block
                                    .2
                                    .push_str(delta["partial_json"].as_str().unwrap_or(""));
                            }
                        }
                    }
                    "content_block_stop" => {
                        if let Some((id, name, json)) = blocks.remove(&index) {
                            emit(ModelEvent::ToolCall(ToolCall {
                                id,
                                name,
                                arguments: if json.is_empty() {
                                    "{}".to_string()
                                } else {
                                    json
                                },
                            }));
                        }
                    }
                    "error" => {
                        return Err(anyhow!(
                            "anthropic stream error: {}",
                            event["error"]["message"].as_str().unwrap_or("unknown")
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

pub fn to_anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        match message {
            ChatMessage::System { .. } => {}
            ChatMessage::User { content } => {
                out.push(json!({ "role": "user", "content": content }))
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = content.as_deref().filter(|t| !t.is_empty()) {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for call in tool_calls.iter().flatten() {
                    let arguments = if call.function.arguments.is_empty() {
                        "{}"
                    } else {
                        &call.function.arguments
                    };
                    let input: Value =
                        serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({ "type": "tool_use", "id": call.id, "name": call.function.name, "input": input }));
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                let result = json!({ "type": "tool_result", "tool_use_id": tool_call_id, "content": content });
                let appended = match out.last_mut() {
                    Some(last)
                        if last["role"] == "user"
                            && last["content"].as_array().is_some_and(|blocks| {
                                blocks.iter().all(|b| b["type"] == "tool_result")
                            }) =>
                    {
                        last["content"].as_array_mut().unwrap().push(result.clone());
                        true
                    }
                    _ => false,
                };
                if !appended {
                    out.push(json!({ "role": "user", "content": [result] }));
                }
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
    fn tool_results_are_grouped_into_one_user_turn() {
        let call = |id: &str| AssistantCall {
            id: id.into(),
            kind: "function".into(),
            function: AssistantFunction {
                name: "kcore_read".into(),
                arguments: "{\"kind\":\"vm\"}".into(),
            },
        };
        let messages = vec![
            ChatMessage::System {
                content: "sys".into(),
            },
            ChatMessage::User {
                content: "list".into(),
            },
            ChatMessage::Assistant {
                content: Some("ok".into()),
                tool_calls: Some(vec![call("a"), call("b")]),
            },
            ChatMessage::Tool {
                tool_call_id: "a".into(),
                content: "1".into(),
            },
            ChatMessage::Tool {
                tool_call_id: "b".into(),
                content: "2".into(),
            },
        ];
        let out = to_anthropic_messages(&messages);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1]["content"][0]["type"], "text");
        assert_eq!(out[1]["content"][1]["input"]["kind"], "vm");
        assert_eq!(out[2]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn bad_tool_arguments_become_an_empty_input() {
        let messages = vec![ChatMessage::Assistant {
            content: None,
            tool_calls: Some(vec![AssistantCall {
                id: "a".into(),
                kind: "function".into(),
                function: AssistantFunction {
                    name: "x".into(),
                    arguments: "{oops".into(),
                },
            }]),
        }];
        assert_eq!(
            to_anthropic_messages(&messages)[0]["content"][0]["input"],
            json!({})
        );
    }
}
