use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tonic::{Request, Response, Status};
use tracing::info;

use super::secrets;
use super::{
    credentials_body, role_from_proto, role_to_proto, token_body, EVT_API_TOKEN_CREATE,
    EVT_API_TOKEN_REVOKE, EVT_CREDENTIALS_SET,
};
use crate::auth::{self, OperatorRole, WebIdentity, WebTokenKind};
use crate::config::WebConfig;
use crate::controller_proto as pb;
use crate::db::{AuthTokenRow, Database, OperatorCredentials};
use crate::grpc::ControllerService;

const PENDING_TTL_SECS: i64 = 300;
const LOGIN_ATTEMPTS_PER_IP_PER_MINUTE: u32 = 20;
const MAX_IP_BUCKETS: usize = 10_000;
const DEFAULT_API_TOKEN_DAYS: i32 = 90;
const MAX_API_TOKEN_DAYS: i32 = 365;
const BAD_LOGIN: &str = "invalid username or password";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn ts(secs: i64) -> Option<prost_types::Timestamp> {
    (secs > 0).then_some(prost_types::Timestamp {
        seconds: secs,
        nanos: 0,
    })
}

fn db_err(e: rusqlite::Error) -> Status {
    Status::internal(format!("auth store: {e}"))
}

/// Who is calling `Auth`, on either listener.
enum Caller {
    Web(WebIdentity),
    /// `CN=kctl:<name>` client certificate.
    Operator(String),
    /// Legacy `CN=kctl`, a peer controller, or a plaintext listener.
    Other(String),
}

#[derive(Clone)]
pub struct AuthService {
    db: Database,
    controller: ControllerService,
    web: WebConfig,
    tls_active: bool,
    bootstrap_kctl: bool,
    password_iterations: u32,
    ip_attempts: Arc<Mutex<HashMap<IpAddr, (Instant, u32)>>>,
}

impl AuthService {
    pub fn new(
        db: Database,
        controller: ControllerService,
        web: WebConfig,
        tls_active: bool,
        bootstrap_kctl: bool,
    ) -> Self {
        Self {
            db,
            controller,
            web,
            tls_active,
            bootstrap_kctl,
            password_iterations: secrets::PASSWORD_ITERATIONS,
            ip_attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    fn with_password_iterations(mut self, iterations: u32) -> Self {
        self.password_iterations = iterations;
        self
    }

    fn caller<T>(&self, request: &Request<T>) -> Caller {
        if let Some(web) = auth::web_identity(request) {
            return Caller::Web(web.clone());
        }
        match auth::peer_cn(request) {
            Some(cn) => match cn.strip_prefix(auth::CN_KCTL_PREFIX) {
                Some(name) => Caller::Operator(name.to_string()),
                None => Caller::Other(cn),
            },
            None => Caller::Other("insecure".into()),
        }
    }

    fn session<T>(&self, request: &Request<T>) -> Result<WebIdentity, Status> {
        match auth::web_identity(request) {
            Some(web) if web.kind == WebTokenKind::Session => Ok(web.clone()),
            Some(_) => Err(Status::permission_denied(
                "this RPC needs a signed-in session, not an API or pending token",
            )),
            None => Err(Status::failed_precondition(
                "this RPC is only available on the web listener",
            )),
        }
    }

    fn pending<T>(&self, request: &Request<T>) -> Result<WebIdentity, Status> {
        match auth::web_identity(request) {
            Some(web) if web.kind == WebTokenKind::Pending => Ok(web.clone()),
            _ => Err(Status::failed_precondition(
                "send the pending token from Login",
            )),
        }
    }

    fn require_admin<T>(&self, request: &Request<T>) -> Result<(), Status> {
        auth::require_controller_operator(
            request,
            &self.db,
            self.tls_active,
            self.bootstrap_kctl,
            OperatorRole::ClusterAdmin,
        )
    }

    fn effective_role<T>(&self, request: &Request<T>) -> Result<OperatorRole, Status> {
        auth::controller_operator_role(request, &self.db, self.tls_active, self.bootstrap_kctl)
    }

    fn throttle_ip<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let Some(addr) = auth::remote_addr(request) else {
            return Ok(());
        };
        let mut map = self.ip_attempts.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if map.len() >= MAX_IP_BUCKETS {
            map.retain(|_, (start, _)| now.duration_since(*start).as_secs() < 60);
        }
        let entry = map.entry(addr.ip()).or_insert((now, 0));
        if now.duration_since(entry.0).as_secs() >= 60 {
            *entry = (now, 0);
        }
        entry.1 += 1;
        if entry.1 > LOGIN_ATTEMPTS_PER_IP_PER_MINUTE {
            return Err(Status::resource_exhausted(
                "too many sign-in attempts from this address; wait a minute",
            ));
        }
        Ok(())
    }

    fn credentials(&self, name: &str) -> Result<OperatorCredentials, Status> {
        Ok(self
            .db
            .get_operator_credentials(name)
            .map_err(db_err)?
            .unwrap_or_else(|| OperatorCredentials {
                operator_name: name.to_string(),
                ..Default::default()
            }))
    }

    fn ensure_not_locked(&self, name: &str) -> Result<(), Status> {
        let state = self.db.get_login_state(name).map_err(db_err)?;
        if state.locked_until > now() {
            return Err(Status::unauthenticated(
                "too many failed attempts; this operator is locked for a while",
            ));
        }
        Ok(())
    }

    fn fail(&self, name: &str, actor: &str, action: &str, reason: &str) -> Status {
        let state = self.db.record_login_failure(
            name,
            now(),
            self.web.max_failed_logins,
            self.web.lockout_secs,
        );
        let locked = matches!(&state, Ok(s) if s.locked_until > now());
        if locked {
            let _ = self.db.delete_operator_tokens(name, "pending", None);
            self.controller.record_audit(
                actor,
                "auth.locked",
                &format!("operator/{name}"),
                format!("locked for {}s", self.web.lockout_secs),
            );
        }
        self.controller
            .record_audit(actor, action, &format!("operator/{name}"), reason);
        Status::unauthenticated(reason.to_string())
    }

    fn replicate_credentials(
        &self,
        actor: &str,
        action: &str,
        creds: &OperatorCredentials,
        revoke_sessions: bool,
    ) -> Result<(), Status> {
        self.controller.log_replication_event_required(
            actor,
            Some(action),
            EVT_CREDENTIALS_SET,
            &format!("operator_credentials/{}", creds.operator_name),
            credentials_body(creds, revoke_sessions),
        )
    }

    fn issue(
        &self,
        operator: &str,
        kind: WebTokenKind,
        name: &str,
        ttl_secs: i64,
        max_role: Option<OperatorRole>,
        created_from: &str,
    ) -> Result<(String, AuthTokenRow), Status> {
        let prefix = match kind {
            WebTokenKind::Pending => "kcp_",
            WebTokenKind::Session => "kcs_",
            WebTokenKind::Api => "kct_",
        };
        let (token, hash) = secrets::new_token(prefix);
        let created = now();
        let row = AuthTokenRow {
            id: secrets::new_token_id(),
            token_hash: hash,
            operator_name: operator.to_string(),
            kind: kind.as_db_str().to_string(),
            name: name.to_string(),
            max_role: max_role
                .map(|r| r.as_db_str().to_string())
                .unwrap_or_default(),
            created_at: created,
            expires_at: created + ttl_secs,
            last_used_at: if kind == WebTokenKind::Api {
                0
            } else {
                created
            },
            created_from: created_from.to_string(),
        };
        self.db.insert_auth_token(&row).map_err(db_err)?;
        Ok((token, row))
    }

    fn start_session<T>(
        &self,
        request: &Request<T>,
        pending: &WebIdentity,
    ) -> Result<pb::AuthSession, Status> {
        self.db
            .delete_auth_token(&pending.token_id)
            .map_err(db_err)?;
        self.db
            .clear_login_failures(&pending.operator)
            .map_err(db_err)?;
        let from = auth::remote_addr(request)
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        let (token, row) = self.issue(
            &pending.operator,
            WebTokenKind::Session,
            "",
            self.web.session_ttl_secs,
            None,
            &from,
        )?;
        self.controller.record_audit(
            &format!("web:{}", pending.operator),
            "auth.login",
            &format!("operator/{}", pending.operator),
            format!("session {} from {from}", row.id),
        );
        info!(operator = %pending.operator, session = %row.id, "web session started");
        Ok(pb::AuthSession {
            token,
            token_id: row.id,
            expires_at: ts(row.expires_at),
            idle_timeout_secs: self.web.session_idle_secs,
        })
    }

    async fn check_password(&self, password: String, stored: String) -> bool {
        tokio::task::spawn_blocking(move || {
            if stored.is_empty() {
                secrets::burn_password_check(&password);
                false
            } else {
                secrets::verify_password(&password, &stored)
            }
        })
        .await
        .unwrap_or(false)
    }

    async fn hash_password(&self, password: String) -> Result<String, Status> {
        let iterations = self.password_iterations;
        tokio::task::spawn_blocking(move || secrets::hash_password_with(&password, iterations))
            .await
            .map_err(|e| Status::internal(format!("hash password: {e}")))
    }

    fn token_info(&self, row: &AuthTokenRow, current: Option<&str>) -> pb::AuthTokenInfo {
        pb::AuthTokenInfo {
            token_id: row.id.clone(),
            operator: row.operator_name.clone(),
            kind: row.kind.clone(),
            name: row.name.clone(),
            created_at: ts(row.created_at),
            expires_at: ts(row.expires_at),
            last_used_at: ts(row.last_used_at),
            created_from: row.created_from.clone(),
            max_role: OperatorRole::from_db_str(&row.max_role)
                .map(|r| role_to_proto(r) as i32)
                .unwrap_or(0),
            current: current == Some(row.id.as_str()),
        }
    }

    fn require_operator_exists(&self, name: &str) -> Result<(), Status> {
        auth::validate_operator_name(name)?;
        if self.db.get_operator_row(name).map_err(db_err)?.is_none() {
            return Err(Status::not_found(format!("operator '{name}' not found")));
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl pb::auth_server::Auth for AuthService {
    async fn login(
        &self,
        request: Request<pb::LoginRequest>,
    ) -> Result<Response<pb::LoginResponse>, Status> {
        self.throttle_ip(&request)?;
        let from = auth::remote_addr(&request)
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        let req = request.into_inner();
        let name = req.username.trim().to_string();
        let known = auth::validate_operator_name(&name).is_ok()
            && self.db.get_operator_row(&name).map_err(db_err)?.is_some();
        if !known {
            self.check_password(req.password, String::new()).await;
            return Err(Status::unauthenticated(BAD_LOGIN));
        }
        self.ensure_not_locked(&name)?;
        let creds = self.credentials(&name)?;
        let actor = format!("web:{name}");
        if !self
            .check_password(req.password, creds.password_hash.clone())
            .await
        {
            return Err(self.fail(&name, &actor, "auth.login.failed", BAD_LOGIN));
        }
        let (token, row) = self.issue(
            &name,
            WebTokenKind::Pending,
            "",
            PENDING_TTL_SECS,
            None,
            &from,
        )?;
        let next_step = if creds.totp_enabled {
            pb::LoginStep::Totp
        } else {
            pb::LoginStep::EnrollTotp
        };
        Ok(Response::new(pb::LoginResponse {
            pending_token: token,
            next_step: next_step as i32,
            pending_expires_at: ts(row.expires_at),
        }))
    }

    async fn verify_second_factor(
        &self,
        request: Request<pb::VerifySecondFactorRequest>,
    ) -> Result<Response<pb::VerifySecondFactorResponse>, Status> {
        let pending = self.pending(&request)?;
        let name = pending.operator.clone();
        let actor = format!("web:{name}");
        self.ensure_not_locked(&name)?;
        let mut creds = self.credentials(&name)?;
        if !creds.totp_enabled {
            return Err(Status::failed_precondition(
                "TOTP is not set up; enroll first",
            ));
        }
        let req = request.get_ref();
        let totp = req.totp_code.trim();
        let recovery = req.recovery_code.trim();
        if !totp.is_empty() {
            let Some(step) = secrets::verify_totp(&creds.totp_secret, totp, now()) else {
                return Err(self.fail(&name, &actor, "auth.totp.failed", "invalid code"));
            };
            if !self.db.claim_totp_step(&name, step).map_err(db_err)? {
                return Err(self.fail(&name, &actor, "auth.totp.failed", "code already used"));
            }
        } else if !recovery.is_empty() {
            let Some(index) = secrets::match_recovery_code(recovery, &creds.recovery_codes) else {
                return Err(self.fail(
                    &name,
                    &actor,
                    "auth.recovery.failed",
                    "invalid recovery code",
                ));
            };
            creds.recovery_codes.remove(index);
            creds.updated_at = now().max(creds.updated_at + 1);
            self.db
                .upsert_operator_credentials(&creds)
                .map_err(db_err)?;
            self.replicate_credentials(&actor, "auth.recovery.used", &creds, false)?;
        } else {
            return Err(Status::invalid_argument(
                "totp_code or recovery_code is required",
            ));
        }
        let session = self.start_session(&request, &pending)?;
        Ok(Response::new(pb::VerifySecondFactorResponse {
            session: Some(session),
            recovery_codes_left: creds.recovery_codes.len() as i32,
        }))
    }

    async fn begin_totp_enrollment(
        &self,
        request: Request<pb::BeginTotpEnrollmentRequest>,
    ) -> Result<Response<pb::BeginTotpEnrollmentResponse>, Status> {
        let web = auth::web_identity(&request)
            .cloned()
            .ok_or_else(|| Status::failed_precondition("enroll on the web listener"))?;
        match web.kind {
            WebTokenKind::Api => {
                return Err(Status::permission_denied("an API token cannot change TOTP"))
            }
            // A password alone must not be able to replace a working second factor.
            WebTokenKind::Pending if self.credentials(&web.operator)?.totp_enabled => {
                return Err(Status::failed_precondition(
                    "TOTP is already set up; send VerifySecondFactor",
                ))
            }
            _ => {}
        }
        let secret = secrets::new_totp_secret();
        self.db
            .set_pending_totp_secret(&web.operator, &secret)
            .map_err(db_err)?;
        let issuer = self.web.totp_issuer.clone();
        Ok(Response::new(pb::BeginTotpEnrollmentResponse {
            otpauth_uri: secrets::otpauth_uri(&issuer, &web.operator, &secret),
            secret,
            issuer,
            account: web.operator,
            digits: secrets::TOTP_DIGITS as i32,
            period_secs: secrets::TOTP_PERIOD_SECS as i32,
        }))
    }

    async fn confirm_totp_enrollment(
        &self,
        request: Request<pb::ConfirmTotpEnrollmentRequest>,
    ) -> Result<Response<pb::ConfirmTotpEnrollmentResponse>, Status> {
        let web = auth::web_identity(&request)
            .cloned()
            .ok_or_else(|| Status::failed_precondition("enroll on the web listener"))?;
        let name = web.operator.clone();
        let actor = format!("web:{name}");
        let mut creds = self.credentials(&name)?;
        match web.kind {
            WebTokenKind::Api => {
                return Err(Status::permission_denied("an API token cannot change TOTP"))
            }
            WebTokenKind::Pending if creds.totp_enabled => {
                return Err(Status::failed_precondition(
                    "TOTP is already set up; send VerifySecondFactor",
                ))
            }
            _ => {}
        }
        self.ensure_not_locked(&name)?;
        let state = self.db.get_login_state(&name).map_err(db_err)?;
        if state.pending_totp_secret.is_empty() {
            return Err(Status::failed_precondition(
                "send BeginTotpEnrollment first",
            ));
        }
        let code = request.get_ref().totp_code.clone();
        let Some(step) = secrets::verify_totp(&state.pending_totp_secret, &code, now()) else {
            return Err(self.fail(&name, &actor, "auth.totp.failed", "invalid code"));
        };
        if !self.db.claim_totp_step(&name, step).map_err(db_err)? {
            return Err(self.fail(&name, &actor, "auth.totp.failed", "code already used"));
        }
        let (codes, hashes) = secrets::new_recovery_codes();
        creds.totp_secret = state.pending_totp_secret;
        creds.totp_enabled = true;
        creds.recovery_codes = hashes;
        creds.updated_at = now().max(creds.updated_at + 1);
        self.db
            .upsert_operator_credentials(&creds)
            .map_err(db_err)?;
        self.db.set_pending_totp_secret(&name, "").map_err(db_err)?;
        self.replicate_credentials(&actor, "auth.totp.enrolled", &creds, false)?;
        let session = if web.kind == WebTokenKind::Pending {
            Some(self.start_session(&request, &web)?)
        } else {
            self.db
                .delete_operator_tokens(&name, "session", Some(&web.token_id))
                .map_err(db_err)?;
            None
        };
        Ok(Response::new(pb::ConfirmTotpEnrollmentResponse {
            recovery_codes: codes,
            session,
        }))
    }

    async fn logout(
        &self,
        request: Request<pb::LogoutRequest>,
    ) -> Result<Response<pb::LogoutResponse>, Status> {
        if let Some(web) = auth::web_identity(&request) {
            self.db.delete_auth_token(&web.token_id).map_err(db_err)?;
            if web.kind == WebTokenKind::Api {
                self.controller.log_replication_event_required(
                    &ControllerService::audit_actor(&request),
                    Some("auth.token.revoked"),
                    EVT_API_TOKEN_REVOKE,
                    &format!("auth_token/{}", web.token_id),
                    serde_json::json!({ "id": web.token_id }),
                )?;
            } else {
                self.controller.record_audit(
                    &format!("web:{}", web.operator),
                    "auth.logout",
                    &format!("operator/{}", web.operator),
                    format!("session {}", web.token_id),
                );
            }
        }
        Ok(Response::new(pb::LogoutResponse {}))
    }

    async fn who_am_i(
        &self,
        request: Request<pb::WhoAmIRequest>,
    ) -> Result<Response<pb::WhoAmIResponse>, Status> {
        let (operator, method, token_id, expires_at) = match self.caller(&request) {
            Caller::Web(web) => {
                let method = match web.kind {
                    WebTokenKind::Api => pb::AuthMethod::ApiToken,
                    _ => pb::AuthMethod::Session,
                };
                (web.operator, method, web.token_id, web.expires_at)
            }
            Caller::Operator(name) => (name, pb::AuthMethod::ClientCertificate, String::new(), 0),
            Caller::Other(name) => {
                let method = if name == "insecure" {
                    pb::AuthMethod::Insecure
                } else {
                    pb::AuthMethod::ClientCertificate
                };
                (name, method, String::new(), 0)
            }
        };
        let effective = self.effective_role(&request)?;
        let roles: Vec<i32> = self
            .db
            .list_operator_role_strings(&operator)
            .map_err(db_err)?
            .iter()
            .filter_map(|r| OperatorRole::from_db_str(r))
            .map(|r| role_to_proto(r) as i32)
            .collect();
        let creds = self
            .db
            .get_operator_credentials(&operator)
            .map_err(db_err)?
            .unwrap_or_default();
        Ok(Response::new(pb::WhoAmIResponse {
            operator,
            method: method as i32,
            effective_role: role_to_proto(effective) as i32,
            roles,
            totp_enabled: creds.totp_enabled,
            password_set: !creds.password_hash.is_empty(),
            token_id,
            expires_at: ts(expires_at),
        }))
    }

    async fn change_password(
        &self,
        request: Request<pb::ChangePasswordRequest>,
    ) -> Result<Response<pb::ChangePasswordResponse>, Status> {
        let web = self.session(&request)?;
        let name = web.operator.clone();
        let actor = format!("web:{name}");
        self.ensure_not_locked(&name)?;
        let req = request.into_inner();
        secrets::check_password_policy(&req.new_password).map_err(Status::invalid_argument)?;
        let mut creds = self.credentials(&name)?;
        if !self
            .check_password(req.current_password, creds.password_hash.clone())
            .await
        {
            return Err(self.fail(
                &name,
                &actor,
                "auth.password.failed",
                "current password is wrong",
            ));
        }
        creds.password_hash = self.hash_password(req.new_password).await?;
        creds.updated_at = now().max(creds.updated_at + 1);
        self.db
            .upsert_operator_credentials(&creds)
            .map_err(db_err)?;
        let revoked = self
            .db
            .delete_operator_tokens(&name, "session", Some(&web.token_id))
            .map_err(db_err)?;
        self.replicate_credentials(&actor, "auth.password.changed", &creds, false)?;
        Ok(Response::new(pb::ChangePasswordResponse {
            sessions_revoked: revoked as i32,
        }))
    }

    async fn list_sessions(
        &self,
        request: Request<pb::ListSessionsRequest>,
    ) -> Result<Response<pb::ListSessionsResponse>, Status> {
        let (operator, current) = match self.caller(&request) {
            Caller::Web(web) => (web.operator, Some(web.token_id)),
            Caller::Operator(name) => (name, None),
            Caller::Other(_) => return Ok(Response::new(pb::ListSessionsResponse::default())),
        };
        let now = now();
        let sessions = self
            .db
            .list_auth_tokens(&operator, "session")
            .map_err(db_err)?
            .iter()
            .filter(|row| row.expires_at > now)
            .map(|row| self.token_info(row, current.as_deref()))
            .collect();
        Ok(Response::new(pb::ListSessionsResponse { sessions }))
    }

    async fn revoke_session(
        &self,
        request: Request<pb::RevokeSessionRequest>,
    ) -> Result<Response<pb::RevokeSessionResponse>, Status> {
        let id = request.get_ref().token_id.trim().to_string();
        let row = self
            .db
            .get_auth_token(&id)
            .map_err(db_err)?
            .filter(|row| row.kind == "session")
            .ok_or_else(|| Status::not_found("session not found"))?;
        let own = match self.caller(&request) {
            Caller::Web(web) => web.operator == row.operator_name,
            Caller::Operator(name) => name == row.operator_name,
            Caller::Other(_) => false,
        };
        if !own {
            self.require_admin(&request)?;
        }
        self.db.delete_auth_token(&id).map_err(db_err)?;
        self.controller.record_audit(
            &ControllerService::audit_actor(&request),
            "auth.session.revoked",
            &format!("operator/{}", row.operator_name),
            format!("session {id}"),
        );
        Ok(Response::new(pb::RevokeSessionResponse {}))
    }

    async fn create_api_token(
        &self,
        request: Request<pb::CreateApiTokenRequest>,
    ) -> Result<Response<pb::CreateApiTokenResponse>, Status> {
        let web = self.session(&request)?;
        let effective = self.effective_role(&request)?;
        let req = request.get_ref();
        let name = req.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(Status::invalid_argument(
                "name is required and must be at most 64 characters",
            ));
        }
        let days = match req.expires_in_days {
            0 => DEFAULT_API_TOKEN_DAYS,
            d @ 1..=MAX_API_TOKEN_DAYS => d,
            _ => {
                return Err(Status::invalid_argument(format!(
                    "expires_in_days must be between 1 and {MAX_API_TOKEN_DAYS}"
                )))
            }
        };
        let max_role = role_from_proto(req.max_role);
        if let Some(cap) = max_role {
            if !effective.satisfies(cap) {
                return Err(Status::permission_denied(format!(
                    "max_role {cap:?} is above your role {effective:?}"
                )));
            }
        }
        let from = auth::remote_addr(&request)
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        let (token, row) = self.issue(
            &web.operator,
            WebTokenKind::Api,
            name,
            i64::from(days) * 86_400,
            max_role,
            &from,
        )?;
        self.controller.log_replication_event_required(
            &format!("web:{}", web.operator),
            Some("auth.token.created"),
            EVT_API_TOKEN_CREATE,
            &format!("auth_token/{}", row.id),
            token_body(&row),
        )?;
        Ok(Response::new(pb::CreateApiTokenResponse {
            token,
            info: Some(self.token_info(&row, None)),
        }))
    }

    async fn list_api_tokens(
        &self,
        request: Request<pb::ListApiTokensRequest>,
    ) -> Result<Response<pb::ListApiTokensResponse>, Status> {
        let asked = request.get_ref().operator.trim().to_string();
        let (own, current) = match self.caller(&request) {
            Caller::Web(web) => (Some(web.operator), Some(web.token_id)),
            Caller::Operator(name) => (Some(name), None),
            Caller::Other(_) => (None, None),
        };
        let operator = if asked.is_empty() {
            own.ok_or_else(|| Status::invalid_argument("operator is required"))?
        } else {
            if own.as_deref() != Some(asked.as_str()) {
                self.require_admin(&request)?;
            }
            asked
        };
        let now = now();
        let tokens = self
            .db
            .list_auth_tokens(&operator, "api")
            .map_err(db_err)?
            .iter()
            .filter(|row| row.expires_at > now)
            .map(|row| self.token_info(row, current.as_deref()))
            .collect();
        Ok(Response::new(pb::ListApiTokensResponse { tokens }))
    }

    async fn revoke_api_token(
        &self,
        request: Request<pb::RevokeApiTokenRequest>,
    ) -> Result<Response<pb::RevokeApiTokenResponse>, Status> {
        let id = request.get_ref().token_id.trim().to_string();
        let row = self
            .db
            .get_auth_token(&id)
            .map_err(db_err)?
            .filter(|row| row.kind == "api")
            .ok_or_else(|| Status::not_found("API token not found"))?;
        let own = match self.caller(&request) {
            Caller::Web(web) => web.operator == row.operator_name,
            Caller::Operator(name) => name == row.operator_name,
            Caller::Other(_) => false,
        };
        if !own {
            self.require_admin(&request)?;
        }
        self.db.delete_auth_token(&id).map_err(db_err)?;
        self.controller.log_replication_event_required(
            &ControllerService::audit_actor(&request),
            Some("auth.token.revoked"),
            EVT_API_TOKEN_REVOKE,
            &format!("auth_token/{id}"),
            serde_json::json!({ "id": id }),
        )?;
        Ok(Response::new(pb::RevokeApiTokenResponse {}))
    }

    async fn set_operator_password(
        &self,
        request: Request<pb::SetOperatorPasswordRequest>,
    ) -> Result<Response<pb::SetOperatorPasswordResponse>, Status> {
        self.require_admin(&request)?;
        let actor = ControllerService::audit_actor(&request);
        let req = request.into_inner();
        let name = req.name.trim().to_string();
        self.require_operator_exists(&name)?;
        secrets::check_password_policy(&req.password).map_err(Status::invalid_argument)?;
        let mut creds = self.credentials(&name)?;
        creds.password_hash = self.hash_password(req.password).await?;
        creds.updated_at = now().max(creds.updated_at + 1);
        self.db
            .upsert_operator_credentials(&creds)
            .map_err(db_err)?;
        let revoked = self
            .db
            .delete_operator_tokens(&name, "session", None)
            .map_err(db_err)?;
        self.db
            .delete_operator_tokens(&name, "pending", None)
            .map_err(db_err)?;
        self.db.clear_login_failures(&name).map_err(db_err)?;
        self.replicate_credentials(&actor, "auth.password.set", &creds, true)?;
        Ok(Response::new(pb::SetOperatorPasswordResponse {
            sessions_revoked: revoked as i32,
        }))
    }

    async fn reset_operator_second_factor(
        &self,
        request: Request<pb::ResetOperatorSecondFactorRequest>,
    ) -> Result<Response<pb::ResetOperatorSecondFactorResponse>, Status> {
        self.require_admin(&request)?;
        let actor = ControllerService::audit_actor(&request);
        let name = request.get_ref().name.trim().to_string();
        self.require_operator_exists(&name)?;
        let mut creds = self.credentials(&name)?;
        creds.totp_secret.clear();
        creds.totp_enabled = false;
        creds.recovery_codes.clear();
        creds.updated_at = now().max(creds.updated_at + 1);
        self.db
            .upsert_operator_credentials(&creds)
            .map_err(db_err)?;
        self.db.set_pending_totp_secret(&name, "").map_err(db_err)?;
        let revoked = self
            .db
            .delete_operator_tokens(&name, "session", None)
            .map_err(db_err)?;
        self.db
            .delete_operator_tokens(&name, "pending", None)
            .map_err(db_err)?;
        self.replicate_credentials(&actor, "auth.totp.reset", &creds, true)?;
        Ok(Response::new(pb::ResetOperatorSecondFactorResponse {
            sessions_revoked: revoked as i32,
        }))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
