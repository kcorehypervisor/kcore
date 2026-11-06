use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

use super::{error_detail, http, Emit, ModelEvent, Splitter, StreamRequest, ToolCall};

/// Chat Completions streaming. OpenAI API keys, DeepSeek and OpenCode all speak this.
pub struct OpenAiCompatible {
    pub id: String,
    base_url: String,
    api_key: String,
    fallback_models: Vec<String>,
    models_url: String,
    client: reqwest::Client,
}

impl OpenAiCompatible {
    pub fn new(
        id: &str,
        base_url: &str,
        api_key: &str,
        fallback_models: &[&str],
        models_url: Option<&str>,
    ) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            id: id.to_string(),
            models_url: models_url
                .map(str::to_string)
                .unwrap_or_else(|| format!("{base_url}/models")),
            base_url,
            api_key: api_key.to_string(),
            fallback_models: fallback_models.iter().map(|m| m.to_string()).collect(),
            client: http(),
        }
    }

    pub async fn models(&self) -> Vec<String> {
        let fetched = async {
            let response = self
                .client
                .get(&self.models_url)
                .bearer_auth(&self.api_key)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            let body: Value = response.json().await.ok()?;
            let mut ids: Vec<String> = body["data"]
                .as_array()?
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect();
            ids.sort();
            (!ids.is_empty()).then_some(ids)
        };
        fetched
            .await
            .unwrap_or_else(|| self.fallback_models.clone())
    }

    pub async fn stream(&self, request: StreamRequest<'_>, emit: Emit<'_>) -> Result<()> {
        let mut body = json!({
            "model": request.model,
            "messages": request.messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !request.tools.is_empty() {
            body["tools"] = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": { "name": tool.name, "description": tool.description, "parameters": tool.parameters },
                    })
                })
                .collect();
        }
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(anyhow!(
                "{} returned {status}: {}",
                self.id,
                error_detail(response).await
            ));
        }

        let mut calls: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
        let mut lines = Splitter::new(b"\n");
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            for line in lines.push(&chunk?) {
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                let Ok(parsed) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let delta = &parsed["choices"][0]["delta"];
                if delta.is_null() {
                    continue;
                }
                if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                    emit(ModelEvent::Text(text.to_string()));
                }
                for piece in delta["tool_calls"].as_array().into_iter().flatten() {
                    let index = piece["index"].as_i64().unwrap_or(0);
                    let slot = calls.entry(index).or_default();
                    if let Some(id) = piece["id"].as_str().filter(|i| !i.is_empty()) {
                        slot.0 = id.to_string();
                    }
                    if let Some(name) = piece["function"]["name"].as_str() {
                        slot.1.push_str(name);
                    }
                    if let Some(arguments) = piece["function"]["arguments"].as_str() {
                        slot.2.push_str(arguments);
                    }
                }
            }
        }
        for (index, (id, name, arguments)) in calls {
            emit(ModelEvent::ToolCall(ToolCall {
                id: if id.is_empty() {
                    format!("call_{index}")
                } else {
                    id
                },
                name,
                arguments: if arguments.is_empty() {
                    "{}".to_string()
                } else {
                    arguments
                },
            }));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::providers::ChatMessage;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_once(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 16384];
            let _ = socket.read(&mut buffer).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn streamed_text_and_tool_calls_are_assembled() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"kcore_read\",\"arguments\":\"{\\\"kind\\\"\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"vm\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n";
        let response: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_boxed_str(),
        );
        let base = serve_once(response).await;
        let provider = OpenAiCompatible::new("test", &base, "key", &[], None);
        let mut events = Vec::new();
        let messages = [ChatMessage::User {
            content: "hi".into(),
        }];
        provider
            .stream(
                StreamRequest {
                    model: "m",
                    messages: &messages,
                    tools: &[],
                },
                &mut |event| events.push(event),
            )
            .await
            .unwrap();
        assert_eq!(events[0], ModelEvent::Text("Hel".into()));
        assert_eq!(events[1], ModelEvent::Text("lo".into()));
        assert_eq!(
            events[2],
            ModelEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "kcore_read".into(),
                arguments: "{\"kind\":\"vm\"}".into()
            })
        );
    }

    #[tokio::test]
    async fn an_error_response_names_the_provider_and_status() {
        let base = serve_once(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 9\r\nconnection: close\r\n\r\nbad token",
        )
        .await;
        let provider = OpenAiCompatible::new("deepseek", &base, "key", &[], None);
        let messages = [ChatMessage::User {
            content: "hi".into(),
        }];
        let error = provider
            .stream(
                StreamRequest {
                    model: "m",
                    messages: &messages,
                    tools: &[],
                },
                &mut |_| {},
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("deepseek returned 401: bad token"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn models_fall_back_when_the_list_cannot_be_read() {
        let base =
            serve_once("HTTP/1.1 500 Oops\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
        let provider = OpenAiCompatible::new("x", &base, "key", &["a", "b"], None);
        assert_eq!(provider.models().await, vec!["a", "b"]);
    }
}
