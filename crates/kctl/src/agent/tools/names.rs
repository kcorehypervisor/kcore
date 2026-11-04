//! Input checks for values that end up in controller requests, file paths and cloud-init.

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

pub type Spec = Map<String, Value>;

/// A problem with what the operator or the model asked for. Shown to them as is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputError(pub String);

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

pub fn input_error<T>(message: impl Into<String>) -> Result<T, InputError> {
    Err(InputError(message.into()))
}

static NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$").unwrap());
static ADDRESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._:-]{1,253}$").unwrap());
static SHA256: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{64}$").unwrap());
static DISK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/dev/[A-Za-z0-9/_-]{1,120}$").unwrap());

fn reject_control_chars(value: &str, label: &str) -> Result<(), InputError> {
    if value.contains('\0') || value.contains('\r') || value.contains('\n') {
        return input_error(format!("{label} contains a newline or null byte"));
    }
    Ok(())
}

pub fn assert_name(value: &str, label: &str) -> Result<String, InputError> {
    let trimmed = value.trim();
    reject_control_chars(trimmed, label)?;
    if !NAME.is_match(trimmed) {
        return input_error(format!(
            "{label} must start with a letter or digit and contain only letters, digits, '.', '_' or '-'"
        ));
    }
    Ok(trimmed.to_string())
}

pub fn assert_address(value: &str, label: &str) -> Result<String, InputError> {
    let trimmed = value.trim();
    reject_control_chars(trimmed, label)?;
    if trimmed.starts_with('-') || !ADDRESS.is_match(trimmed) || !trimmed.contains(':') {
        return input_error(format!("{label} must look like host:port"));
    }
    Ok(trimmed.to_string())
}

pub fn assert_https_url(value: &str) -> Result<String, InputError> {
    let trimmed = value.trim();
    reject_control_chars(trimmed, "image URL")?;
    let Ok(url) = url::Url::parse(trimmed) else {
        return input_error("image URL must be an https:// address");
    };
    if url.scheme() != "https" {
        return input_error("image URL must use https://");
    }
    Ok(trimmed.to_string())
}

pub fn assert_sha256(value: &str) -> Result<String, InputError> {
    let trimmed = value.trim();
    if !SHA256.is_match(trimmed) {
        return input_error("image SHA256 must be 64 hexadecimal characters");
    }
    Ok(trimmed.to_lowercase())
}

pub fn assert_disk(value: &str, label: &str) -> Result<String, InputError> {
    let trimmed = value.trim();
    reject_control_chars(trimmed, label)?;
    if !DISK.is_match(trimmed) {
        return input_error(format!("{label} must be a /dev/ path"));
    }
    Ok(trimmed.to_string())
}

pub fn assert_ssh_public_key(value: &str) -> Result<String, InputError> {
    let trimmed = value.trim();
    reject_control_chars(trimmed, "SSH public key")?;
    if !trimmed.starts_with("ssh-") {
        return input_error("SSH public key must start with ssh-");
    }
    if trimmed.len() > 8192 {
        return input_error("SSH public key is too long");
    }
    Ok(trimmed.to_string())
}

pub fn assert_arg(value: &str, label: &str) -> Result<String, InputError> {
    let trimmed = value.trim();
    reject_control_chars(trimmed, label)?;
    if trimmed.starts_with('-') || trimmed.is_empty() || trimmed.len() > 4096 {
        return input_error(format!("{label} is empty or looks like a flag"));
    }
    Ok(trimmed.to_string())
}

pub fn is_blank(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.trim().is_empty(),
        Some(Value::Array(items)) => items.is_empty(),
        _ => false,
    }
}

/// A spec value as trimmed text. Numbers are accepted because models send `"cpu": 2`.
pub fn opt(spec: &Spec, key: &str) -> Option<String> {
    match spec.get(key)? {
        Value::Number(number) => Some(number_text(number)),
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        _ => None,
    }
}

pub fn number_text(number: &serde_json::Number) -> String {
    if let Some(float) = number.as_f64() {
        if number.is_f64() && float.fract() == 0.0 && float.abs() < 1e15 {
            return format!("{}", float as i64);
        }
    }
    number.to_string()
}

pub fn required(spec: &Spec, key: &str, label: &str) -> Result<String, InputError> {
    match opt(spec, key) {
        Some(value) => Ok(value),
        None => input_error(format!("{label} is required")),
    }
}

/// `true`, `"true"` and `"yes"` are true. Anything else is false.
pub fn boolish(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text == "true" || text == "yes",
        _ => false,
    }
}

pub fn string_list(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn str_of(spec: &Spec, key: &str) -> String {
    spec.get(key)
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_follow_the_controller_rules() {
        assert_eq!(assert_name(" web-01 ", "name").unwrap(), "web-01");
        assert!(assert_name("-web", "name").is_err());
        assert!(assert_name("web\n01", "name").is_err());
        assert!(assert_name("", "name").is_err());
        assert!(assert_name(&"a".repeat(64), "name").is_err());
    }

    #[test]
    fn addresses_need_a_port_and_cannot_look_like_flags() {
        assert!(assert_address("10.0.0.1:9090", "controller").is_ok());
        assert!(assert_address("10.0.0.1", "controller").is_err());
        assert!(assert_address("--help:1", "controller").is_err());
    }

    #[test]
    fn image_urls_must_be_https_and_checksums_64_hex() {
        assert!(assert_https_url("https://example.org/a.qcow2").is_ok());
        assert!(assert_https_url("http://example.org/a.qcow2").is_err());
        assert!(assert_https_url("not a url").is_err());
        assert_eq!(assert_sha256(&"AB".repeat(32)).unwrap(), "ab".repeat(32));
        assert!(assert_sha256("abc").is_err());
    }

    #[test]
    fn disks_must_be_device_paths() {
        assert!(assert_disk("/dev/sda", "disk").is_ok());
        assert!(assert_disk("sda", "disk").is_err());
        assert!(assert_disk("/dev/sda\n/dev/sdb", "disk").is_err());
    }

    #[test]
    fn blank_and_numeric_spec_values() {
        let spec = json!({"cpu": 2, "name": "  ", "x": 2.0})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(opt(&spec, "cpu").as_deref(), Some("2"));
        assert_eq!(opt(&spec, "x").as_deref(), Some("2"));
        assert_eq!(opt(&spec, "name"), None);
        assert!(is_blank(spec.get("name")));
        assert!(is_blank(spec.get("missing")));
        assert!(!is_blank(spec.get("cpu")));
    }

    #[test]
    fn boolish_accepts_yes_and_true_only() {
        assert!(boolish(Some(&json!(true))));
        assert!(boolish(Some(&json!("yes"))));
        assert!(boolish(Some(&json!("true"))));
        assert!(!boolish(Some(&json!("no"))));
        assert!(!boolish(None));
    }
}
