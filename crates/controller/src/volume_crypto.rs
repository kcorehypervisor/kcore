//! E2 per-volume encryption: wrap volume DEKs with a cluster master key.
//!
//! Master key path: `/etc/kcore/recovery/volume-master.key` (32 random bytes).
//! Wrapped DEK format (base64): `v1 || nonce(12) || ciphertext+tag`.

use std::path::{Path, PathBuf};

use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use base64::{engine::general_purpose::STANDARD as B64, Engine};

pub const DEFAULT_MASTER_KEY_PATH: &str = "/etc/kcore/recovery/volume-master.key";
const DEK_LEN: usize = 32;
const VERSION: u8 = 1;

#[derive(Debug, Clone)]
pub struct MasterKeyStore {
    path: PathBuf,
}

impl Default for MasterKeyStore {
    fn default() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_MASTER_KEY_PATH),
        }
    }
}

impl MasterKeyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load or create the 32-byte master key.
    pub fn load_or_create(&self) -> Result<[u8; DEK_LEN], String> {
        if self.path.is_file() {
            let bytes = std::fs::read(&self.path).map_err(|e| format!("read master key: {e}"))?;
            if bytes.len() != DEK_LEN {
                return Err(format!(
                    "master key at {} must be {DEK_LEN} bytes, got {}",
                    self.path.display(),
                    bytes.len()
                ));
            }
            let mut key = [0u8; DEK_LEN];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let mut key = [0u8; DEK_LEN];
        SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| "rng failed".to_string())?;
        std::fs::write(&self.path, key).map_err(|e| format!("write master key: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(key)
    }
}

pub fn generate_dek() -> Result<Vec<u8>, String> {
    let mut dek = vec![0u8; DEK_LEN];
    SystemRandom::new()
        .fill(&mut dek)
        .map_err(|_| "rng failed".to_string())?;
    Ok(dek)
}

pub fn wrap_dek(master: &[u8; DEK_LEN], dek: &[u8]) -> Result<String, String> {
    if dek.len() != DEK_LEN {
        return Err(format!("dek must be {DEK_LEN} bytes"));
    }
    let unbound =
        UnboundKey::new(&AES_256_GCM, master).map_err(|_| "bad master key".to_string())?;
    let key = LessSafeKey::new(unbound);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| "rng failed".to_string())?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = dek.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| "seal failed".to_string())?;
    let mut packed = Vec::with_capacity(1 + NONCE_LEN + in_out.len());
    packed.push(VERSION);
    packed.extend_from_slice(&nonce_bytes);
    packed.extend_from_slice(&in_out);
    Ok(B64.encode(packed))
}

pub fn unwrap_dek(master: &[u8; DEK_LEN], wrapped_b64: &str) -> Result<Vec<u8>, String> {
    let packed = B64
        .decode(wrapped_b64.trim())
        .map_err(|e| format!("b64: {e}"))?;
    if packed.len() < 1 + NONCE_LEN + 16 {
        return Err("wrapped dek too short".into());
    }
    if packed[0] != VERSION {
        return Err(format!("unsupported wrap version {}", packed[0]));
    }
    let nonce_bytes: [u8; NONCE_LEN] = packed[1..1 + NONCE_LEN]
        .try_into()
        .map_err(|_| "nonce".to_string())?;
    let mut body = packed[1 + NONCE_LEN..].to_vec();
    let unbound =
        UnboundKey::new(&AES_256_GCM, master).map_err(|_| "bad master key".to_string())?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut body)
        .map_err(|_| "unwrap failed".to_string())?;
    if plain.len() != DEK_LEN {
        return Err("unwrapped dek wrong length".into());
    }
    Ok(plain.to_vec())
}

pub fn mapper_name_for_serial(serial: &str) -> String {
    let s: String = serial
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(48)
        .collect();
    format!("kcore-vol-{s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = MasterKeyStore::new(dir.path().join("volume-master.key"));
        let master = store.load_or_create().unwrap();
        let dek = generate_dek().unwrap();
        let wrapped = wrap_dek(&master, &dek).unwrap();
        let out = unwrap_dek(&master, &wrapped).unwrap();
        assert_eq!(dek, out);
        // reload master from disk
        let master2 = store.load_or_create().unwrap();
        assert_eq!(master, master2);
        assert_eq!(unwrap_dek(&master2, &wrapped).unwrap(), dek);
    }

    #[test]
    fn wrong_master_fails() {
        let m1 = [1u8; 32];
        let m2 = [2u8; 32];
        let dek = generate_dek().unwrap();
        let wrapped = wrap_dek(&m1, &dek).unwrap();
        assert!(unwrap_dek(&m2, &wrapped).is_err());
    }

    #[test]
    fn mapper_name_stable() {
        assert_eq!(mapper_name_for_serial("abc-12"), "kcore-vol-abc-12");
    }
}
