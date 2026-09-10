//! Encryption at rest for small secrets (the TOTP secret): AES-256-GCM with
//! a key derived from the configured `encryption_secret` via HKDF-SHA256. The
//! secret never touches the database, so a dump alone cannot recover the
//! plaintext — the same reasoning that moved Mastodon 4.x to encrypted
//! `otp_secret`s.

use std::collections::HashMap;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroize;

use crate::config::Config;

const NONCE_LEN: usize = 12;

/// A derived-key AES-256-GCM box. Construct once per purpose with a distinct
/// `info` string so different secret kinds never share a key.
pub struct SecretBox {
    key: [u8; 32],
}

impl Drop for SecretBox {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl SecretBox {
    #[must_use]
    pub fn new(secret: &str, info: &[u8]) -> Self {
        let hkdf = Hkdf::<Sha256>::new(None, secret.as_bytes());
        let mut key = [0u8; 32];
        hkdf.expand(info, &mut key)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self { key }
    }

    /// `base64url(nonce ‖ ciphertext ‖ tag)` with a fresh random nonce.
    #[must_use]
    pub fn encrypt(&self, plaintext: &[u8]) -> String {
        self.encrypt_with_aad(plaintext, &[])
    }

    /// Authenticated encryption additionally bound to immutable row context
    /// (for federation keys, the exact published key URI).
    #[must_use]
    pub fn encrypt_with_aad(&self, plaintext: &[u8], aad: &[u8]) -> String {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("system randomness available");
        let ciphertext = Aes256Gcm::new_from_slice(&self.key)
            .expect("32-byte key")
            .encrypt(
                &Nonce::try_from(nonce.as_slice()).expect("12-byte nonce"),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("AES-GCM encryption is infallible for in-memory data");
        let mut combined = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        combined.extend_from_slice(&nonce);
        combined.extend_from_slice(&ciphertext);
        URL_SAFE_NO_PAD.encode(combined)
    }

    /// `None` on any malformed or tampered input (wrong key included).
    #[must_use]
    pub fn decrypt(&self, stored: &str) -> Option<Vec<u8>> {
        self.decrypt_with_aad(stored, &[])
    }

    #[must_use]
    pub fn decrypt_with_aad(&self, stored: &str, aad: &[u8]) -> Option<Vec<u8>> {
        let combined = URL_SAFE_NO_PAD.decode(stored).ok()?;
        if combined.len() <= NONCE_LEN {
            return None;
        }
        let (nonce, ciphertext) = combined.split_at(NONCE_LEN);
        Aes256Gcm::new_from_slice(&self.key)
            .expect("32-byte key")
            .decrypt(
                &Nonce::try_from(nonce).expect("12-byte nonce"),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .ok()
    }
}

/// A decrypted signing key. Its contents are deliberately absent from
/// `Debug` and zeroized on drop; exposing them is an explicit operation at the
/// narrow signer boundary.
pub struct PrivateSigningKey(Vec<u8>);

impl PrivateSigningKey {
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    pub fn expose_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.0)
    }
}

impl std::fmt::Debug for PrivateSigningKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PrivateSigningKey([REDACTED])")
    }
}

impl Drop for PrivateSigningKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeyEncryptionError {
    #[error("federation signing-key encryption is not configured")]
    MissingConfiguration,
    #[error("federation signing-key encryption secret for version {0} is shorter than 32 bytes")]
    WeakConfiguration(i32),
    #[error("unsupported or malformed private-key ciphertext envelope")]
    BadEnvelope,
    #[error("no configured encryption key can open ciphertext version {0}")]
    UnknownVersion(i32),
    #[error("private-key ciphertext authentication failed")]
    Authentication,
}

/// Ciphertext ready for the normalized key repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedPrivateKey {
    pub ciphertext: String,
    pub key_version: i32,
}

/// Primary + decrypt-only previous keys for rolling at-rest key rotation.
/// Each secret derives a purpose-specific AES key through a federation-only
/// HKDF info string; TOTP and federation material never share a derived key.
pub struct FederationKeyring {
    primary_version: i32,
    boxes: HashMap<i32, SecretBox>,
}

impl FederationKeyring {
    #[must_use]
    pub const fn primary_version(&self) -> i32 {
        self.primary_version
    }

    pub fn from_config(config: &Config) -> Result<Self, KeyEncryptionError> {
        let primary = config
            .encryption_secret
            .as_ref()
            .ok_or(KeyEncryptionError::MissingConfiguration)?;
        if primary.expose().len() < 32 {
            return Err(KeyEncryptionError::WeakConfiguration(
                config.encryption_secret_version,
            ));
        }
        let mut boxes = HashMap::with_capacity(1 + config.encryption_previous_secrets.len());
        boxes.insert(
            config.encryption_secret_version,
            SecretBox::new(primary.expose(), b"plamenu:federation-signing-key:v1"),
        );
        for (version, secret) in &config.encryption_previous_secrets {
            if secret.expose().len() < 32 {
                return Err(KeyEncryptionError::WeakConfiguration(*version));
            }
            boxes.insert(
                *version,
                SecretBox::new(secret.expose(), b"plamenu:federation-signing-key:v1"),
            );
        }
        Ok(Self {
            primary_version: config.encryption_secret_version,
            boxes,
        })
    }

    #[cfg(test)]
    fn from_test_secrets(primary_version: i32, primary: &str, previous: &[(i32, &str)]) -> Self {
        let mut boxes = HashMap::new();
        boxes.insert(
            primary_version,
            SecretBox::new(primary, b"plamenu:federation-signing-key:v1"),
        );
        for (version, secret) in previous {
            boxes.insert(
                *version,
                SecretBox::new(secret, b"plamenu:federation-signing-key:v1"),
            );
        }
        Self {
            primary_version,
            boxes,
        }
    }

    fn aad(key_uri: &str) -> Vec<u8> {
        format!("plamenu:federation-signing-key:{key_uri}").into_bytes()
    }

    #[must_use]
    pub fn encrypt(&self, key_uri: &str, plaintext: &[u8]) -> EncryptedPrivateKey {
        let body = self
            .boxes
            .get(&self.primary_version)
            .expect("primary encryption key is present")
            .encrypt_with_aad(plaintext, &Self::aad(key_uri));
        EncryptedPrivateKey {
            ciphertext: format!("fsk1.{}.{}", self.primary_version, body),
            key_version: self.primary_version,
        }
    }

    pub fn decrypt(
        &self,
        key_uri: &str,
        stored_version: i32,
        envelope: &str,
    ) -> Result<PrivateSigningKey, KeyEncryptionError> {
        let mut fields = envelope.splitn(3, '.');
        if fields.next() != Some("fsk1") {
            return Err(KeyEncryptionError::BadEnvelope);
        }
        let version = fields
            .next()
            .and_then(|raw| raw.parse::<i32>().ok())
            .ok_or(KeyEncryptionError::BadEnvelope)?;
        let body = fields.next().ok_or(KeyEncryptionError::BadEnvelope)?;
        if version != stored_version {
            return Err(KeyEncryptionError::BadEnvelope);
        }
        let secret_box = self
            .boxes
            .get(&version)
            .ok_or(KeyEncryptionError::UnknownVersion(version))?;
        let plaintext = secret_box
            .decrypt_with_aad(body, &Self::aad(key_uri))
            .ok_or(KeyEncryptionError::Authentication)?;
        Ok(PrivateSigningKey(plaintext))
    }
}

/// The box for TOTP secrets, or `None` when the operator has not configured
/// `encryption_secret` (two-factor enrollment is then unavailable).
#[must_use]
pub fn otp_box(config: &Config) -> Option<SecretBox> {
    config
        .encryption_secret
        .as_ref()
        .map(|secret| SecretBox::new(secret.expose(), b"plamenu:otp-secret"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_tampering() {
        let secret_box = SecretBox::new("test secret", b"test:info");
        let stored = secret_box.encrypt(b"JBSWY3DPEHPK3PXP");
        assert_eq!(
            secret_box.decrypt(&stored).as_deref(),
            Some(b"JBSWY3DPEHPK3PXP".as_slice())
        );

        // Distinct nonces: the same plaintext encrypts differently.
        assert_ne!(stored, secret_box.encrypt(b"JBSWY3DPEHPK3PXP"));

        // A different key (info string) cannot read it.
        let other = SecretBox::new("test secret", b"other:info");
        assert_eq!(other.decrypt(&stored), None);

        // Bit flips are rejected by the GCM tag.
        let mut tampered = URL_SAFE_NO_PAD.decode(&stored).unwrap();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert_eq!(secret_box.decrypt(&URL_SAFE_NO_PAD.encode(tampered)), None);
        assert_eq!(secret_box.decrypt("not-base64!"), None);
        assert_eq!(secret_box.decrypt(""), None);
    }

    #[test]
    fn federation_envelope_is_versioned_bound_and_redacted() {
        let ring = FederationKeyring::from_test_secrets(2, "new secret", &[(1, "old secret")]);
        let encrypted = ring.encrypt("https://example/actor#key", b"PRIVATE KEY");
        assert!(encrypted.ciphertext.starts_with("fsk1.2."));
        let plain = ring
            .decrypt(
                "https://example/actor#key",
                encrypted.key_version,
                &encrypted.ciphertext,
            )
            .unwrap();
        assert_eq!(plain.expose(), b"PRIVATE KEY");
        assert!(!format!("{plain:?}").contains("PRIVATE KEY"));
        assert!(matches!(
            ring.decrypt(
                "https://example/other#key",
                encrypted.key_version,
                &encrypted.ciphertext,
            ),
            Err(KeyEncryptionError::Authentication)
        ));
    }
}
