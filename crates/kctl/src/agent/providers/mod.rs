//! Model providers. Each one turns the chat history and tool list into a stream of text and tool calls.

pub mod anthropic;
pub mod openai;
pub mod responses;

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use anthropic::Anthropic;
pub use openai::OpenAiCompatible;
pub use responses::ChatGptProvider;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: AssistantFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantFunction {
    pub name: String,
    pub arguments: String,
}

/// Chat Completions message shape. The transcript stores this verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<AssistantCall>>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    Text(String),
    ToolCall(ToolCall),
}

pub struct StreamRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    pub tools: &'a [ToolSpec],
}

pub type Emit<'a> = &'a mut (dyn FnMut(ModelEvent) + Send);

#[derive(Clone)]
pub enum Provider {
    OpenAi(Arc<OpenAiCompatible>),
    Anthropic(Arc<Anthropic>),
    ChatGpt(Arc<ChatGptProvider>),
    #[cfg(test)]
    Scripted(Arc<Scripted>),
}

#[cfg(test)]
pub type Script = Box<dyn Fn(usize) -> Vec<ModelEvent> + Send + Sync>;

/// A provider that plays back canned turns and records what it was sent.
#[cfg(test)]
pub struct Scripted {
    script: Script,
    pub requests: std::sync::Mutex<Vec<(Vec<ChatMessage>, Vec<String>)>>,
}

#[cfg(test)]
impl Scripted {
    pub fn new(script: impl Fn(usize) -> Vec<ModelEvent> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            requests: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn call(id: &str, name: &str, arguments: &Value) -> ModelEvent {
        ModelEvent::ToolCall(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
        })
    }
}

impl Provider {
    pub fn id(&self) -> &str {
        match self {
            Self::OpenAi(provider) => &provider.id,
            Self::Anthropic(_) => "anthropic",
            Self::ChatGpt(_) => "chatgpt",
            #[cfg(test)]
            Self::Scripted(_) => "scripted",
        }
    }

    pub async fn models(&self) -> Vec<String> {
        match self {
            Self::OpenAi(provider) => provider.models().await,
            Self::Anthropic(provider) => provider.models().await,
            Self::ChatGpt(provider) => provider.models().await,
            #[cfg(test)]
            Self::Scripted(_) => vec!["scripted-1".to_string()],
        }
    }

    pub async fn stream(&self, request: StreamRequest<'_>, emit: Emit<'_>) -> Result<()> {
        match self {
            Self::OpenAi(provider) => provider.stream(request, emit).await,
            Self::Anthropic(provider) => provider.stream(request, emit).await,
            Self::ChatGpt(provider) => provider.stream(request, emit).await,
            #[cfg(test)]
            Self::Scripted(provider) => {
                let turn = {
                    let mut seen = provider.requests.lock().unwrap();
                    seen.push((
                        request.messages.to_vec(),
                        request.tools.iter().map(|t| t.name.clone()).collect(),
                    ));
                    seen.len() - 1
                };
                for event in (provider.script)(turn) {
                    emit(event);
                }
                Ok(())
            }
        }
    }
}

/// Splits a byte stream into pieces ended by `separator`, keeping a partial tail.
/// Works on bytes so a multi-byte character cut by a chunk boundary is not damaged.
pub struct Splitter {
    buffer: Vec<u8>,
    separator: &'static [u8],
}

impl Splitter {
    pub fn new(separator: &'static [u8]) -> Self {
        Self {
            buffer: Vec::new(),
            separator,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(at) = find(&self.buffer, self.separator) {
            let rest = self.buffer.split_off(at + self.separator.len());
            let mut piece = std::mem::replace(&mut self.buffer, rest);
            piece.truncate(at);
            out.push(String::from_utf8_lossy(&piece).into_owned());
        }
        out
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("kctl-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

pub async fn error_detail(response: reqwest::Response) -> String {
    let text = response.text().await.unwrap_or_default();
    text.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_splitter_keeps_partial_lines_and_split_characters() {
        let mut lines = Splitter::new(b"\n");
        assert_eq!(lines.push(b"data: one\ndata: tw"), vec!["data: one"]);
        let euro = "€".as_bytes();
        assert!(lines.push(&[b'o', euro[0]]).is_empty());
        let mut rest = euro[1..].to_vec();
        rest.push(b'\n');
        assert_eq!(lines.push(&rest), vec!["data: two€"]);
    }

    #[test]
    fn messages_serialize_in_the_chat_completions_shape() {
        let message = ChatMessage::Assistant {
            content: None,
            tool_calls: Some(vec![AssistantCall {
                id: "c1".into(),
                kind: "function".into(),
                function: AssistantFunction {
                    name: "kcore_read".into(),
                    arguments: "{}".into(),
                },
            }]),
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"], Value::Null);
        assert_eq!(value["tool_calls"][0]["type"], "function");
        let back: ChatMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back, message);
    }
}
