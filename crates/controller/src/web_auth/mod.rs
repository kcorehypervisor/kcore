//! Sign-in for the web listener: passwords, TOTP, bearer tokens, and the
//! gRPC-web server that checks them.

pub mod listener;
pub mod secrets;
mod service;

pub use service::AuthService;

use serde_json::Value;

use crate::auth::OperatorRole;
use crate::controller_proto;
use crate::db::{AuthTokenRow, Database, OperatorCredentials};

pub const EVT_CREDENTIALS_SET: &str = "operator_credentials.set";
pub const EVT_API_TOKEN_CREATE: &str = "auth_token.create";
pub const EVT_API_TOKEN_REVOKE: &str = "auth_token.revoke";

pub fn role_to_proto(role: OperatorRole) -> controller_proto::OperatorRoleKind {
    use controller_proto::OperatorRoleKind as K;
    match role {
        OperatorRole::ReadOnly => K::ReadOnly,
        OperatorRole::BackupOperator => K::BackupOperator,
        OperatorRole::VmAdmin => K::VmAdmin,
        OperatorRole::ClusterAdmin => K::ClusterAdmin,
    }
}

pub fn role_from_proto(kind: i32) -> Option<OperatorRole> {
    use controller_proto::OperatorRoleKind as K;
    match K::try_from(kind).ok()? {
        K::Unspecified => None,
        K::ReadOnly => Some(OperatorRole::ReadOnly),
        K::BackupOperator => Some(OperatorRole::BackupOperator),
        K::VmAdmin => Some(OperatorRole::VmAdmin),
        K::ClusterAdmin => Some(OperatorRole::ClusterAdmin),
    }
}

pub fn credentials_body(creds: &OperatorCredentials, revoke_sessions: bool) -> Value {
    serde_json::json!({
        "name": creds.operator_name,
        "passwordHash": creds.password_hash,
        "totpSecret": creds.totp_secret,
        "totpEnabled": creds.totp_enabled,
        "recoveryCodes": creds.recovery_codes,
        "updatedAt": creds.updated_at,
        "revokeSessions": revoke_sessions,
    })
}

/// Apply `operator_credentials.set` from a peer controller. A password or
/// TOTP reset elsewhere also ends this controller's sessions for the operator.
pub fn apply_replicated_credentials(db: &Database, body: &Value) -> Result<(), String> {
    let text = |key: &str| body.get(key).and_then(Value::as_str).unwrap_or_default();
    let name = text("name");
    if name.is_empty() {
        return Err("missing name".into());
    }
    let creds = OperatorCredentials {
        operator_name: name.to_string(),
        password_hash: text("passwordHash").to_string(),
        totp_secret: text("totpSecret").to_string(),
        totp_enabled: body
            .get("totpEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        recovery_codes: body
            .get("recoveryCodes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        updated_at: body.get("updatedAt").and_then(Value::as_i64).unwrap_or(0),
    };
    if db
        .get_operator_row(name)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err(format!("operator {name} does not exist yet"));
    }
    let applied = db
        .upsert_operator_credentials(&creds)
        .map_err(|e| format!("upsert credentials {name}: {e}"))?;
    let revoke = body
        .get("revokeSessions")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if applied && revoke {
        db.delete_operator_tokens(name, "session", None)
            .map_err(|e| format!("revoke sessions {name}: {e}"))?;
        db.delete_operator_tokens(name, "pending", None)
            .map_err(|e| format!("revoke pending {name}: {e}"))?;
    }
    Ok(())
}

pub fn token_body(row: &AuthTokenRow) -> Value {
    serde_json::json!({
        "id": row.id,
        "tokenHash": row.token_hash,
        "operator": row.operator_name,
        "name": row.name,
        "maxRole": row.max_role,
        "createdAt": row.created_at,
        "expiresAt": row.expires_at,
        "createdFrom": row.created_from,
    })
}

/// Apply `auth_token.create` from a peer. Only API tokens replicate.
pub fn apply_replicated_token(db: &Database, body: &Value) -> Result<(), String> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let row = AuthTokenRow {
        id: text("id"),
        token_hash: text("tokenHash"),
        operator_name: text("operator"),
        kind: "api".into(),
        name: text("name"),
        max_role: text("maxRole"),
        created_at: body.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
        expires_at: body.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
        last_used_at: 0,
        created_from: text("createdFrom"),
    };
    if row.id.is_empty() || row.token_hash.len() != 64 || row.operator_name.is_empty() {
        return Err("token event needs id, a 64-character tokenHash, and operator".into());
    }
    db.insert_auth_token(&row)
        .map_err(|e| format!("insert auth token {}: {e}", row.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicated_credentials_round_trip_and_revoke_sessions() {
        let db = Database::open(":memory:").unwrap();
        db.create_operator("alice").unwrap();
        db.insert_auth_token(&AuthTokenRow {
            id: "s1".into(),
            token_hash: "h".into(),
            operator_name: "alice".into(),
            kind: "session".into(),
            name: String::new(),
            max_role: String::new(),
            created_at: 1,
            expires_at: i64::MAX,
            last_used_at: 0,
            created_from: String::new(),
        })
        .unwrap();
        let creds = OperatorCredentials {
            operator_name: "alice".into(),
            password_hash: "p".into(),
            totp_secret: "S".into(),
            totp_enabled: true,
            recovery_codes: vec!["c".into()],
            updated_at: 5,
        };
        apply_replicated_credentials(&db, &credentials_body(&creds, true)).unwrap();
        assert_eq!(
            db.get_operator_credentials("alice").unwrap().unwrap(),
            creds
        );
        assert!(db.get_auth_token("s1").unwrap().is_none());
    }

    #[test]
    fn replicated_credentials_need_the_operator() {
        let db = Database::open(":memory:").unwrap();
        let creds = OperatorCredentials {
            operator_name: "ghost".into(),
            ..Default::default()
        };
        assert!(apply_replicated_credentials(&db, &credentials_body(&creds, false)).is_err());
    }

    #[test]
    fn replicated_api_token_is_inserted_once() {
        let db = Database::open(":memory:").unwrap();
        db.create_operator("alice").unwrap();
        let row = AuthTokenRow {
            id: "t1".into(),
            token_hash: "a".repeat(64),
            operator_name: "alice".into(),
            kind: "api".into(),
            name: "ci".into(),
            max_role: "read-only".into(),
            created_at: 1,
            expires_at: 99,
            last_used_at: 0,
            created_from: String::new(),
        };
        let body = token_body(&row);
        apply_replicated_token(&db, &body).unwrap();
        apply_replicated_token(&db, &body).unwrap();
        assert_eq!(db.get_auth_token("t1").unwrap().unwrap(), row);
        assert!(apply_replicated_token(&db, &serde_json::json!({"id": "x"})).is_err());
    }
}
