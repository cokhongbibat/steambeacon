//! AES-256-GCM / AES-256-CBC decrypt helpers.
//!
//! Wire format (matches discord-natri-bot::crypto_db):
//!   GCM (v2): `v2:<nonce_hex>:<ciphertext_hex>:<tag_hex>`
//!   CBC (legacy): `<iv_hex>:<ciphertext_hex>`
//!
//! Key comes from `SS_PRIVATE_KEY_DB`, falling back to `PRIVATE_KEY_DB`
//! (32 bytes = 64 hex chars). Validated once at startup and cached for the
//! lifetime of the process.

use std::sync::OnceLock;

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use cbc::{
    cipher::{BlockDecryptMut, KeyIvInit},
    Decryptor,
};

use crate::config::require_env_first;

type Aes256CbcDec = Decryptor<aes::Aes256>;

static DB_KEY: OnceLock<[u8; 32]> = OnceLock::new();

pub fn init_key() -> anyhow::Result<()> {
    let (var_name, hex_key) = require_env_first(&["SS_PRIVATE_KEY_DB", "PRIVATE_KEY_DB"])?;
    if hex_key.len() != 64 {
        anyhow::bail!(
            "{var_name} must be exactly 64 hex chars (32 bytes), got {} chars",
            hex_key.len()
        );
    }
    let bytes =
        hex::decode(&hex_key).map_err(|e| anyhow::anyhow!("{var_name} is not valid hex: {e}"))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{var_name} must be exactly 32 bytes"))?;
    let _ = DB_KEY.set(key);
    Ok(())
}

fn get_key() -> anyhow::Result<&'static [u8; 32]> {
    DB_KEY
        .get()
        .ok_or_else(|| anyhow::anyhow!("crypto key not initialised — call init_key() first"))
}

/// Decrypt a sealed value produced by `discord-natri-bot::crypto_db::encrypt_data`.
/// Uses the process-global key set by [`init_key`].
pub fn decrypt_data(encrypted: &str) -> anyhow::Result<String> {
    let key = get_key()?;
    decrypt_with(encrypted, key)
}

/// Decrypt a sealed value with an explicitly-provided 32-byte key. Used by tests
/// (and any future caller that doesn't want to depend on the global `OnceLock`).
fn decrypt_with(encrypted: &str, key: &[u8; 32]) -> anyhow::Result<String> {
    if let Some(gcm_part) = encrypted.strip_prefix("v2:") {
        decrypt_gcm(gcm_part, key)
    } else {
        decrypt_cbc(encrypted, key)
    }
}

/// Returns `true` when `value` looks like a sealed payload from the shared
/// encryption scheme. Matches both:
///   - GCM v2:  `"v2:" + nonce_hex + ":" + ciphertext_hex + ":" + tag_hex`
///   - CBC legacy: `iv_hex + ":" + ciphertext_hex`
///
/// Steam session cookies contain `=`, `;`, `/`, etc. and never match the
/// hex-only pattern, so false-positives on plaintext values don't happen.
#[allow(dead_code)]
pub fn is_encrypted(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if let Some(inner) = value.strip_prefix("v2:") {
        let parts: Vec<&str> = inner.splitn(3, ':').collect();
        return parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_hexdigit()));
    }
    let parts: Vec<&str> = value.splitn(2, ':').collect();
    parts.len() == 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn decrypt_gcm(gcm_data: &str, raw_key: &[u8; 32]) -> anyhow::Result<String> {
    let parts: Vec<&str> = gcm_data.splitn(3, ':').collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid GCM data format: expected v2:nonce:ciphertext:tag");
    }

    let key = Key::<Aes256Gcm>::from_slice(raw_key);
    let cipher = Aes256Gcm::new(key);

    let nonce_bytes =
        hex::decode(parts[0]).map_err(|e| anyhow::anyhow!("GCM nonce decode failed: {e}"))?;
    let ciphertext =
        hex::decode(parts[1]).map_err(|e| anyhow::anyhow!("GCM ciphertext decode failed: {e}"))?;
    let tag = hex::decode(parts[2]).map_err(|e| anyhow::anyhow!("GCM tag decode failed: {e}"))?;

    let nonce = Nonce::from_slice(&nonce_bytes);

    // aes-gcm expects tag appended to ciphertext
    let mut payload = ciphertext;
    payload.extend_from_slice(&tag);

    let plaintext = cipher
        .decrypt(nonce, payload.as_ref())
        .map_err(|e| anyhow::anyhow!("GCM decrypt failed: {e}"))?;

    String::from_utf8(plaintext).map_err(|e| anyhow::anyhow!("GCM UTF-8 decode failed: {e}"))
}

fn decrypt_cbc(encrypted: &str, raw_key: &[u8; 32]) -> anyhow::Result<String> {
    let parts: Vec<&str> = encrypted.splitn(2, ':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid CBC data format: expected iv_hex:ciphertext_hex");
    }

    let iv_bytes =
        hex::decode(parts[0]).map_err(|e| anyhow::anyhow!("CBC IV decode failed: {e}"))?;
    let mut ciphertext =
        hex::decode(parts[1]).map_err(|e| anyhow::anyhow!("CBC ciphertext decode failed: {e}"))?;

    let decryptor = Aes256CbcDec::new_from_slices(raw_key, &iv_bytes)
        .map_err(|e| anyhow::anyhow!("CBC init failed: {e}"))?;

    let plaintext = decryptor
        .decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut ciphertext)
        .map_err(|e| anyhow::anyhow!("CBC decrypt failed: {e}"))?;

    String::from_utf8(plaintext.to_vec())
        .map_err(|e| anyhow::anyhow!("CBC UTF-8 decode failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decrypt_gcm_roundtrip() {
        let key_hex = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
        let key_arr: [u8; 32] = hex::decode(key_hex).unwrap().try_into().unwrap();

        let plaintext = "hello_storebooster";
        let nonce_bytes = [0u8; 12];

        let key = Key::<Aes256Gcm>::from_slice(&key_arr);
        let cipher = Aes256Gcm::new(key);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let mut encrypted = cipher.encrypt(nonce, plaintext.as_bytes()).unwrap();
        let tag = encrypted.split_off(encrypted.len() - 16);
        let sealed = format!(
            "v2:{}:{}:{}",
            hex::encode(nonce_bytes),
            hex::encode(&encrypted),
            hex::encode(&tag)
        );

        let result = decrypt_with(&sealed, &key_arr).unwrap();
        assert_eq!(result, plaintext);
    }

    #[test]
    fn test_is_encrypted_v2_format() {
        assert!(is_encrypted("v2:aabbcc:ddeeff:001122"));
        assert!(!is_encrypted("plaintext_value"));
        assert!(!is_encrypted("v2:bad"));
        assert!(!is_encrypted("v2:::"));
        assert!(!is_encrypted(""));
    }

    #[test]
    fn test_is_encrypted_cbc_format() {
        assert!(is_encrypted("abcd1234:0a0b0c0d"));
        assert!(!is_encrypted("abcd1234:xyz"));
        assert!(!is_encrypted("aa:bb:cc"));
        assert!(!is_encrypted("aabbccdd"));
    }
}
