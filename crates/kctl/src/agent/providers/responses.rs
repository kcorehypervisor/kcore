use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::{error_detail, http, ChatMessage, Emit, ModelEvent, Splitter, StreamRequest, ToolCall};

/// Where the access token comes from. `force` asks for a fresh one after a 401.
pub trait TokenSource: Send + Sync {
    fn token(&self, force: bool) -> BoxFuture<'_, Result<String>>;
}

impl TokenSource for crate::agent::chatgpt::ChatGptAuth {
    fn token(&self, force: bool) -> BoxFuture<'_, Result<String>> {
        Box::pin(self.access_token(force))
    }
}

/// The Responses API as Sign in with ChatGPT allows it: `store: false`, `stream: true`, the whole
/// history in `input`, the system prompt in `instructions`, and function tools grouped in a namespace.
pub struct ChatGptProvider {
    base_url: String,
    tokens: Arc<dyn TokenSource>,
    namespace: String,
    client: reqwest::Client,
}

impl ChatGptProvider {
    pub fn new(tokens: Arc<dyn TokenSource>, base_url: Option<&str>) -> Self {
        Self {
            base_url: base_url
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/')
                .to_string(),
            tokens,
            namespace: "kcore".to_string(),
            client: http(),
        }
    }

    async fn authed(
        &self,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let first = build(&self.tokens.token(false).await?).send().await?;
        if first.status().as_u16() != 401 {
            return Ok(first);
        }
        Ok(build(&self.tokens.token(true).await?).send().await?)
    }

    pub async fn models(&self) -> Vec<String> {
        let fetched = async {
            let response = self
                .authed(|token| {
                    self.client
                        .get(format!("{}/models", self.base_url))
                        .bearer_auth(token)
                        .timeout(Duration::from_secs(15))
                })
                .await?;
            if !response.status().is_success() {
                bail!("model list answered {}", response.status().as_u16());
            }
            let body: Value = response.json().await?;
            Ok::<_, anyhow::Error>(
                body["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| m["visibility"] == "list")
                    .filter_map(|m| m["slug"].as_str().map(str::to_string))
                    .collect::<Vec<_>>(),
            )
        };
        fetched.await.unwrap_or_default()
    }

    pub async fn stream(&self, request: StreamRequest<'_>, emit: Emit<'_>) -> Result<()> {
        let instructions: Vec<&str> = request
            .messages
            .iter()
            .filter_map(|m| match m {
                ChatMessage::System { content } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        let mut body = json!({
            "model": request.model,
            "store": false,
            "stream": true,
            "input": to_responses_input(request.messages, &self.namespace),
        });
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions.join("\n\n"));
        }
        if !request.tools.is_empty() {
            body["tools"] = json!([{
                "type": "namespace",
                "name": self.namespace,
                "description": "kcore cluster management tools",
                "tools": request.tools.iter().map(|t| json!({
                    "type": "function", "name": t.name, "description": t.description, "parameters": t.parameters,
                })).collect::<Vec<_>>(),
            }]);
        }

        let response = self
            .authed(|token| {
                self.client
                    .post(format!("{}/responses", self.base_url))
                    .bearer_auth(token)
                    .header("accept", "text/event-stream")
                    .json(&body)
            })
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(anyhow!(
                "chatgpt returned {status}: {}",
                error_detail(response).await
            ));
        }

        let mut completed = false;
        let mut events = Splitter::new(b"\n\n");
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            for raw in events.push(&chunk?) {
                let data: String = raw
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim)
                    .collect();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                match event["type"].as_str().unwrap_or("") {
                    "response.output_text.delta" => {
                        if let Some(delta) = event["delta"].as_str().filter(|d| !d.is_empty()) {
                            emit(ModelEvent::Text(delta.to_string()));
                        }
                    }
                    "response.output_item.done" => {
                        let item = &event["item"];
                        if let (Some(id), Some(name)) = (
                            item["call_id"].as_str().filter(|i| !i.is_empty()),
                            item["name"].as_str().filter(|n| !n.is_empty()),
                        ) {
                            emit(ModelEvent::ToolCall(ToolCall {
                                id: id.to_string(),
                                name: name.to_string(),
                                arguments: item["arguments"]
                                    .as_str()
                                    .filter(|a| !a.is_empty())
                                    .unwrap_or("{}")
                                    .to_string(),
                            }));
                        }
                    }
                    "response.completed" => completed = true,
                    "response.failed" => {
                        return Err(anyhow!(describe_failure(
                            event["response"]["error"]["code"].as_str(),
                            event["response"]["error"]["message"].as_str(),
                        )));
                    }
                    "response.incomplete" => {
                        return Err(anyhow!(
                            "the response ended early: {}",
                            event["response"]["incomplete_details"]["reason"]
                                .as_str()
                                .unwrap_or("unknown reason")
                        ));
                    }
                    "error" => {
                        return Err(anyhow!(describe_failure(
                            event["code"].as_str(),
                            event["message"].as_str()
                        )));
                    }
                    _ => {}
                }
            }
        }
        if !completed {
            bail!("the stream ended before response.completed");
        }
        Ok(())
    }
}

fn describe_failure(code: Option<&str>, message: Option<&str>) -> String {
    match code {
        Some("subscription_sharing_usage_limit_exceeded") => {
            "ChatGPT plan usage limit reached for this app. Check ChatGPT Settings → Usage, or switch to an API key.".to_string()
        }
        Some("subscription_sharing_usage_unavailable") => {
            "ChatGPT plan usage is not available for this account right now.".to_string()
        }
        _ => format!(
            "chatgpt failed{}{}",
            code.map(|c| format!(" ({c})")).unwrap_or_default(),
            message.map(|m| format!(": {m}")).unwrap_or_default()
        ),
    }
}

pub fn to_responses_input(messages: &[ChatMessage], namespace: &str) -> Vec<Value> {
    let mut out = Vec::new();
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
                if let Some(text) = content.as_deref().filter(|t| !t.is_empty()) {
                    out.push(json!({
                        "type": "message", "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in tool_calls.iter().flatten() {
                    out.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function.name,
                        "namespace": namespace,
                        "arguments": if call.function.arguments.is_empty() { "{}" } else { &call.function.arguments },
                    }));
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                out.push(json!({ "type": "function_call_output", "call_id": tool_call_id, "output": content }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::providers::{AssistantCall, AssistantFunction, ToolSpec};
    use crate::agent::util::testutil::{mock, Reply};
    use std::sync::Mutex;

    struct Fixed(Mutex<Vec<bool>>);

    impl TokenSource for Fixed {
        fn token(&self, force: bool) -> BoxFuture<'_, Result<String>> {
            self.0.lock().unwrap().push(force);
            Box::pin(async move { Ok(if force { "new" } else { "old" }.to_string()) })
        }
    }

    fn sse(events: &[Value]) -> Reply {
        Reply::sse(
            &events
                .iter()
                .map(|e| format!("data: {e}\n\n"))
                .collect::<String>(),
        )
    }

    async fn run(reply: Reply) -> Result<Vec<ModelEvent>> {
        let reply = Mutex::new(Some(reply));
        let net = mock(move |_| {
            reply
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Reply::status(500, "again"))
        })
        .await;
        let provider = ChatGptProvider::new(Arc::new(Fixed(Mutex::default())), Some(&net.base));
        let mut events = Vec::new();
        let messages = [ChatMessage::User {
            content: "x".into(),
        }];
        provider
            .stream(
                StreamRequest {
                    model: "m",
                    messages: &messages,
                    tools: &[],
                },
                &mut |e| events.push(e),
            )
            .await?;
        Ok(events)
    }

    #[tokio::test]
    async fn the_adapter_sends_the_shape_chatgpt_plan_usage_requires_and_reads_the_stream() {
        let net = mock(|_| {
            sse(&[
                json!({ "type": "response.output_text.delta", "delta": "Hel" }),
                json!({ "type": "response.output_text.delta", "delta": "lo" }),
                json!({ "type": "response.output_item.done", "item": { "type": "function_call", "call_id": "c1", "name": "kcore_read", "arguments": "{\"a\":1}" } }),
                json!({ "type": "response.completed", "response": {} }),
            ])
        })
        .await;
        let provider = ChatGptProvider::new(Arc::new(Fixed(Mutex::default())), Some(&net.base));
        let messages = vec![
            ChatMessage::System {
                content: "be brief".into(),
            },
            ChatMessage::User {
                content: "hi".into(),
            },
            ChatMessage::Assistant {
                content: None,
                tool_calls: Some(vec![AssistantCall {
                    id: "c0".into(),
                    kind: "function".into(),
                    function: AssistantFunction {
                        name: "kcore_read".into(),
                        arguments: "{}".into(),
                    },
                }]),
            },
            ChatMessage::Tool {
                tool_call_id: "c0".into(),
                content: "ok".into(),
            },
        ];
        let tools = [ToolSpec {
            name: "kcore_read".into(),
            description: "d".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }];
        let mut events = Vec::new();
        provider
            .stream(
                StreamRequest {
                    model: "m",
                    messages: &messages,
                    tools: &tools,
                },
                &mut |e| events.push(e),
            )
            .await
            .unwrap();

        let request = &net.requests()[0];
        assert_eq!(request.headers["authorization"], "Bearer old");
        let sent: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(sent["store"], false);
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["instructions"], "be brief");
        for banned in [
            "temperature",
            "max_output_tokens",
            "previous_response_id",
            "user",
            "metadata",
        ] {
            assert!(sent.get(banned).is_none(), "{banned}");
        }
        assert_eq!(sent["tools"][0]["type"], "namespace");
        assert_eq!(sent["tools"][0]["name"], "kcore");
        assert_eq!(sent["tools"][0]["tools"][0]["name"], "kcore_read");
        let input = sent["input"].as_array().unwrap();
        assert!(input.iter().all(|i| i["role"] != "system"));
        assert_eq!(
            input[1],
            json!({ "type": "function_call", "call_id": "c0", "name": "kcore_read", "namespace": "kcore", "arguments": "{}" })
        );
        assert_eq!(
            input[2],
            json!({ "type": "function_call_output", "call_id": "c0", "output": "ok" })
        );
        assert_eq!(
            events,
            vec![
                ModelEvent::Text("Hel".into()),
                ModelEvent::Text("lo".into()),
                ModelEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "kcore_read".into(),
                    arguments: "{\"a\":1}".into()
                }),
            ]
        );
    }

    #[tokio::test]
    async fn a_stream_with_no_completion_a_failure_or_a_usage_limit_is_an_error() {
        let missing = run(sse(&[
            json!({ "type": "response.output_text.delta", "delta": "x" }),
        ]))
        .await;
        assert!(missing
            .unwrap_err()
            .to_string()
            .contains("before response.completed"));
        let limit = run(sse(&[json!({ "type": "response.failed", "response": { "error": { "code": "subscription_sharing_usage_limit_exceeded" } } })])).await;
        assert!(limit.unwrap_err().to_string().contains("usage limit"));
        let early = run(sse(&[json!({ "type": "response.incomplete", "response": { "incomplete_details": { "reason": "max_tokens" } } })])).await;
        assert!(early.unwrap_err().to_string().contains("max_tokens"));
    }

    #[tokio::test]
    async fn a_401_refreshes_the_token_once_and_retries() {
        let net = mock(|seen| {
            if seen.headers["authorization"] == "Bearer new" {
                sse(&[json!({ "type": "response.completed", "response": {} })])
            } else {
                Reply::status(401, "no")
            }
        })
        .await;
        let tokens = Arc::new(Fixed(Mutex::default()));
        let provider = ChatGptProvider::new(tokens.clone(), Some(&net.base));
        let messages = [ChatMessage::User {
            content: "x".into(),
        }];
        provider
            .stream(
                StreamRequest {
                    model: "m",
                    messages: &messages,
                    tools: &[],
                },
                &mut |_| {},
            )
            .await
            .unwrap();
        assert_eq!(net.requests().len(), 2);
        assert_eq!(*tokens.0.lock().unwrap(), vec![false, true]);
    }

    #[tokio::test]
    async fn the_model_list_keeps_what_chatgpt_marks_as_listed() {
        let net = mock(|_| Reply::json(json!({ "models": [{ "slug": "a", "visibility": "list" }, { "slug": "hidden", "visibility": "hide" }, { "slug": "b", "visibility": "list" }] }))).await;
        let provider = ChatGptProvider::new(Arc::new(Fixed(Mutex::default())), Some(&net.base));
        assert_eq!(provider.models().await, vec!["a", "b"]);
    }

    #[test]
    fn system_messages_are_dropped_from_the_input() {
        let input = to_responses_input(
            &[
                ChatMessage::System {
                    content: "s".into(),
                },
                ChatMessage::User {
                    content: "u".into(),
                },
            ],
            "kcore",
        );
        assert_eq!(input, vec![json!({ "role": "user", "content": "u" })]);
    }
}
