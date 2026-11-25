//! Cluster hardening extras stored beside the Kubernetes ops row: OIDC,
//! cert policy, encryption-at-rest keys, and default-deny label parsing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OidcSpec {
    pub issuer_url: String,
    pub client_id: String,
    pub username_claim: String,
    pub groups_claim: String,
    /// PEM CA for the issuer; empty means system roots / no --oidc-ca-file.
    pub ca_pem: String,
}

impl OidcSpec {
    pub fn enabled(&self) -> bool {
        !self.issuer_url.trim().is_empty()
    }

    pub fn from_json(json: &str) -> Self {
        if json.trim().is_empty() {
            return Self::default();
        }
        serde_json::from_str(json).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        if !self.enabled() {
            return String::new();
        }
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CertPolicy {
    #[serde(default = "default_leaf_days")]
    pub leaf_validity_days: i64,
    #[serde(default = "default_renew_before")]
    pub renew_before_days: i64,
}

fn default_leaf_days() -> i64 {
    365
}
fn default_renew_before() -> i64 {
    30
}

impl Default for CertPolicy {
    fn default() -> Self {
        Self {
            leaf_validity_days: default_leaf_days(),
            renew_before_days: default_renew_before(),
        }
    }
}

impl CertPolicy {
    pub fn from_json(json: &str) -> Self {
        if json.trim().is_empty() {
            return Self::default();
        }
        serde_json::from_str(json).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.leaf_validity_days < 1 {
            return Err("cert.leafValidityDays must be at least 1".into());
        }
        if self.renew_before_days < 0 {
            return Err("cert.renewBeforeDays must not be negative".into());
        }
        if self.renew_before_days >= self.leaf_validity_days {
            return Err("cert.renewBeforeDays must be less than leafValidityDays".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptionKeys {
    pub current_id: String,
    pub current_secret_b64: String,
    #[serde(default)]
    pub previous_id: String,
    #[serde(default)]
    pub previous_secret_b64: String,
}

impl EncryptionKeys {
    pub fn from_json(json: &str) -> Option<Self> {
        if json.trim().is_empty() {
            return None;
        }
        serde_json::from_str(json).ok()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn new_random() -> Self {
        use aws_lc_rs::rand::{SecureRandom, SystemRandom};
        let mut buf = [0u8; 32];
        SystemRandom::new().fill(&mut buf).expect("system random");
        Self {
            current_id: format!("key-{}", uuid::Uuid::new_v4().simple()),
            current_secret_b64: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                buf,
            ),
            previous_id: String::new(),
            previous_secret_b64: String::new(),
        }
    }

    pub fn rotate(&self) -> Self {
        let mut next = Self::new_random();
        next.previous_id = self.current_id.clone();
        next.previous_secret_b64 = self.current_secret_b64.clone();
        next
    }

    pub fn provider_config_yaml(&self) -> String {
        let mut keys = format!(
            r#"            - name: {}
              secret: {}
"#,
            self.current_id, self.current_secret_b64
        );
        if !self.previous_id.is_empty() && !self.previous_secret_b64.is_empty() {
            keys.push_str(&format!(
                r#"            - name: {}
              secret: {}
"#,
                self.previous_id, self.previous_secret_b64
            ));
        }
        format!(
            r#"apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources:
      - secrets
    providers:
      - secretbox:
          keys:
{keys}      - identity: {{}}
"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_policy_defaults_and_validate() {
        let p = CertPolicy::default();
        assert!(p.validate().is_ok());
        assert!(CertPolicy {
            leaf_validity_days: 30,
            renew_before_days: 30,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn encryption_rotate_keeps_previous() {
        let first = EncryptionKeys::new_random();
        let second = first.rotate();
        assert_eq!(second.previous_id, first.current_id);
        assert!(second.provider_config_yaml().contains(&first.current_id));
        assert!(second.provider_config_yaml().contains(&second.current_id));
    }
}
