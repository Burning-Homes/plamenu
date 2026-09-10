//! Actor key material.
//!
//! `ActivityPub` HTTP signatures in the wild are RSA-SHA256 over 2048-bit keys;
//! that is what Mastodon generates and verifies, so it is what we generate.

use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::{RsaPrivateKey, RsaPublicKey};
use thiserror::Error;
use zeroize::Zeroize;

const KEY_BITS: usize = 2048;

#[derive(Debug, Error)]
pub enum KeyError {
    #[error("key generation failed: {0}")]
    Generate(#[from] rsa::Error),
    #[error("private key PEM encoding failed: {0}")]
    EncodePrivate(#[from] rsa::pkcs8::Error),
    #[error("public key PEM encoding failed: {0}")]
    EncodePublic(#[from] rsa::pkcs8::spki::Error),
    #[error("private key PEM decoding failed: {0}")]
    DecodePrivate(#[source] rsa::pkcs8::Error),
}

/// A freshly generated actor keypair, PEM-encoded for storage.
#[derive(Clone)]
pub struct KeyPairPem {
    /// PKCS#8 private key PEM. Secret; stored only for local accounts.
    pub private_pem: String,
    /// SPKI public key PEM, published in the actor document.
    pub public_pem: String,
}

impl Drop for KeyPairPem {
    fn drop(&mut self) {
        self.private_pem.zeroize();
    }
}

/// Generates a new RSA-2048 keypair for a local actor.
pub fn generate_keypair() -> Result<KeyPairPem, KeyError> {
    // OsRng from rsa's own rand_core lineage: rsa 0.9 is not compatible with
    // the current standalone `rand` (it needs rand_core 0.6).
    let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, KEY_BITS)?;
    let public = RsaPublicKey::from(&private);
    Ok(KeyPairPem {
        private_pem: private.to_pkcs8_pem(LineEnding::LF)?.to_string(),
        public_pem: public.to_public_key_pem(LineEnding::LF)?,
    })
}

/// Reconstructs the normalized SPKI public key from PKCS#8 private material.
/// Used before plaintext backfill sources are cleared.
pub fn rsa_public_from_private(private_pem: &str) -> Result<String, KeyError> {
    let private = RsaPrivateKey::from_pkcs8_pem(private_pem).map_err(KeyError::DecodePrivate)?;
    Ok(RsaPublicKey::from(&private).to_public_key_pem(LineEnding::LF)?)
}

/// Reconstructs an Ed25519 public Multikey from its private Multikey.
pub fn ed25519_public_from_private(
    private_multibase: &str,
) -> Result<String, crate::multikey::MultikeyError> {
    let secret = crate::multikey::decode_ed25519_private(private_multibase)?;
    let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
    Ok(crate::multikey::encode_ed25519_public(
        signing.verifying_key().as_bytes(),
    ))
}

/// A freshly generated Ed25519 keypair (FEP-521a), Multikey-encoded for
/// storage — the public half is published verbatim as `publicKeyMultibase`.
#[derive(Clone)]
pub struct Ed25519KeyPairMultibase {
    /// Secret Multikey (`z3u2…`). Stored only for local accounts.
    pub private_multibase: String,
    /// Public Multikey (`z6Mk…`), published in the actor document.
    pub public_multibase: String,
}

impl Drop for Ed25519KeyPairMultibase {
    fn drop(&mut self) {
        self.private_multibase.zeroize();
    }
}

/// Generates a new Ed25519 keypair for a local actor.
///
/// Key bytes come straight from the OS RNG: `ed25519_dalek`'s own generator
/// wants a `rand_core` 0.9 RNG while `rsa` pins 0.6, and 32 random bytes are
/// the whole input anyway.
#[must_use]
pub fn generate_ed25519_keypair() -> Ed25519KeyPairMultibase {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("system randomness available");
    let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
    Ed25519KeyPairMultibase {
        private_multibase: crate::multikey::encode_ed25519_private(&secret),
        public_multibase: crate::multikey::encode_ed25519_public(
            signing.verifying_key().as_bytes(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{generate_ed25519_keypair, generate_keypair};

    #[test]
    fn generates_ed25519_multikey_pair() {
        let pair = generate_ed25519_keypair();
        assert!(pair.public_multibase.starts_with("z6Mk"));
        assert!(pair.private_multibase.starts_with("z3u2"));
        // The stored secret must regenerate the same public key.
        let secret = crate::multikey::decode_ed25519_private(&pair.private_multibase).unwrap();
        let public = ed25519_dalek::SigningKey::from_bytes(&secret).verifying_key();
        assert_eq!(
            crate::multikey::encode_ed25519_public(public.as_bytes()),
            pair.public_multibase
        );
    }

    #[test]
    fn generates_pem_keypair() {
        let pair = generate_keypair().unwrap();
        assert!(pair.private_pem.starts_with("-----BEGIN PRIVATE KEY-----")); // gitleaks:allow
        assert!(pair.public_pem.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert!(
            pair.public_pem
                .trim_end()
                .ends_with("-----END PUBLIC KEY-----")
        );
    }
}
