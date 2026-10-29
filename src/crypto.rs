use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::Engine;
use sha2::{Digest, Sha256};

const NONCE_LEN: usize = 12;

/// AES-256-GCM envelope for provider credentials stored in Postgres (and conversation state in
/// Valkey). During a key rotation it also holds the key being retired: values are opened with
/// either key and sealed with the current one only.
#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
    /// SHA-256 of the key, to tell two boxes apart without keeping the key around.
    fingerprint: [u8; 32],
    previous: Option<Box<SecretBox>>,
}

impl SecretBox {
    pub fn from_base64(key_b64: &str) -> Result<Self, String> {
        let key = base64::engine::general_purpose::STANDARD
            .decode(key_b64.trim())
            .map_err(|_| "must be base64".to_string())?;
        if key.len() != 32 {
            return Err(format!("must decode to 32 bytes, got {}", key.len()));
        }
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "invalid key".to_string())?;
        let mut fingerprint = [0u8; 32];
        fingerprint.copy_from_slice(&Sha256::digest(&key));
        Ok(Self { cipher, fingerprint, previous: None })
    }

    /// The key being retired: `open` falls back to it, `seal` never uses it.
    pub fn with_previous(mut self, previous: SecretBox) -> Result<Self, String> {
        if previous.fingerprint == self.fingerprint {
            return Err("is the same key as GATEWAY_ENCRYPTION_KEY".into());
        }
        self.previous = Some(Box::new(previous));
        Ok(self)
    }

    pub fn has_previous(&self) -> bool {
        self.previous.is_some()
    }

    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let nonce_bytes = crate::util::random_bytes::<NONCE_LEN>();
        let nonce = Nonce::try_from(&nonce_bytes[..]).expect("nonce length");
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .expect("AES-GCM encryption cannot fail for in-memory buffers");
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        out
    }

    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        match (self.open_current(sealed), &self.previous) {
            (Err(_), Some(previous)) => previous.open(sealed),
            (result, _) => result,
        }
    }

    fn open_current(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        if sealed.len() < NONCE_LEN + 16 {
            return Err("ciphertext too short".into());
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_from(nonce).map_err(|_| "bad nonce".to_string())?;
        self.cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| "decryption failed (wrong GATEWAY_ENCRYPTION_KEY?)".to_string())
    }

    /// Re-seals a value under the current key: `None` when it already is, the new value when the
    /// previous key opens it, and an error when neither does.
    pub fn reseal(&self, sealed: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if self.open_current(sealed).is_ok() {
            return Ok(None);
        }
        let previous = self.previous.as_ref().ok_or_else(|| "no previous key to re-encrypt from".to_string())?;
        Ok(Some(self.seal(&previous.open(sealed)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(byte: u8) -> SecretBox {
        SecretBox::from_base64(&base64::engine::general_purpose::STANDARD.encode([byte; 32])).unwrap()
    }

    #[test]
    fn roundtrip() {
        let b = boxed(7);
        let sealed = b.seal(b"hello");
        assert_eq!(b.open(&sealed).unwrap(), b"hello");
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(b.open(&tampered).is_err());
    }

    #[test]
    fn rotation() {
        let old = boxed(1);
        let new = boxed(2);
        let unrelated = boxed(3);
        let rotating = boxed(2).with_previous(boxed(1)).unwrap();

        // The retired key still opens what it sealed; new seals never use it.
        let under_old = old.seal(b"secret");
        assert_eq!(rotating.open(&under_old).unwrap(), b"secret");
        assert!(new.open(&under_old).is_err());
        assert_eq!(new.open(&rotating.seal(b"fresh")).unwrap(), b"fresh");

        // reseal moves a value to the current key once, and leaves it alone afterwards.
        let moved = rotating.reseal(&under_old).unwrap().expect("re-encrypted");
        assert_eq!(new.open(&moved).unwrap(), b"secret");
        assert!(old.open(&moved).is_err());
        assert!(rotating.reseal(&moved).unwrap().is_none());

        // A value neither key opens is an error, not a silent skip.
        assert!(rotating.reseal(&unrelated.seal(b"x")).is_err());
        assert!(new.reseal(&under_old).is_err());

        assert!(boxed(1).with_previous(boxed(1)).is_err());
    }
}
