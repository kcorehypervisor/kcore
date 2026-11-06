//! Sign in with ChatGPT, open-source flow: https://developers.openai.com/siwc/token-sharing-open-source

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::util::{base64url, now_ms, random_bytes, sha256_base64url, write_private};

const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "kcore";
const RESOURCE: &str = "https://api.openai.com/v1";
const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const REFRESH_MARGIN_MS: i64 = 3 * 60_000;
const ATTEMPT_TTL_MS: i64 = 10 * 60_000;
const DEAD_REFRESH: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

pub const CALLBACK_PATH: &str = "/auth/callback";
pub const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

#[derive(Debug)]
pub struct NotSignedIn(pub String);

impl Default for NotSignedIn {
    fn default() -> Self {
        Self("ChatGPT is not signed in".to_string())
    }
}

impl fmt::Display for NotSignedIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotSignedIn {}

#[derive(Debug)]
struct TokenError {
    code: String,
    description: Option<String>,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.description {
            Some(description) => write!(f, "{}: {description}", self.code),
            None => f.write_str(&self.code),
        }
    }
}

impl std::error::Error for TokenError {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub ext_agent_host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub saved_at: String,
}

impl Record {
    fn signed_out(mut self) -> Self {
        self.access_token = None;
        self.refresh_token = None;
        self.id_token = None;
        self.expires_at = None;
        self
    }

    fn can_infer(&self) -> bool {
        self.access_token.is_some() && self.scopes.iter().any(|s| s == DIRECT_SCOPE)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub signed_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A previous account is remembered, so the next sign-in reuses its registration.
    pub returning: bool,
}

#[derive(Debug)]
struct Attempt {
    verifier: String,
    nonce: String,
    redirect_uri: String,
    client_id: Option<String>,
    created_at: i64,
}

#[derive(Debug, Default, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<f64>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Discovery {
    jwks_uri: Option<String>,
    revocation_endpoint: Option<String>,
}

#[derive(Default)]
struct Inner {
    attempts: HashMap<String, Attempt>,
    last_error: Option<String>,
    discovery: Option<Discovery>,
}

pub struct CallbackOutcome {
    pub ok: bool,
    pub message: String,
    /// A fresh authorization the browser should be sent to straight away.
    pub retry_url: Option<String>,
}

pub struct ChatGptAuth {
    home: PathBuf,
    issuer: String,
    client: reqwest::Client,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    revoke_pause: Duration,
    inner: Mutex<Inner>,
    refresh_lock: tokio::sync::Mutex<()>,
}

impl ChatGptAuth {
    pub fn new(home: PathBuf) -> Self {
        Self::with(
            home,
            "https://auth.openai.com",
            Arc::new(now_ms),
            Duration::from_millis(250),
        )
    }

    pub fn with(
        home: PathBuf,
        issuer: &str,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
        revoke_pause: Duration,
    ) -> Self {
        Self {
            home,
            issuer: issuer.trim_end_matches('/').to_string(),
            client: super::providers::http(),
            now,
            revoke_pause,
            inner: Mutex::new(Inner::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn now(&self) -> i64 {
        (self.now)()
    }

    fn record_path(&self) -> PathBuf {
        self.home.join("chatgpt.json")
    }

    fn host_path(&self) -> PathBuf {
        self.home.join("host.json")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One opaque id per machine, chosen before the first sign-in and never changed.
    pub fn host_id(&self) -> Result<String> {
        if let Ok(text) = std::fs::read_to_string(self.host_path()) {
            if let Some(found) = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["ext_agent_host_id"].as_str().map(str::to_string))
                .filter(|id| !id.is_empty())
            {
                return Ok(found);
            }
        }
        let id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        write_private(
            &self.host_path(),
            serde_json::to_string_pretty(&serde_json::json!({ "ext_agent_host_id": id }))?
                .as_bytes(),
        )?;
        Ok(id)
    }

    fn load(&self) -> Option<Record> {
        serde_json::from_str(&std::fs::read_to_string(self.record_path()).ok()?).ok()
    }

    fn save(&self, record: &Record) -> Result<()> {
        write_private(
            &self.record_path(),
            serde_json::to_string_pretty(record)?.as_bytes(),
        )
    }

    fn expire_attempts(&self, inner: &mut Inner) {
        let now = self.now();
        inner
            .attempts
            .retain(|_, a| now - a.created_at <= ATTEMPT_TTL_MS);
    }

    pub fn status(&self) -> Status {
        let mut inner = self.lock();
        self.expire_attempts(&mut inner);
        let record = self.load();
        Status {
            signed_in: record.as_ref().is_some_and(Record::can_infer),
            email: record
                .as_ref()
                .map(|r| r.email.clone())
                .filter(|e| !e.is_empty()),
            pending: !inner.attempts.is_empty(),
            error: inner.last_error.clone(),
            returning: record.as_ref().is_some_and(|r| !r.client_id.is_empty()),
        }
    }

    /// The page to open in the browser. `port` is the port this agent listens on; only the port may
    /// change between sign-ins, the host and path are fixed by OpenAI's registration.
    pub fn start(&self, port: u16, new_account: bool) -> Result<String> {
        let saved = self.load();
        let reuse = if new_account {
            None
        } else {
            saved.filter(|r| !r.client_id.is_empty())
        };
        let state = base64url(&random_bytes(24)?);
        let nonce = base64url(&random_bytes(24)?);
        let verifier = base64url(&random_bytes(48)?);
        let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
        let host_id = self.host_id()?;
        {
            let mut inner = self.lock();
            self.expire_attempts(&mut inner);
            inner.last_error = None;
            inner.attempts.insert(
                state.clone(),
                Attempt {
                    verifier: verifier.clone(),
                    nonce: nonce.clone(),
                    redirect_uri: redirect_uri.clone(),
                    client_id: reuse.as_ref().map(|r| r.client_id.clone()),
                    created_at: self.now(),
                },
            );
        }

        let mut url = url::Url::parse(&format!("{}/api/accounts/authorize", self.issuer))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair(
                "client_id",
                reuse.as_ref().map_or(DYNAMIC_CLIENT, |r| &r.client_id),
            );
            if reuse.is_none() {
                query.append_pair("agent_name_hint", AGENT_NAME);
            }
            query.append_pair("ext_agent_host_id", &host_id);
            if let Some(token) = reuse.as_ref().and_then(|r| r.id_token.as_deref()) {
                query.append_pair("id_token_hint", token);
            }
            if let Some(email) = reuse
                .as_ref()
                .map(|r| r.email.as_str())
                .filter(|e| !e.is_empty())
            {
                query.append_pair("login_hint", email);
            }
            query.append_pair("response_type", "code");
            query.append_pair("redirect_uri", &redirect_uri);
            query.append_pair("scope", SCOPES);
            query.append_pair("resource", RESOURCE);
            query.append_pair("state", &state);
            query.append_pair("nonce", &nonce);
            query.append_pair("code_challenge", &sha256_base64url(&verifier));
            query.append_pair("code_challenge_method", "S256");
        }
        Ok(url.to_string())
    }

    fn retry_with_registration(&self, client_id: &str, redirect_uri: &str) -> Option<String> {
        let port = url::Url::parse(redirect_uri).ok()?.port()?;
        if self.load().is_none_or(|r| r.client_id.is_empty()) {
            let record = Record {
                issuer: self.issuer.clone(),
                client_id: client_id.to_string(),
                ext_agent_host_id: self.host_id().ok()?,
                token_type: "Bearer".to_string(),
                ..Record::default()
            };
            self.save(&record).ok()?;
        }
        self.start(port, false).ok()
    }

    fn fail(&self, message: impl Into<String>) -> CallbackOutcome {
        let message = message.into();
        self.lock().last_error = Some(message.clone());
        CallbackOutcome {
            ok: false,
            message,
            retry_url: None,
        }
    }

    /// The browser landed on our loopback callback. The message is shown to the operator, so it
    /// carries no secrets.
    pub async fn handle_callback(&self, params: &HashMap<String, String>) -> CallbackOutcome {
        let state = params.get("state").cloned().unwrap_or_default();
        let attempt = {
            let mut inner = self.lock();
            self.expire_attempts(&mut inner);
            inner.attempts.remove(&state)
        };
        let Some(attempt) = attempt else {
            return self.fail("This sign-in attempt is unknown or has expired. Start it again from the agent page.");
        };

        if let Some(oauth_error) = params.get("error") {
            return self.fail(if oauth_error == "access_denied" {
                "Sign-in was declined in the browser, so nothing was connected.".to_string()
            } else {
                match params.get("error_description") {
                    Some(description) => {
                        format!("ChatGPT sign-in failed: {oauth_error} ({description})")
                    }
                    None => format!("ChatGPT sign-in failed: {oauth_error}"),
                }
            });
        }
        let Some(code) = params.get("code").filter(|c| !c.is_empty()) else {
            return self.fail("ChatGPT did not return an authorization code.");
        };

        let callback_client = params.get("client_id").filter(|c| !c.is_empty());
        let client_id = match &attempt.client_id {
            Some(known) => {
                if callback_client.is_some_and(|c| c != known) {
                    return self.fail("ChatGPT answered for a different registration, so the result was rejected.");
                }
                known.clone()
            }
            None => match callback_client {
                Some(issued) if issued != DYNAMIC_CLIENT => issued.clone(),
                _ => {
                    return self
                        .fail("ChatGPT did not issue a client id, so registration is incomplete.")
                }
            },
        };

        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", code.as_str()),
            ("code_verifier", attempt.verifier.as_str()),
            ("redirect_uri", attempt.redirect_uri.as_str()),
            ("resource", RESOURCE),
        ];
        let tokens = match self.post_token(&form).await {
            Ok(tokens) => tokens,
            Err(error) => {
                let rejected = error
                    .downcast_ref::<TokenError>()
                    .is_some_and(|e| e.code == "invalid_grant");
                if rejected && attempt.client_id.is_none() {
                    // OpenAI's documented recovery for a first registration whose code is refused:
                    // keep the issued client id and authorize again with it.
                    if let Some(url) =
                        self.retry_with_registration(&client_id, &attempt.redirect_uri)
                    {
                        let mut outcome = self.fail(
                            "ChatGPT did not accept the first sign-in code, so signing in again with the registration it just issued.",
                        );
                        outcome.retry_url = Some(url);
                        return outcome;
                    }
                }
                if rejected {
                    return self.fail(format!(
                        "ChatGPT rejected the sign-in code (invalid_grant for client {client_id}, redirect {}). Start the sign-in again.",
                        attempt.redirect_uri
                    ));
                }
                return self.fail(format!("Could not exchange the sign-in code: {error}"));
            }
        };
        let (Some(access_token), Some(id_token)) =
            (tokens.access_token.clone(), tokens.id_token.clone())
        else {
            return self.fail("ChatGPT returned an incomplete token response.");
        };

        let claims = match self
            .verify_id_token(&id_token, &client_id, &attempt.nonce)
            .await
        {
            Ok(claims) => claims,
            Err(error) => {
                return self.fail(format!(
                    "ChatGPT returned an ID token that did not check out: {error}"
                ));
            }
        };

        let scope_text = tokens
            .scope
            .clone()
            .or_else(|| params.get("scope").cloned())
            .unwrap_or_default();
        let scopes: Vec<String> = scope_text
            .split(|c: char| c.is_whitespace() || c == '+')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !scopes.iter().any(|s| s == DIRECT_SCOPE) {
            return self.fail(format!(
                "ChatGPT plan use was not granted to this app. Allow it under ChatGPT Settings, then sign in again. Usage is shown at {CHATGPT_USAGE_URL}."
            ));
        }

        let subject = claims["sub"].as_str().unwrap_or("").to_string();
        let previous = self.load();
        if attempt.client_id.is_some()
            && previous
                .as_ref()
                .is_some_and(|p| !p.subject.is_empty() && p.subject != subject)
        {
            return self.fail("The ChatGPT account that signed in is not the account this registration belongs to.");
        }
        let replaced = previous
            .as_ref()
            .filter(|p| p.subject != subject && p.refresh_token.is_some())
            .cloned();

        let expires_in = tokens.expires_in.unwrap_or(3600.0) as i64;
        let host_id = match self.host_id() {
            Ok(id) => id,
            Err(error) => return self.fail(format!("Could not save the sign-in: {error}")),
        };
        let record = Record {
            email: claims["email"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| previous.map(|p| p.email).unwrap_or_default()),
            issuer: self.issuer.clone(),
            subject,
            client_id,
            ext_agent_host_id: host_id,
            id_token: Some(id_token),
            access_token: Some(access_token),
            refresh_token: tokens.refresh_token,
            token_type: tokens.token_type.unwrap_or_else(|| "Bearer".to_string()),
            expires_in,
            expires_at: Some(self.now() + expires_in * 1000),
            scopes,
            saved_at: iso(self.now()),
        };
        if let Err(error) = self.save(&record) {
            return self.fail(format!("Could not save the sign-in: {error}"));
        }
        self.lock().last_error = None;
        if let Some(old) = replaced {
            let _ = self.revoke(&old).await;
        }
        CallbackOutcome {
            ok: true,
            message: "Signed in with ChatGPT. You can close this tab and go back to the agent."
                .to_string(),
            retry_url: None,
        }
    }

    /// A token good for at least three more minutes. Refreshes are serialized because the refresh
    /// token rotates.
    pub async fn access_token(&self, force: bool) -> Result<String> {
        let record = self
            .load()
            .filter(Record::can_infer)
            .ok_or_else(NotSignedIn::default)?;
        if !force && record.expires_at.unwrap_or(0) - self.now() > REFRESH_MARGIN_MS {
            return Ok(record.access_token.unwrap_or_default());
        }
        let _guard = self.refresh_lock.lock().await;
        let record = self
            .load()
            .filter(Record::can_infer)
            .ok_or_else(NotSignedIn::default)?;
        if !force && record.expires_at.unwrap_or(0) - self.now() > REFRESH_MARGIN_MS {
            return Ok(record.access_token.unwrap_or_default());
        }
        let updated = self.refresh().await?;
        updated
            .access_token
            .ok_or_else(|| NotSignedIn::default().into())
    }

    async fn refresh(&self) -> Result<Record> {
        let record = self.load().ok_or_else(NotSignedIn::default)?;
        let Some(refresh_token) = record.refresh_token.clone() else {
            return Err(NotSignedIn(
                "ChatGPT is not signed in, or has no refresh token. Sign in again.".to_string(),
            )
            .into());
        };
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", record.client_id.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("resource", RESOURCE),
        ];
        let tokens = match self.post_token(&form).await {
            Ok(tokens) => tokens,
            Err(error) => {
                if error
                    .downcast_ref::<TokenError>()
                    .is_some_and(|e| DEAD_REFRESH.contains(&e.code.as_str()))
                {
                    self.save(&record.clone().signed_out())?;
                    let message =
                        "ChatGPT no longer accepts this sign-in. Sign in again.".to_string();
                    self.lock().last_error = Some(message.clone());
                    return Err(NotSignedIn(message).into());
                }
                return Err(error);
            }
        };
        let Some(access_token) = tokens.access_token else {
            bail!("ChatGPT returned no access token on refresh");
        };
        let scopes = match tokens.scope.as_deref() {
            Some(scope) => scope.split_whitespace().map(str::to_string).collect(),
            None => record.scopes.clone(),
        };
        let expires_in = tokens
            .expires_in
            .map(|n| n as i64)
            .unwrap_or(record.expires_in);
        let next = Record {
            access_token: Some(access_token),
            refresh_token: tokens.refresh_token.or(record.refresh_token.clone()),
            id_token: tokens.id_token.or(record.id_token.clone()),
            expires_in,
            expires_at: Some(self.now() + expires_in * 1000),
            scopes,
            saved_at: iso(self.now()),
            ..record
        };
        self.save(&next)?;
        Ok(next)
    }

    /// Stops using the account, revokes the refresh token, and clears the tokens. The registration
    /// and the host id stay so the next sign-in reuses them. Returns whether OpenAI confirmed the
    /// revocation.
    pub async fn sign_out(&self) -> bool {
        let record = self.load();
        {
            let mut inner = self.lock();
            inner.attempts.clear();
            inner.last_error = None;
        }
        let Some(record) = record else {
            return true;
        };
        let mut revoked = true;
        if record.refresh_token.is_some() && self.revoke(&record).await.is_err() {
            revoked = false;
        }
        let _ = self.save(&record.signed_out());
        revoked
    }

    async fn revoke(&self, record: &Record) -> Result<()> {
        let endpoint = self
            .discover()
            .await?
            .revocation_endpoint
            .ok_or_else(|| anyhow!("OpenAI publishes no revocation endpoint"))?;
        let mut last = anyhow!("revocation failed");
        for attempt in 0..3u32 {
            let sent = self
                .client
                .post(&endpoint)
                .header("accept", "application/json")
                .form(&[
                    ("token", record.refresh_token.as_deref().unwrap_or("")),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", record.client_id.as_str()),
                ])
                .timeout(Duration::from_secs(15))
                .send()
                .await;
            match sent {
                Ok(response) if response.status().as_u16() == 200 => return Ok(()),
                Ok(response) if response.status().as_u16() < 500 => {
                    bail!("revocation answered {}", response.status().as_u16());
                }
                Ok(response) => {
                    last = anyhow!("revocation answered {}", response.status().as_u16())
                }
                Err(error) => last = error.into(),
            }
            tokio::time::sleep(self.revoke_pause * 2u32.pow(attempt)).await;
        }
        Err(last)
    }

    async fn discover(&self) -> Result<Discovery> {
        if let Some(found) = self.lock().discovery.clone() {
            return Ok(found);
        }
        let response = self
            .client
            .get(format!("{}/.well-known/openid-configuration", self.issuer))
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("discovery answered {}", response.status().as_u16());
        }
        let found: Discovery = response.json().await?;
        self.lock().discovery = Some(found.clone());
        Ok(found)
    }

    async fn post_token(&self, form: &[(&str, &str)]) -> Result<TokenResponse> {
        let response = self
            .client
            .post(format!("{}/api/accounts/oauth/token", self.issuer))
            .header("accept", "application/json")
            .form(form)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        let parsed: TokenResponse = serde_json::from_str(&text).unwrap_or_default();
        if !status.is_success() {
            return Err(TokenError {
                code: parsed
                    .error
                    .unwrap_or_else(|| format!("http_{}", status.as_u16())),
                description: parsed.error_description,
            }
            .into());
        }
        Ok(parsed)
    }

    async fn verify_id_token(&self, token: &str, client_id: &str, nonce: &str) -> Result<Value> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| anyhow!("not a JWT"))?;
        let algorithm = match header.alg {
            Algorithm::RS256 | Algorithm::ES256 => header.alg,
            other => bail!("unsupported signing algorithm {other:?}"),
        };
        let jwks_uri = self
            .discover()
            .await?
            .jwks_uri
            .ok_or_else(|| anyhow!("OpenAI publishes no JWKS"))?;
        let response = self
            .client
            .get(&jwks_uri)
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("JWKS answered {}", response.status().as_u16());
        }
        let keys: Vec<Value> = response.json::<Value>().await?["keys"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let jwk = keys
            .iter()
            .find(|k| header.kid.is_some() && k["kid"].as_str() == header.kid.as_deref())
            .or_else(|| (keys.len() == 1 && header.kid.is_none()).then(|| &keys[0]))
            .ok_or_else(|| anyhow!("signing key not found"))?;
        let part = |name: &str| {
            jwk[name]
                .as_str()
                .ok_or_else(|| anyhow!("signing key is incomplete"))
        };
        let key = match algorithm {
            Algorithm::RS256 => DecodingKey::from_rsa_components(part("n")?, part("e")?),
            _ => DecodingKey::from_ec_components(part("x")?, part("y")?),
        }
        .map_err(|e| anyhow!("signing key is unusable: {e}"))?;

        let mut validation = Validation::new(algorithm);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = false;
        validation.required_spec_claims.clear();
        let claims = jsonwebtoken::decode::<Value>(token, &key, &validation)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::InvalidSignature => anyhow!("bad signature"),
                _ => anyhow!("{e}"),
            })?
            .claims;

        if claims["iss"].as_str() != Some(self.issuer.as_str()) {
            bail!("wrong issuer");
        }
        let audience_ok = match &claims["aud"] {
            Value::Array(items) => items.iter().any(|a| a == client_id),
            other => other == client_id,
        };
        if !audience_ok {
            bail!("wrong audience");
        }
        match claims["exp"].as_f64() {
            Some(exp) if exp * 1000.0 >= (self.now() - 60_000) as f64 => {}
            _ => bail!("expired"),
        }
        if claims["nonce"].as_str() != Some(nonce) {
            bail!("nonce mismatch");
        }
        if claims["sub"].as_str().is_none_or(str::is_empty) {
            bail!("no subject");
        }
        Ok(claims)
    }
}

fn iso(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::util::testutil::{mock, Mock, Reply, Seen};
    use base64::Engine;
    use jsonwebtoken::{EncodingKey, Header};
    use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
    use serde_json::json;
    use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};

    const START: i64 = 1_800_000_000_000;
    const SCOPE: &str =
        "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";

    struct Keys {
        pem: String,
        jwk: Value,
    }

    fn keys() -> Keys {
        let pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let raw = pair.public_key_raw();
        let b64 = |bytes: &[u8]| base64url(bytes);
        Keys {
            pem: pair.serialize_pem(),
            jwk: json!({ "kty": "EC", "crv": "P-256", "x": b64(&raw[1..33]), "y": b64(&raw[33..65]), "kid": "k1", "alg": "ES256", "use": "sig" }),
        }
    }

    fn jwt(keys: &Keys, claims: Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("k1".to_string());
        jsonwebtoken::encode(
            &header,
            &claims,
            &EncodingKey::from_ec_pem(keys.pem.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    struct Setup {
        net: Mock,
        auth: ChatGptAuth,
        clock: Arc<AtomicI64>,
        home: tempfile::TempDir,
        keys: Arc<Keys>,
        nonce: Arc<Mutex<String>>,
        challenge: Arc<Mutex<String>>,
        tweaks: Arc<Mutex<Tweaks>>,
        refreshes: Arc<AtomicU32>,
    }

    struct Tweaks {
        scope: String,
        token_error: Option<String>,
        revoke_status: u16,
        wrong_issuer: bool,
        wrong_audience: bool,
        fail_first_exchange: bool,
    }

    async fn setup() -> Setup {
        let keys = Arc::new(keys());
        let nonce = Arc::new(Mutex::new(String::new()));
        let refreshes = Arc::new(AtomicU32::new(0));
        let tweaks = Arc::new(Mutex::new(Tweaks {
            scope: SCOPE.to_string(),
            token_error: None,
            revoke_status: 200,
            wrong_issuer: false,
            wrong_audience: false,
            fail_first_exchange: false,
        }));
        let challenge = Arc::new(Mutex::new(String::new()));
        let issuer_cell = Arc::new(Mutex::new(String::new()));
        let net = {
            let (keys, nonce, refreshes, tweaks, issuer_cell, challenge) = (
                keys.clone(),
                nonce.clone(),
                refreshes.clone(),
                tweaks.clone(),
                issuer_cell.clone(),
                challenge.clone(),
            );
            mock(move |seen: &Seen| {
                let issuer = issuer_cell.lock().unwrap().clone();
                let mut tweaks = tweaks.lock().unwrap();
                let form: HashMap<String, String> = url::form_urlencoded::parse(seen.body.as_bytes()).into_owned().collect();
                match seen.path.as_str() {
                    "/.well-known/openid-configuration" => {
                        Reply::json(json!({ "jwks_uri": format!("{issuer}/jwks"), "revocation_endpoint": format!("{issuer}/revoke") }))
                    }
                    "/jwks" => Reply::json(json!({ "keys": [keys.jwk] })),
                    "/revoke" => Reply::status(tweaks.revoke_status, ""),
                    "/api/accounts/oauth/token" => {
                        if let Some(error) = &tweaks.token_error {
                            return Reply { status: 400, content_type: "application/json", body: json!({ "error": error }).to_string() };
                        }
                        if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
                            let n = refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                            return Reply::json(json!({ "access_token": format!("access-r{n}"), "refresh_token": format!("refresh-r{n}"), "expires_in": 3600, "scope": SCOPE }));
                        }
                        let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                        let pkce_ok = sha256_base64url(&verifier) == *challenge.lock().unwrap();
                        if !pkce_ok || std::mem::take(&mut tweaks.fail_first_exchange) {
                            return Reply { status: 400, content_type: "application/json", body: json!({ "error": "invalid_grant" }).to_string() };
                        }
                        let client_id = form.get("client_id").cloned().unwrap_or_default();
                        let id_token = jwt(
                            &keys,
                            json!({
                                "iss": if tweaks.wrong_issuer { "https://evil.test".to_string() } else { issuer.clone() },
                                "aud": if tweaks.wrong_audience { "someone-else".to_string() } else { client_id },
                                "exp": START / 1000 + 600,
                                "nonce": nonce.lock().unwrap().clone(),
                                "sub": "user-1",
                                "email": "me@example.test",
                            }),
                        );
                        Reply::json(json!({ "access_token": "access-1", "refresh_token": "refresh-1", "id_token": id_token, "token_type": "Bearer", "expires_in": 3600, "scope": tweaks.scope }))
                    }
                    other => Reply::status(404, other),
                }
            })
            .await
        };
        *issuer_cell.lock().unwrap() = net.base.clone();
        let clock = Arc::new(AtomicI64::new(START));
        let home = tempfile::tempdir().unwrap();
        let tick = clock.clone();
        let auth = ChatGptAuth::with(
            home.path().to_path_buf(),
            &net.base,
            Arc::new(move || tick.load(Ordering::SeqCst)),
            Duration::from_millis(0),
        );
        Setup {
            net,
            auth,
            clock,
            home,
            keys,
            nonce,
            challenge,
            tweaks,
            refreshes,
        }
    }

    fn query(url: &str) -> HashMap<String, String> {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    async fn sign_in(t: &Setup, client_id: &str) -> (HashMap<String, String>, CallbackOutcome) {
        let url = query(&t.auth.start(4321, false).unwrap());
        *t.nonce.lock().unwrap() = url["nonce"].clone();
        *t.challenge.lock().unwrap() = url["code_challenge"].clone();
        let params = HashMap::from([
            ("code".to_string(), "code-1".to_string()),
            ("state".to_string(), url["state"].clone()),
            ("scope".to_string(), SCOPE.to_string()),
            ("client_id".to_string(), client_id.to_string()),
        ]);
        let result = t.auth.handle_callback(&params).await;
        (url, result)
    }

    #[tokio::test]
    async fn first_sign_in_registers_validates_the_id_token_and_stores_0600_credentials() {
        use std::os::unix::fs::PermissionsExt;
        let t = setup().await;
        let (url, result) = sign_in(&t, "oaiapp_abc").await;
        assert_eq!(url["client_id"], "dynamic_agent_client");
        assert_eq!(url["agent_name_hint"], "kcore");
        assert!(url["ext_agent_host_id"].starts_with("urn:uuid:"));
        assert_eq!(url["redirect_uri"], "http://127.0.0.1:4321/auth/callback");
        assert_eq!(url["code_challenge_method"], "S256");
        assert_eq!(url["resource"], "https://api.openai.com/v1");
        assert!(url["scope"].contains("chatgpt.tokens.use.direct"));
        assert!(result.ok, "{}", result.message);

        let exchange: HashMap<String, String> = url::form_urlencoded::parse(
            t.net
                .requests()
                .iter()
                .find(|r| r.body.contains("authorization_code"))
                .unwrap()
                .body
                .as_bytes(),
        )
        .into_owned()
        .collect();
        assert_eq!(exchange["client_id"], "oaiapp_abc");
        assert_eq!(
            exchange["redirect_uri"],
            "http://127.0.0.1:4321/auth/callback"
        );
        assert!(!exchange["code_verifier"].is_empty());

        let file = t.home.path().join("chatgpt.json");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        assert_eq!(saved["client_id"], "oaiapp_abc");
        assert_eq!(saved["subject"], "user-1");
        assert_eq!(t.auth.access_token(false).await.unwrap(), "access-1");
        let status = t.auth.status();
        assert!(status.signed_in && status.returning && !status.pending);
        assert_eq!(status.email.as_deref(), Some("me@example.test"));
    }

    #[tokio::test]
    async fn a_refused_first_code_retries_with_the_issued_registration() {
        let t = setup().await;
        t.tweaks.lock().unwrap().fail_first_exchange = true;
        let (_, first) = sign_in(&t, "oaiapp_abc").await;
        assert!(!first.ok);
        let retry = query(&first.retry_url.expect("a retry url"));
        assert_eq!(retry["client_id"], "oaiapp_abc");
        assert!(!retry.contains_key("agent_name_hint"));
        assert_eq!(retry["redirect_uri"], "http://127.0.0.1:4321/auth/callback");
        assert!(!t.auth.status().signed_in);
        assert!(t.auth.status().returning);

        *t.nonce.lock().unwrap() = retry["nonce"].clone();
        *t.challenge.lock().unwrap() = retry["code_challenge"].clone();
        let params = HashMap::from([
            ("code".to_string(), "code-2".to_string()),
            ("state".to_string(), retry["state"].clone()),
            ("scope".to_string(), SCOPE.to_string()),
        ]);
        let second = t.auth.handle_callback(&params).await;
        assert!(second.ok, "{}", second.message);
        assert!(t.auth.status().signed_in);
    }

    #[tokio::test]
    async fn a_second_refusal_is_reported_with_the_details_and_does_not_loop() {
        let t = setup().await;
        t.tweaks.lock().unwrap().token_error = Some("invalid_grant".to_string());
        let (_, first) = sign_in(&t, "oaiapp_abc").await;
        let retry = query(&first.retry_url.expect("one retry"));
        let params = HashMap::from([
            ("code".to_string(), "code-2".to_string()),
            ("state".to_string(), retry["state"].clone()),
        ]);
        let second = t.auth.handle_callback(&params).await;
        assert!(second.retry_url.is_none());
        assert!(
            second.message.contains("oaiapp_abc") && second.message.contains("127.0.0.1:4321"),
            "{}",
            second.message
        );
    }

    #[tokio::test]
    async fn the_host_id_is_chosen_once_and_kept() {
        let t = setup().await;
        let first = t.auth.host_id().unwrap();
        let again = ChatGptAuth::new(t.home.path().to_path_buf());
        assert_eq!(again.host_id().unwrap(), first);
    }

    #[tokio::test]
    async fn state_is_single_use_and_must_be_known() {
        let t = setup().await;
        let (url, _) = sign_in(&t, "oaiapp_abc").await;
        let replay = HashMap::from([
            ("code".into(), "x".into()),
            ("state".into(), url["state"].clone()),
            ("client_id".into(), "oaiapp_abc".into()),
        ]);
        assert!(!t.auth.handle_callback(&replay).await.ok);
        let stranger = HashMap::from([
            ("code".into(), "x".into()),
            ("state".into(), "nope".into()),
            ("client_id".into(), "oaiapp_abc".into()),
        ]);
        assert!(!t.auth.handle_callback(&stranger).await.ok);
    }

    #[tokio::test]
    async fn a_declined_sign_in_exchanges_nothing() {
        let t = setup().await;
        let url = query(&t.auth.start(4321, false).unwrap());
        let denied = HashMap::from([
            ("error".into(), "access_denied".into()),
            ("state".into(), url["state"].clone()),
        ]);
        let result = t.auth.handle_callback(&denied).await;
        assert!(!result.ok);
        assert!(result.message.contains("declined"));
        assert!(t.net.requests().is_empty());
        assert!(!t.auth.status().signed_in);
    }

    #[tokio::test]
    async fn registration_without_an_issued_client_id_is_refused() {
        let t = setup().await;
        let url = query(&t.auth.start(4321, false).unwrap());
        let params = HashMap::from([
            ("code".into(), "c".into()),
            ("state".into(), url["state"].clone()),
        ]);
        assert!(!t.auth.handle_callback(&params).await.ok);
        let url = query(&t.auth.start(4321, false).unwrap());
        let params = HashMap::from([
            ("code".into(), "c".into()),
            ("state".into(), url["state"].clone()),
            ("client_id".into(), "dynamic_agent_client".into()),
        ]);
        assert!(!t.auth.handle_callback(&params).await.ok);
    }

    #[tokio::test]
    async fn an_id_token_with_a_bad_issuer_audience_or_nonce_is_rejected() {
        let t = setup().await;
        t.tweaks.lock().unwrap().wrong_issuer = true;
        assert!(!sign_in(&t, "oaiapp_abc").await.1.ok);
        t.tweaks.lock().unwrap().wrong_issuer = false;
        t.tweaks.lock().unwrap().wrong_audience = true;
        assert!(!sign_in(&t, "oaiapp_abc").await.1.ok);
        t.tweaks.lock().unwrap().wrong_audience = false;

        let url = query(&t.auth.start(4321, false).unwrap());
        *t.challenge.lock().unwrap() = url["code_challenge"].clone();
        *t.nonce.lock().unwrap() = "not-the-nonce".to_string();
        let params = HashMap::from([
            ("code".into(), "c".into()),
            ("state".into(), url["state"].clone()),
            ("client_id".into(), "oaiapp_abc".into()),
        ]);
        let result = t.auth.handle_callback(&params).await;
        assert!(
            !result.ok && result.message.contains("nonce"),
            "{}",
            result.message
        );
        assert!(!t.auth.status().signed_in);
    }

    #[tokio::test]
    async fn a_token_signed_by_another_key_is_rejected() {
        let t = setup().await;
        let url = query(&t.auth.start(4321, false).unwrap());
        *t.nonce.lock().unwrap() = url["nonce"].clone();
        let stranger = keys();
        let forged = jwt(
            &stranger,
            json!({ "iss": t.net.base, "aud": "oaiapp_abc", "exp": START / 1000 + 600, "nonce": url["nonce"], "sub": "u" }),
        );
        let outcome = t
            .auth
            .verify_id_token(&forged, "oaiapp_abc", &url["nonce"])
            .await;
        assert!(outcome.unwrap_err().to_string().contains("bad signature"));
        let genuine = jwt(
            &t.keys,
            json!({ "iss": t.net.base, "aud": "oaiapp_abc", "exp": START / 1000 + 600, "nonce": url["nonce"], "sub": "u" }),
        );
        assert!(t
            .auth
            .verify_id_token(&genuine, "oaiapp_abc", &url["nonce"])
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn without_the_direct_scope_the_account_is_not_usable() {
        let t = setup().await;
        t.tweaks.lock().unwrap().scope = "openid email".to_string();
        let url = query(&t.auth.start(4321, false).unwrap());
        *t.nonce.lock().unwrap() = url["nonce"].clone();
        *t.challenge.lock().unwrap() = url["code_challenge"].clone();
        let params = HashMap::from([
            ("code".into(), "c".into()),
            ("state".into(), url["state"].clone()),
            ("client_id".into(), "oaiapp_abc".into()),
        ]);
        let result = t.auth.handle_callback(&params).await;
        assert!(
            !result.ok && result.message.contains("plan use was not granted"),
            "{}",
            result.message
        );
        assert!(!t.auth.status().signed_in);
    }

    #[tokio::test]
    async fn tokens_refresh_three_minutes_before_expiry_and_the_rotated_token_replaces_the_old_one()
    {
        let t = setup().await;
        assert!(sign_in(&t, "oaiapp_abc").await.1.ok);
        t.clock.fetch_add(3_000_000, Ordering::SeqCst);
        assert_eq!(t.auth.access_token(false).await.unwrap(), "access-1");
        t.clock.fetch_add(500_000, Ordering::SeqCst);
        assert_eq!(t.auth.access_token(false).await.unwrap(), "access-r1");
        assert_eq!(t.auth.access_token(false).await.unwrap(), "access-r1");
        assert_eq!(t.refreshes.load(Ordering::SeqCst), 1);
        let refresh = t
            .net
            .requests()
            .into_iter()
            .rfind(|r| r.body.contains("grant_type=refresh_token"))
            .unwrap();
        assert!(refresh.body.contains("refresh_token=refresh-1"));
        assert_eq!(t.auth.access_token(true).await.unwrap(), "access-r2");
    }

    #[tokio::test]
    async fn a_refresh_token_openai_no_longer_accepts_signs_out_but_keeps_the_registration() {
        let t = setup().await;
        assert!(sign_in(&t, "oaiapp_abc").await.1.ok);
        t.clock.fetch_add(3_600_000, Ordering::SeqCst);
        t.tweaks.lock().unwrap().token_error = Some("invalid_grant".to_string());
        let error = t.auth.access_token(false).await.unwrap_err();
        assert!(error.downcast_ref::<NotSignedIn>().is_some(), "{error}");
        let status = t.auth.status();
        assert!(!status.signed_in);
        assert!(status.returning);
        assert!(status.error.unwrap().contains("Sign in again"));
    }

    #[tokio::test]
    async fn signing_in_again_reuses_the_client_id_and_another_account_is_refused() {
        let t = setup().await;
        assert!(sign_in(&t, "oaiapp_abc").await.1.ok);
        let url = query(&t.auth.start(4321, false).unwrap());
        assert_eq!(url["client_id"], "oaiapp_abc");
        assert!(!url.contains_key("agent_name_hint"));
        assert_eq!(url["login_hint"], "me@example.test");
        assert!(url.contains_key("id_token_hint"));
        let fresh = query(&t.auth.start(4321, true).unwrap());
        assert_eq!(fresh["client_id"], "dynamic_agent_client");
        let other = HashMap::from([
            ("code".into(), "c".into()),
            ("state".into(), url["state"].clone()),
            ("client_id".into(), "oaiapp_other".into()),
        ]);
        assert!(!t.auth.handle_callback(&other).await.ok);
    }

    #[tokio::test]
    async fn sign_out_revokes_clears_tokens_and_keeps_the_registration() {
        let t = setup().await;
        assert!(sign_in(&t, "oaiapp_abc").await.1.ok);
        assert!(t.auth.sign_out().await);
        let revoke = t
            .net
            .requests()
            .into_iter()
            .find(|r| r.path == "/revoke")
            .unwrap();
        assert!(
            revoke.body.contains("token=refresh-1")
                && revoke.body.contains("token_type_hint=refresh_token")
        );
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(t.home.path().join("chatgpt.json")).unwrap(),
        )
        .unwrap();
        assert!(saved.get("access_token").is_none() && saved.get("refresh_token").is_none());
        assert_eq!(saved["client_id"], "oaiapp_abc");
        assert!(t.auth.access_token(false).await.is_err());
    }

    #[tokio::test]
    async fn sign_out_still_clears_locally_when_revocation_cannot_be_confirmed() {
        let t = setup().await;
        assert!(sign_in(&t, "oaiapp_abc").await.1.ok);
        t.tweaks.lock().unwrap().revoke_status = 503;
        assert!(!t.auth.sign_out().await);
        assert!(!t.auth.status().signed_in);
        assert_eq!(
            t.net
                .requests()
                .iter()
                .filter(|r| r.path == "/revoke")
                .count(),
            3
        );
    }

    #[test]
    fn unpadded_base64url_is_what_the_sha256_challenge_uses() {
        let digest = sha256_base64url("abc");
        assert!(!digest.contains('='));
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(digest)
                .unwrap()
                .len(),
            32
        );
    }
}
