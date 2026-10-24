use std::sync::{Arc, Mutex};

use http::{header, HeaderMap, HeaderValue};
use tonic::{Code, Request};

use super::AuthService;
use crate::auth::{OperatorRole, WebIdentity};
use crate::config::{NetworkConfig, WebConfig};
use crate::controller_proto as pb;
use crate::controller_proto::auth_server::Auth;
use crate::db::Database;
use crate::grpc::{ControllerService, SubCaState};
use crate::node_client::NodeClients;
use crate::web_auth::listener::{GateDecision, TokenGate};
use crate::web_auth::secrets;

const PASSWORD: &str = "correct horse battery staple";

struct Harness {
    db: Database,
    auth: AuthService,
    gate: TokenGate,
}

fn harness(max_failed: i64) -> Harness {
    let db = Database::open(":memory:").unwrap();
    let controller = ControllerService::new(
        db.clone(),
        NodeClients::new(None),
        NetworkConfig {
            gateway_interface: "eno1".into(),
            external_ip: "203.0.113.10".into(),
            gateway_ip: "10.0.0.1".into(),
            internal_netmask: "255.255.255.0".into(),
        },
        Arc::new(Mutex::new(SubCaState::default())),
        None,
        false,
        false,
    );
    let web = WebConfig {
        enabled: true,
        max_failed_logins: max_failed,
        ..WebConfig::default()
    };
    let auth =
        AuthService::new(db.clone(), controller, web, false, false).with_password_iterations(1000);
    db.create_operator("alice").unwrap();
    db.grant_operator_role_str("alice", "vm-admin").unwrap();
    Harness {
        gate: TokenGate::new(db.clone(), 3600),
        db,
        auth,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn code(secret: &str, offset_steps: i64) -> String {
    let raw = secrets::base32_decode(secret).unwrap();
    format!(
        "{:06}",
        secrets::totp_code(&raw, now() / secrets::TOTP_PERIOD_SECS + offset_steps)
    )
}

impl Harness {
    fn identity(&self, path: &str, token: &str) -> Result<WebIdentity, Code> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        match self.gate.check(path, &headers, now()) {
            GateDecision::Identity(id) => Ok(id),
            GateDecision::Deny(code, _) => Err(code),
            GateDecision::Public => panic!("{path} is not public"),
        }
    }

    fn with_token<T>(&self, method: &str, token: &str, msg: T) -> Request<T> {
        let identity = self
            .identity(&format!("/kcore.controller.Auth/{method}"), token)
            .unwrap_or_else(|code| panic!("{method}: gate said {code:?}"));
        let mut req = Request::new(msg);
        req.extensions_mut().insert(identity);
        req
    }

    async fn set_password(&self) {
        self.auth
            .set_operator_password(Request::new(pb::SetOperatorPasswordRequest {
                name: "alice".into(),
                password: PASSWORD.into(),
            }))
            .await
            .unwrap();
    }

    async fn login(&self, password: &str) -> Result<pb::LoginResponse, Code> {
        self.auth
            .login(Request::new(pb::LoginRequest {
                username: "alice".into(),
                password: password.into(),
            }))
            .await
            .map(|r| r.into_inner())
            .map_err(|s| s.code())
    }

    /// Password, enrollment, and the first session. Returns the session token,
    /// the TOTP secret, and the recovery codes.
    async fn enroll(&self) -> (String, String, Vec<String>) {
        self.set_password().await;
        let login = self.login(PASSWORD).await.unwrap();
        assert_eq!(login.next_step, pb::LoginStep::EnrollTotp as i32);
        let begin = self
            .auth
            .begin_totp_enrollment(self.with_token(
                "BeginTotpEnrollment",
                &login.pending_token,
                pb::BeginTotpEnrollmentRequest {},
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(begin.otpauth_uri.contains(&begin.secret));
        let confirm = self
            .auth
            .confirm_totp_enrollment(self.with_token(
                "ConfirmTotpEnrollment",
                &login.pending_token,
                pb::ConfirmTotpEnrollmentRequest {
                    totp_code: code(&begin.secret, 0),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(confirm.recovery_codes.len(), secrets::RECOVERY_CODE_COUNT);
        let session = confirm.session.expect("first session");
        assert!(session.token.starts_with("kcs_"));
        assert_eq!(
            self.identity("/kcore.controller.Auth/WhoAmI", &login.pending_token),
            Err(Code::Unauthenticated),
            "pending token is spent"
        );
        (session.token, begin.secret, confirm.recovery_codes)
    }
}

#[tokio::test]
async fn first_login_enrolls_totp_and_signs_in() {
    let h = harness(5);
    let (session, _, _) = h.enroll().await;
    let me = h
        .auth
        .who_am_i(h.with_token("WhoAmI", &session, pb::WhoAmIRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(me.operator, "alice");
    assert_eq!(me.method, pb::AuthMethod::Session as i32);
    assert_eq!(me.effective_role, pb::OperatorRoleKind::VmAdmin as i32);
    assert!(me.totp_enabled && me.password_set);
}

#[tokio::test]
async fn wrong_password_and_unknown_user_look_the_same() {
    let h = harness(5);
    h.set_password().await;
    assert_eq!(
        h.login("wrong password!!").await.unwrap_err(),
        Code::Unauthenticated
    );
    let unknown = h
        .auth
        .login(Request::new(pb::LoginRequest {
            username: "mallory".into(),
            password: PASSWORD.into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), Code::Unauthenticated);
    assert_eq!(unknown.message(), "invalid username or password");
}

#[tokio::test]
async fn operator_without_password_cannot_sign_in() {
    let h = harness(5);
    assert_eq!(h.login(PASSWORD).await.unwrap_err(), Code::Unauthenticated);
}

#[tokio::test]
async fn totp_code_cannot_be_replayed_and_recovery_codes_are_single_use() {
    let h = harness(10);
    let (_, secret, recovery) = h.enroll().await;

    let login = h.login(PASSWORD).await.unwrap();
    assert_eq!(login.next_step, pb::LoginStep::Totp as i32);
    let replay = h
        .auth
        .verify_second_factor(h.with_token(
            "VerifySecondFactor",
            &login.pending_token,
            pb::VerifySecondFactorRequest {
                totp_code: code(&secret, 0),
                recovery_code: String::new(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(replay.message(), "code already used");

    let ok = h
        .auth
        .verify_second_factor(h.with_token(
            "VerifySecondFactor",
            &login.pending_token,
            pb::VerifySecondFactorRequest {
                totp_code: code(&secret, 1),
                recovery_code: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(ok.session.is_some());

    let login = h.login(PASSWORD).await.unwrap();
    let used = h
        .auth
        .verify_second_factor(h.with_token(
            "VerifySecondFactor",
            &login.pending_token,
            pb::VerifySecondFactorRequest {
                totp_code: String::new(),
                recovery_code: recovery[0].clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(used.recovery_codes_left, 9);

    let login = h.login(PASSWORD).await.unwrap();
    let again = h
        .auth
        .verify_second_factor(h.with_token(
            "VerifySecondFactor",
            &login.pending_token,
            pb::VerifySecondFactorRequest {
                totp_code: String::new(),
                recovery_code: recovery[0].clone(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(again.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn password_alone_cannot_replace_totp() {
    let h = harness(5);
    h.enroll().await;
    let login = h.login(PASSWORD).await.unwrap();
    let err = h
        .auth
        .begin_totp_enrollment(h.with_token(
            "BeginTotpEnrollment",
            &login.pending_token,
            pb::BeginTotpEnrollmentRequest {},
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert_eq!(
        h.identity("/kcore.controller.Controller/ListVms", &login.pending_token),
        Err(Code::PermissionDenied)
    );
}

#[tokio::test]
async fn repeated_failures_lock_the_operator() {
    let h = harness(3);
    h.set_password().await;
    for _ in 0..3 {
        assert_eq!(
            h.login("wrong password!!").await.unwrap_err(),
            Code::Unauthenticated
        );
    }
    let locked = h
        .auth
        .login(Request::new(pb::LoginRequest {
            username: "alice".into(),
            password: PASSWORD.into(),
        }))
        .await
        .unwrap_err();
    assert!(locked.message().contains("locked"), "{}", locked.message());
    h.set_password().await;
    assert!(
        h.login(PASSWORD).await.is_ok(),
        "an admin reset clears the lock"
    );
}

#[tokio::test]
async fn api_tokens_are_capped_and_cannot_mint_more_tokens() {
    let h = harness(5);
    let (session, _, _) = h.enroll().await;
    let above = h
        .auth
        .create_api_token(h.with_token(
            "CreateApiToken",
            &session,
            pb::CreateApiTokenRequest {
                name: "ci".into(),
                expires_in_days: 30,
                max_role: pb::OperatorRoleKind::ClusterAdmin as i32,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(above.code(), Code::PermissionDenied);

    let created = h
        .auth
        .create_api_token(h.with_token(
            "CreateApiToken",
            &session,
            pb::CreateApiTokenRequest {
                name: "ci".into(),
                expires_in_days: 30,
                max_role: pb::OperatorRoleKind::ReadOnly as i32,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(created.token.starts_with("kct_"));

    let me = h
        .auth
        .who_am_i(h.with_token("WhoAmI", &created.token, pb::WhoAmIRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(me.method, pb::AuthMethod::ApiToken as i32);
    assert_eq!(me.effective_role, pb::OperatorRoleKind::ReadOnly as i32);

    let nested = h
        .auth
        .create_api_token(h.with_token(
            "CreateApiToken",
            &created.token,
            pb::CreateApiTokenRequest {
                name: "nested".into(),
                expires_in_days: 1,
                max_role: 0,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(nested.code(), Code::PermissionDenied);

    let listed = h
        .auth
        .list_api_tokens(h.with_token(
            "ListApiTokens",
            &session,
            pb::ListApiTokensRequest::default(),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.tokens.len(), 1);
    let id = listed.tokens[0].token_id.clone();
    h.auth
        .revoke_api_token(h.with_token(
            "RevokeApiToken",
            &session,
            pb::RevokeApiTokenRequest { token_id: id },
        ))
        .await
        .unwrap();
    assert_eq!(
        h.identity("/kcore.controller.Controller/ListVms", &created.token),
        Err(Code::Unauthenticated)
    );
}

#[tokio::test]
async fn changing_the_password_ends_other_sessions() {
    let h = harness(10);
    let (first, secret, _) = h.enroll().await;
    let login = h.login(PASSWORD).await.unwrap();
    let second = h
        .auth
        .verify_second_factor(h.with_token(
            "VerifySecondFactor",
            &login.pending_token,
            pb::VerifySecondFactorRequest {
                totp_code: code(&secret, 1),
                recovery_code: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .session
        .unwrap()
        .token;
    let changed = h
        .auth
        .change_password(h.with_token(
            "ChangePassword",
            &second,
            pb::ChangePasswordRequest {
                current_password: PASSWORD.into(),
                new_password: "a brand new passphrase".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(changed.sessions_revoked, 1);
    assert_eq!(
        h.identity("/kcore.controller.Auth/WhoAmI", &first),
        Err(Code::Unauthenticated)
    );
    assert!(h.identity("/kcore.controller.Auth/WhoAmI", &second).is_ok());
    assert!(h.login("a brand new passphrase").await.is_ok());
}

#[tokio::test]
async fn admin_reset_needs_cluster_admin_and_forces_enrollment() {
    let h = harness(5);
    let (session, _, _) = h.enroll().await;
    let denied = h
        .auth
        .reset_operator_second_factor(h.with_token(
            "ResetOperatorSecondFactor",
            &session,
            pb::ResetOperatorSecondFactorRequest {
                name: "alice".into(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), Code::PermissionDenied);

    let reset = h
        .auth
        .reset_operator_second_factor(Request::new(pb::ResetOperatorSecondFactorRequest {
            name: "alice".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reset.sessions_revoked, 1);
    assert_eq!(
        h.login(PASSWORD).await.unwrap().next_step,
        pb::LoginStep::EnrollTotp as i32
    );
    let creds = h.db.get_operator_credentials("alice").unwrap().unwrap();
    assert!(!creds.totp_enabled && creds.recovery_codes.is_empty());
}

#[tokio::test]
async fn session_role_checks_match_the_operator_certificate() {
    let h = harness(5);
    let (session, _, _) = h.enroll().await;
    let req = h.with_token("WhoAmI", &session, ());
    assert!(crate::auth::require_controller_operator(
        &req,
        &h.db,
        true,
        false,
        OperatorRole::VmAdmin
    )
    .is_ok());
    assert_eq!(
        crate::auth::require_controller_operator(
            &req,
            &h.db,
            true,
            false,
            OperatorRole::ClusterAdmin
        )
        .unwrap_err()
        .code(),
        Code::PermissionDenied
    );
    assert_eq!(crate::auth::peer_cn(&req).as_deref(), Some("kctl:alice"));
}
