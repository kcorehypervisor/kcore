//! Operator credentials, login throttling, and bearer tokens for the web listener.

use rusqlite::{params, OptionalExtension};

use super::Database;

/// Replicated sign-in material for one operator. Only hashes and the TOTP
/// seed are stored; the seed has to be readable to check a code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatorCredentials {
    pub operator_name: String,
    pub password_hash: String,
    pub totp_secret: String,
    pub totp_enabled: bool,
    /// SHA-256 hex of each unused recovery code.
    pub recovery_codes: Vec<String>,
    pub updated_at: i64,
}

/// Per-controller state that is never replicated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginState {
    pub failed_attempts: i64,
    pub locked_until: i64,
    pub last_totp_step: i64,
    pub pending_totp_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthTokenRow {
    pub id: String,
    pub token_hash: String,
    pub operator_name: String,
    /// `pending`, `session`, or `api`.
    pub kind: String,
    pub name: String,
    pub max_role: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub last_used_at: i64,
    pub created_from: String,
}

const TOKEN_COLUMNS: &str = "id, token_hash, operator_name, kind, name, max_role, created_at, expires_at, last_used_at, created_from";

fn token_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuthTokenRow> {
    Ok(AuthTokenRow {
        id: row.get(0)?,
        token_hash: row.get(1)?,
        operator_name: row.get(2)?,
        kind: row.get(3)?,
        name: row.get(4)?,
        max_role: row.get(5)?,
        created_at: row.get(6)?,
        expires_at: row.get(7)?,
        last_used_at: row.get(8)?,
        created_from: row.get(9)?,
    })
}

impl Database {
    pub fn get_operator_credentials(
        &self,
        name: &str,
    ) -> Result<Option<OperatorCredentials>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT operator_name, password_hash, totp_secret, totp_enabled, recovery_codes, updated_at
             FROM operator_credentials WHERE operator_name = ?1",
            params![name],
            |row| {
                let codes: String = row.get(4)?;
                Ok(OperatorCredentials {
                    operator_name: row.get(0)?,
                    password_hash: row.get(1)?,
                    totp_secret: row.get(2)?,
                    totp_enabled: row.get::<_, i64>(3)? != 0,
                    recovery_codes: serde_json::from_str(&codes).unwrap_or_default(),
                    updated_at: row.get(5)?,
                })
            },
        )
        .optional()
    }

    /// Last writer wins on `updated_at`, so a replayed older event cannot
    /// bring back a password or TOTP seed that was replaced.
    pub fn upsert_operator_credentials(
        &self,
        creds: &OperatorCredentials,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let codes = serde_json::to_string(&creds.recovery_codes).unwrap_or_else(|_| "[]".into());
        let n = conn.execute(
            "INSERT INTO operator_credentials
                (operator_name, password_hash, totp_secret, totp_enabled, recovery_codes, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(operator_name) DO UPDATE SET
                password_hash = excluded.password_hash,
                totp_secret = excluded.totp_secret,
                totp_enabled = excluded.totp_enabled,
                recovery_codes = excluded.recovery_codes,
                updated_at = excluded.updated_at
             WHERE excluded.updated_at >= operator_credentials.updated_at",
            params![
                creds.operator_name,
                creds.password_hash,
                creds.totp_secret,
                i64::from(creds.totp_enabled),
                codes,
                creds.updated_at
            ],
        )?;
        Ok(n > 0)
    }

    pub fn get_login_state(&self, name: &str) -> Result<LoginState, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn
            .query_row(
                "SELECT failed_attempts, locked_until, last_totp_step, pending_totp_secret
                 FROM operator_login_state WHERE operator_name = ?1",
                params![name],
                |row| {
                    Ok(LoginState {
                        failed_attempts: row.get(0)?,
                        locked_until: row.get(1)?,
                        last_totp_step: row.get(2)?,
                        pending_totp_secret: row.get(3)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Count one failed attempt. At `max_attempts` the operator is locked
    /// until `now + lock_secs` and the counter starts again.
    pub fn record_login_failure(
        &self,
        name: &str,
        now: i64,
        max_attempts: i64,
        lock_secs: i64,
    ) -> Result<LoginState, rusqlite::Error> {
        {
            let conn = self.lock_conn()?;
            conn.execute(
                "INSERT INTO operator_login_state (operator_name, failed_attempts) VALUES (?1, 1)
                 ON CONFLICT(operator_name) DO UPDATE SET failed_attempts = failed_attempts + 1",
                params![name],
            )?;
            conn.execute(
                "UPDATE operator_login_state SET failed_attempts = 0, locked_until = ?2
                 WHERE operator_name = ?1 AND failed_attempts >= ?3",
                params![name, now + lock_secs, max_attempts],
            )?;
        }
        self.get_login_state(name)
    }

    pub fn clear_login_failures(&self, name: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE operator_login_state SET failed_attempts = 0, locked_until = 0
             WHERE operator_name = ?1",
            params![name],
        )?;
        Ok(())
    }

    /// Record the TOTP time step that was just accepted. Returns false when
    /// that step or a later one was already used, which blocks a replay.
    pub fn claim_totp_step(&self, name: &str, step: i64) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO operator_login_state (operator_name) VALUES (?1)",
            params![name],
        )?;
        let n = conn.execute(
            "UPDATE operator_login_state SET last_totp_step = ?2
             WHERE operator_name = ?1 AND last_totp_step < ?2",
            params![name, step],
        )?;
        Ok(n > 0)
    }

    pub fn set_pending_totp_secret(&self, name: &str, secret: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO operator_login_state (operator_name, pending_totp_secret) VALUES (?1, ?2)
             ON CONFLICT(operator_name) DO UPDATE SET pending_totp_secret = excluded.pending_totp_secret",
            params![name, secret],
        )?;
        Ok(())
    }

    /// Insert a token. An existing id is left alone so a replayed
    /// replication event is harmless.
    pub fn insert_auth_token(&self, row: &AuthTokenRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            &format!(
                "INSERT INTO auth_tokens ({TOKEN_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id) DO NOTHING"
            ),
            params![
                row.id,
                row.token_hash,
                row.operator_name,
                row.kind,
                row.name,
                row.max_role,
                row.created_at,
                row.expires_at,
                row.last_used_at,
                row.created_from
            ],
        )?;
        Ok(())
    }

    pub fn find_auth_token_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<AuthTokenRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            &format!("SELECT {TOKEN_COLUMNS} FROM auth_tokens WHERE token_hash = ?1"),
            params![token_hash],
            token_row,
        )
        .optional()
    }

    pub fn get_auth_token(&self, id: &str) -> Result<Option<AuthTokenRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            &format!("SELECT {TOKEN_COLUMNS} FROM auth_tokens WHERE id = ?1"),
            params![id],
            token_row,
        )
        .optional()
    }

    pub fn touch_auth_token(&self, id: &str, now: i64) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE auth_tokens SET last_used_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }

    pub fn delete_auth_token(&self, id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let n = conn.execute("DELETE FROM auth_tokens WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Delete an operator's tokens of `kind`, keeping `keep_id` when given.
    pub fn delete_operator_tokens(
        &self,
        name: &str,
        kind: &str,
        keep_id: Option<&str>,
    ) -> Result<usize, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM auth_tokens WHERE operator_name = ?1 AND kind = ?2 AND id != ?3",
            params![name, kind, keep_id.unwrap_or("")],
        )
    }

    pub fn list_auth_tokens(
        &self,
        name: &str,
        kind: &str,
    ) -> Result<Vec<AuthTokenRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {TOKEN_COLUMNS} FROM auth_tokens
             WHERE operator_name = ?1 AND kind = ?2 ORDER BY created_at DESC, id"
        ))?;
        let rows = stmt.query_map(params![name, kind], token_row)?;
        rows.collect()
    }

    pub fn delete_expired_auth_tokens(&self, now: i64) -> Result<usize, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM auth_tokens WHERE expires_at <= ?1",
            params![now],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with(name: &str) -> Database {
        let db = Database::open(":memory:").unwrap();
        db.create_operator(name).unwrap();
        db
    }

    fn token(id: &str, operator: &str, kind: &str) -> AuthTokenRow {
        AuthTokenRow {
            id: id.into(),
            token_hash: format!("hash-{id}"),
            operator_name: operator.into(),
            kind: kind.into(),
            name: String::new(),
            max_role: String::new(),
            created_at: 10,
            expires_at: 100,
            last_used_at: 0,
            created_from: String::new(),
        }
    }

    #[test]
    fn credentials_keep_the_newest_write() {
        let db = db_with("alice");
        let mut creds = OperatorCredentials {
            operator_name: "alice".into(),
            password_hash: "new".into(),
            updated_at: 20,
            ..Default::default()
        };
        assert!(db.upsert_operator_credentials(&creds).unwrap());
        creds.password_hash = "old".into();
        creds.updated_at = 10;
        assert!(!db.upsert_operator_credentials(&creds).unwrap());
        let stored = db.get_operator_credentials("alice").unwrap().unwrap();
        assert_eq!(stored.password_hash, "new");
    }

    #[test]
    fn failures_lock_then_reset() {
        let db = db_with("alice");
        for _ in 0..4 {
            let state = db.record_login_failure("alice", 1000, 5, 900).unwrap();
            assert_eq!(state.locked_until, 0);
        }
        let state = db.record_login_failure("alice", 1000, 5, 900).unwrap();
        assert_eq!(state.locked_until, 1900);
        assert_eq!(state.failed_attempts, 0);
        db.clear_login_failures("alice").unwrap();
        assert_eq!(db.get_login_state("alice").unwrap().locked_until, 0);
    }

    #[test]
    fn totp_step_is_single_use() {
        let db = db_with("alice");
        assert!(db.claim_totp_step("alice", 50).unwrap());
        assert!(!db.claim_totp_step("alice", 50).unwrap());
        assert!(!db.claim_totp_step("alice", 49).unwrap());
        assert!(db.claim_totp_step("alice", 51).unwrap());
    }

    #[test]
    fn tokens_follow_the_operator() {
        let db = db_with("alice");
        db.insert_auth_token(&token("s1", "alice", "session"))
            .unwrap();
        db.insert_auth_token(&token("s2", "alice", "session"))
            .unwrap();
        db.insert_auth_token(&token("a1", "alice", "api")).unwrap();
        assert_eq!(
            db.delete_operator_tokens("alice", "session", Some("s1"))
                .unwrap(),
            1
        );
        assert!(db.find_auth_token_by_hash("hash-s1").unwrap().is_some());
        db.delete_operator("alice").unwrap();
        assert!(db.get_auth_token("a1").unwrap().is_none());
        assert!(db.get_operator_credentials("alice").unwrap().is_none());
    }

    #[test]
    fn expired_tokens_are_pruned() {
        let db = db_with("alice");
        db.insert_auth_token(&token("s1", "alice", "session"))
            .unwrap();
        assert_eq!(db.delete_expired_auth_tokens(99).unwrap(), 0);
        assert_eq!(db.delete_expired_auth_tokens(100).unwrap(), 1);
    }

    #[test]
    fn schema_66_rolls_back() {
        let db = db_with("alice");
        db.insert_auth_token(&token("s1", "alice", "session"))
            .unwrap();
        db.migrate_down(65).unwrap();
        assert!(db.get_auth_token("s1").is_err());
        db.migrate().unwrap();
        assert!(db.get_auth_token("s1").unwrap().is_none());
    }
}
