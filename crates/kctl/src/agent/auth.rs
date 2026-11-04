//! API keys and the ChatGPT sign-in. Keys live in `auth.json` (mode 0600) or come from the environment.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};

use super::chatgpt::ChatGptAuth;
use super::providers::{Anthropic, ChatGptProvider, OpenAiCompatible, Provider};
use super::util::write_private;

pub const CHATGPT_ID: &str = "chatgpt";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    OpenAiCompatible,
    Anthropic,
}

struct ProviderDef {
    id: &'static str,
    kind: Kind,
    label: &'static str,
    note: Option<&'static str>,
    key_url: &'static str,
    base_url: &'static str,
    base_url_env: &'static str,
    models_url: Option<&'static str>,
    env_key: &'static str,
    fallback_models: &'static [&'static str],
    open_code_id: &'static str,
}

const PROVIDERS: [ProviderDef; 5] = [
    ProviderDef {
        id: "openai",
        kind: Kind::OpenAiCompatible,
        label: "OpenAI API key",
        note: None,
        key_url: "https://platform.openai.com/api-keys",
        base_url: "https://api.openai.com/v1",
        base_url_env: "OPENAI_BASE_URL",
        models_url: None,
        env_key: "OPENAI_API_KEY",
        fallback_models: &["gpt-4.1"],
        open_code_id: "openai",
    },
    ProviderDef {
        id: "anthropic",
        kind: Kind::Anthropic,
        label: "Claude (Anthropic API key)",
        note: Some("Needs a Claude Console API key. Anthropic does not allow Claude.ai or Claude subscription logins in other apps. To use Claude without your own Console account, connect OpenCode below and pick a claude model."),
        key_url: "https://platform.claude.com/settings/keys",
        base_url: "https://api.anthropic.com/v1",
        base_url_env: "ANTHROPIC_BASE_URL",
        models_url: None,
        env_key: "ANTHROPIC_API_KEY",
        fallback_models: &["claude-sonnet-4-5"],
        open_code_id: "anthropic",
    },
    ProviderDef {
        id: "deepseek",
        kind: Kind::OpenAiCompatible,
        label: "DeepSeek API key",
        note: Some("API key only. DeepSeek has no sign-in for other apps; the link opens its key page."),
        key_url: "https://platform.deepseek.com/api_keys",
        base_url: "https://api.deepseek.com",
        base_url_env: "DEEPSEEK_BASE_URL",
        models_url: None,
        env_key: "DEEPSEEK_API_KEY",
        fallback_models: &["deepseek-chat"],
        open_code_id: "deepseek",
    },
    ProviderDef {
        id: "opencode",
        kind: Kind::OpenAiCompatible,
        label: "OpenCode Console (pay as you go)",
        note: Some("One key for Claude, GPT, DeepSeek, Gemini, Kimi and others, billed by OpenCode. Models are listed by what your key can use."),
        key_url: "https://opencode.ai/auth",
        base_url: "https://opencode.ai/inference/openai/v1",
        base_url_env: "OPENCODE_BASE_URL",
        models_url: Some("https://opencode.ai/inference/v1/models"),
        env_key: "OPENCODE_API_KEY",
        fallback_models: &["claude-sonnet-5-5"],
        open_code_id: "opencode",
    },
    ProviderDef {
        id: "opencode-go",
        kind: Kind::OpenAiCompatible,
        label: "OpenCode Go (subscription)",
        note: Some("The OpenCode Go plan: open models such as DeepSeek, Kimi and GLM with the usage the plan includes. Use a key from a workspace that has Go."),
        key_url: "https://opencode.ai/auth",
        base_url: "https://opencode.ai/inference/go/openai/v1",
        base_url_env: "OPENCODE_GO_BASE_URL",
        models_url: Some("https://opencode.ai/inference/go/v1/models"),
        env_key: "OPENCODE_GO_API_KEY",
        fallback_models: &["deepseek-v4-pro"],
        open_code_id: "opencode-go",
    },
];

#[derive(Debug, Serialize, PartialEq)]
pub struct Importable {
    pub id: String,
    pub label: String,
    pub stored: bool,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Skipped {
    pub open_code_id: String,
    pub reason: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct OpenCodeScan {
    pub found: bool,
    pub importable: Vec<Importable>,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_url: Option<String>,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returning: Option<bool>,
    pub default_model: String,
}

pub struct AuthOptions {
    pub env: HashMap<String, String>,
    pub open_code_auth_path: Option<PathBuf>,
    pub chatgpt_issuer: Option<String>,
    pub chatgpt_base_url: Option<String>,
}

impl AuthOptions {
    pub fn from_process() -> Self {
        Self {
            env: std::env::vars().collect(),
            open_code_auth_path: None,
            chatgpt_issuer: None,
            chatgpt_base_url: None,
        }
    }
}

pub struct AuthStore {
    path: PathBuf,
    env: HashMap<String, String>,
    open_code_path: PathBuf,
    chatgpt_base_url: Option<String>,
    pub chatgpt: Arc<ChatGptAuth>,
    write_lock: Mutex<()>,
}

impl AuthStore {
    pub fn new(home: PathBuf, options: AuthOptions) -> Self {
        let data_home = options
            .env
            .get("XDG_DATA_HOME")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
            .unwrap_or_default();
        let chatgpt = match &options.chatgpt_issuer {
            Some(issuer) => ChatGptAuth::with(
                home.clone(),
                issuer,
                Arc::new(super::util::now_ms),
                std::time::Duration::from_millis(250),
            ),
            None => ChatGptAuth::new(home.clone()),
        };
        Self {
            path: home.join("auth.json"),
            open_code_path: options
                .open_code_auth_path
                .unwrap_or_else(|| data_home.join("opencode").join("auth.json")),
            env: options.env,
            chatgpt_base_url: options.chatgpt_base_url,
            chatgpt: Arc::new(chatgpt),
            write_lock: Mutex::new(()),
        }
    }

    fn read(&self) -> Map<String, Value> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    fn write(&self, file: &Map<String, Value>) -> Result<()> {
        write_private(&self.path, serde_json::to_string_pretty(file)?.as_bytes())
    }

    fn stored_key(file: &Map<String, Value>, id: &str) -> Option<String> {
        file.get(id)?["key"]
            .as_str()
            .filter(|k| !k.is_empty())
            .map(str::to_string)
    }

    pub fn set_key(&self, id: &str, key: &str) -> Result<()> {
        if !PROVIDERS.iter().any(|p| p.id == id) {
            bail!("unknown provider {id}");
        }
        let key = key.trim();
        if key.is_empty() {
            bail!("key is empty");
        }
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.read();
        file.insert(id.to_string(), json!({ "type": "api", "key": key }));
        self.write(&file)
    }

    pub fn remove_key(&self, id: &str) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.read();
        file.remove(id);
        self.write(&file)
    }

    fn read_open_code(&self) -> Option<Map<String, Value>> {
        let text = std::fs::read_to_string(&self.open_code_path).ok()?;
        serde_json::from_str::<Value>(&text)
            .ok()?
            .as_object()
            .cloned()
    }

    /// Looks at the operator's OpenCode login file without returning any secret.
    pub fn scan_open_code(&self) -> OpenCodeScan {
        let Some(entries) = self.read_open_code() else {
            return OpenCodeScan {
                found: false,
                importable: vec![],
                skipped: vec![],
            };
        };
        let stored = self.read();
        let mut importable = Vec::new();
        let mut skipped = Vec::new();
        for (open_code_id, entry) in &entries {
            let Some(def) = PROVIDERS.iter().find(|p| p.open_code_id == open_code_id) else {
                continue;
            };
            if entry["type"] == "api" && entry["key"].as_str().is_some_and(|k| !k.trim().is_empty())
            {
                importable.push(Importable {
                    id: def.id.to_string(),
                    label: def.label.to_string(),
                    stored: Self::stored_key(&stored, def.id).is_some(),
                });
            } else {
                skipped.push(Skipped {
                    open_code_id: open_code_id.clone(),
                    reason: "Signed in with an account, not a key. That login belongs to OpenCode."
                        .to_string(),
                });
            }
        }
        OpenCodeScan {
            found: true,
            importable,
            skipped,
        }
    }

    /// Copies OpenCode API keys we do not already hold. Account logins are never copied.
    pub fn import_open_code(&self) -> Vec<String> {
        let Some(entries) = self.read_open_code() else {
            return Vec::new();
        };
        let mut imported = Vec::new();
        for def in &PROVIDERS {
            let Some(entry) = entries.get(def.open_code_id) else {
                continue;
            };
            let Some(key) = entry["key"]
                .as_str()
                .filter(|k| entry["type"] == "api" && !k.trim().is_empty())
            else {
                continue;
            };
            if Self::stored_key(&self.read(), def.id).is_some() {
                continue;
            }
            if self.set_key(def.id, key).is_ok() {
                imported.push(def.id.to_string());
            }
        }
        imported
    }

    fn env(&self, name: &str) -> Option<&str> {
        self.env
            .get(name)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }

    fn key_for(&self, def: &ProviderDef) -> Option<(String, &'static str)> {
        if let Some(key) = Self::stored_key(&self.read(), def.id) {
            return Some((key, "file"));
        }
        self.env(def.env_key).map(|k| (k.to_string(), "env"))
    }

    /// What the browser may know: never a key or a token.
    pub fn describe(&self) -> Vec<ProviderInfo> {
        let status = self.chatgpt.status();
        let mut out = vec![ProviderInfo {
            id: CHATGPT_ID.to_string(),
            label: "ChatGPT plan".to_string(),
            kind: "oauth",
            note: Some("Uses your ChatGPT plan through Sign in with ChatGPT. Not available on every plan; use an OpenAI key if sign-in is refused.".to_string()),
            key_url: None,
            connected: status.signed_in,
            source: None,
            account: status.email,
            pending: Some(status.pending),
            error: status.error,
            returning: Some(status.returning),
            default_model: "gpt-5".to_string(),
        }];
        for def in &PROVIDERS {
            let found = self.key_for(def);
            out.push(ProviderInfo {
                id: def.id.to_string(),
                label: def.label.to_string(),
                kind: "api",
                note: def.note.map(str::to_string),
                key_url: Some(def.key_url.to_string()),
                connected: found.is_some(),
                source: found.map(|(_, source)| source),
                account: None,
                pending: None,
                error: None,
                returning: None,
                default_model: def
                    .fallback_models
                    .first()
                    .copied()
                    .unwrap_or("")
                    .to_string(),
            });
        }
        out
    }

    pub fn provider(&self, id: &str) -> Option<Provider> {
        if id == CHATGPT_ID {
            if !self.chatgpt.status().signed_in {
                return None;
            }
            return Some(Provider::ChatGpt(Arc::new(ChatGptProvider::new(
                self.chatgpt.clone(),
                self.chatgpt_base_url.as_deref(),
            ))));
        }
        let def = PROVIDERS.iter().find(|p| p.id == id)?;
        let (key, _) = self.key_for(def)?;
        let redirected = self.env(def.base_url_env);
        let base_url = redirected.unwrap_or(def.base_url);
        Some(match def.kind {
            Kind::Anthropic => Provider::Anthropic(Arc::new(Anthropic::new(&key, Some(base_url)))),
            Kind::OpenAiCompatible => Provider::OpenAi(Arc::new(OpenAiCompatible::new(
                def.id,
                base_url,
                &key,
                def.fallback_models,
                // A redirected base URL (a local gateway, a test server) keeps the standard /models path.
                if redirected.is_some() {
                    None
                } else {
                    def.models_url
                },
            ))),
        })
    }

    pub fn require_provider(&self, id: &str) -> Result<Provider> {
        self.provider(id)
            .ok_or_else(|| anyhow!("{id} is not connected. Add a key or sign in first."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::util::testutil::{mock, Reply};

    fn store(env: &[(&str, &str)], open_code: Option<PathBuf>) -> (AuthStore, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let options = AuthOptions {
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            open_code_auth_path: open_code,
            chatgpt_issuer: None,
            chatgpt_base_url: None,
        };
        (AuthStore::new(home.path().to_path_buf(), options), home)
    }

    fn open_code_file(entries: Value) -> (PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, entries.to_string()).unwrap();
        (path, dir)
    }

    #[test]
    fn a_stored_key_is_private_and_never_described() {
        use std::os::unix::fs::PermissionsExt;
        let (auth, home) = store(&[], None);
        assert!(auth.provider("deepseek").is_none());
        auth.set_key("deepseek", " sk-secret ").unwrap();
        let mode = std::fs::metadata(home.path().join("auth.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let info = auth.describe();
        let deepseek = info.iter().find(|p| p.id == "deepseek").unwrap();
        assert!(deepseek.connected && deepseek.source == Some("file"));
        assert!(!serde_json::to_string(&info).unwrap().contains("sk-secret"));
        assert!(auth.set_key("nope", "k").is_err());
        assert!(auth.set_key("openai", "  ").is_err());
        auth.remove_key("deepseek").unwrap();
        assert!(auth.provider("deepseek").is_none());
    }

    #[test]
    fn a_key_in_the_environment_connects_the_provider() {
        let (auth, _home) = store(&[("ANTHROPIC_API_KEY", "k")], None);
        let info = auth.describe();
        let anthropic = info.iter().find(|p| p.id == "anthropic").unwrap();
        assert!(anthropic.connected && anthropic.source == Some("env"));
        assert_eq!(
            anthropic.key_url.as_deref(),
            Some("https://platform.claude.com/settings/keys")
        );
        assert_eq!(
            info.iter()
                .find(|p| p.id == "opencode")
                .unwrap()
                .default_model,
            "claude-sonnet-5-5"
        );
        assert!(auth.provider("anthropic").is_some());
    }

    #[tokio::test]
    async fn open_code_providers_use_the_console_gateway_and_a_redirect_moves_the_model_list_too() {
        let net = mock(|_| Reply::json(json!({ "data": [{ "id": "b" }, { "id": "a" }] }))).await;
        let (auth, _home) = store(
            &[
                ("OPENCODE_API_KEY", "oc-key"),
                ("OPENCODE_BASE_URL", &net.base),
            ],
            None,
        );
        assert_eq!(
            auth.provider("opencode").unwrap().models().await,
            vec!["a", "b"]
        );
        let seen = net.requests();
        assert_eq!(seen[0].path, "/models");
        assert_eq!(seen[0].headers["authorization"], "Bearer oc-key");
    }

    #[test]
    fn open_code_import_copies_api_keys_skips_logins_and_keys_we_hold() {
        let (path, _dir) = open_code_file(json!({
            "anthropic": { "type": "api", "key": "sk-ant-from-opencode" },
            "deepseek": { "type": "api", "key": "sk-deep" },
            "openai": { "type": "oauth", "refresh": "r", "access": "a", "expires": 1 },
            "opencode": { "type": "api", "key": "oc-1" },
            "unknown": { "type": "api", "key": "zzz" },
        }));
        let (auth, home) = store(&[], Some(path));
        auth.set_key("deepseek", "mine").unwrap();
        let scan = auth.scan_open_code();
        assert!(scan.found);
        let ids: Vec<(&str, bool)> = scan
            .importable
            .iter()
            .map(|i| (i.id.as_str(), i.stored))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("anthropic", false),
                ("deepseek", true),
                ("opencode", false)
            ]
        );
        assert_eq!(
            scan.skipped
                .iter()
                .map(|s| s.open_code_id.as_str())
                .collect::<Vec<_>>(),
            vec!["openai"]
        );
        assert!(!serde_json::to_string(&scan)
            .unwrap()
            .contains("sk-ant-from-opencode"));

        assert_eq!(auth.import_open_code(), vec!["anthropic", "opencode"]);
        let stored: Value =
            serde_json::from_str(&std::fs::read_to_string(home.path().join("auth.json")).unwrap())
                .unwrap();
        assert_eq!(stored["anthropic"]["key"], "sk-ant-from-opencode");
        assert_eq!(stored["deepseek"]["key"], "mine");
        assert!(stored.get("openai").is_none());
        assert!(auth.import_open_code().is_empty());
    }

    #[test]
    fn a_missing_or_broken_open_code_file_imports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (missing, _h) = store(&[], Some(dir.path().join("nope.json")));
        assert_eq!(
            missing.scan_open_code(),
            OpenCodeScan {
                found: false,
                importable: vec![],
                skipped: vec![]
            }
        );
        assert!(missing.import_open_code().is_empty());
        let broken = dir.path().join("auth.json");
        std::fs::write(&broken, "{not json").unwrap();
        let (auth, _h2) = store(&[], Some(broken));
        assert!(!auth.scan_open_code().found);
    }
}
