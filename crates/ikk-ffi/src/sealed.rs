//! Small files encrypted at rest (the user's recorded voice commands):
//! AES-256-GCM, the key in the platform's secure store (Keychain, Android
//! Keystore, Windows Credential Manager), handed in by the shell as hex.
//!
//! Layout: `IKKS1` | 12-byte nonce | ciphertext with tag. Writes are atomic
//! (temp file + rename).

use std::path::Path;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

const MAGIC: &[u8] = b"IKKS1";
const KEY_LEN: usize = 32;

/// A new random key, as hex for the secure store.
pub fn new_key() -> Result<String, String> {
    let mut key = [0u8; KEY_LEN];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| "no random source".to_string())?;
    Ok(key.iter().map(|b| format!("{b:02x}")).collect())
}

fn key(hex: &str) -> Result<LessSafeKey, String> {
    let hex = hex.trim();
    if hex.len() != KEY_LEN * 2 || !hex.is_ascii() {
        return Err("bad file key".into());
    }
    let bytes = (0..KEY_LEN)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| "bad file key".to_string())?;
    let unbound = UnboundKey::new(&AES_256_GCM, &bytes).map_err(|_| "bad file key".to_string())?;
    Ok(LessSafeKey::new(unbound))
}

pub fn seal(key_hex: &str, plain: &str) -> Result<Vec<u8>, String> {
    let key = key(key_hex)?;
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| "no random source".to_string())?;
    let mut data = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(MAGIC), &mut data)
        .map_err(|_| "encryption failed".to_string())?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + data.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&data);
    Ok(out)
}

pub fn open(key_hex: &str, sealed: &[u8]) -> Result<String, String> {
    let key = key(key_hex)?;
    let rest = sealed.strip_prefix(MAGIC).ok_or("not an encrypted file")?;
    if rest.len() < NONCE_LEN {
        return Err("file too short".into());
    }
    let (nonce, data) = rest.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| "bad nonce".to_string())?;
    let mut data = data.to_vec();
    let plain = key
        .open_in_place(nonce, Aad::from(MAGIC), &mut data)
        .map_err(|_| "wrong key or damaged file".to_string())?;
    String::from_utf8(plain.to_vec()).map_err(|_| "damaged file".to_string())
}

/// Encrypt `plain` into `path` (directories are created).
pub fn save(path: &str, key_hex: &str, plain: &str) -> Result<(), String> {
    let path = Path::new(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, seal(key_hex, plain)?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// The decrypted file, or `None` when there is none.
pub fn load(path: &str, key_hex: &str) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => open(key_hex, &bytes).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_wrong_key() {
        let k = new_key().unwrap();
        assert_eq!(k.len(), 64);
        let sealed = seal(&k, "{\"model\":1}").unwrap();
        assert!(sealed.starts_with(MAGIC));
        assert!(!sealed.windows(5).any(|w| w == b"model"), "plain text leaked");
        assert_eq!(open(&k, &sealed).unwrap(), "{\"model\":1}");
        assert!(open(&new_key().unwrap(), &sealed).is_err());
        let mut damaged = sealed.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(open(&k, &damaged).is_err());
        assert!(key("xyz").is_err());
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join(format!("ikk-sealed-{}", std::process::id()));
        let path = dir.join("sub").join("v.bin");
        let p = path.to_str().unwrap();
        let k = new_key().unwrap();
        assert_eq!(load(p, &k).unwrap(), None);
        save(p, &k, "hello").unwrap();
        assert_eq!(load(p, &k).unwrap().as_deref(), Some("hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
