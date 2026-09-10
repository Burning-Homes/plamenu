//! Multikey encoding (FEP-521a / W3C Controlled Identifiers): keys as
//! multibase base58btc strings with a multicodec prefix.
//!
//! RSA, Ed25519 and ML-DSA-44 public keys are decoded. That is Mastodon 4.7's
//! FEP-521a input set and covers both HTTP signatures and the two Object
//! Integrity Proof suites Plamenu accepts.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::RsaPublicKey;
use rsa::pkcs1::{DecodeRsaPublicKey, EncodeRsaPublicKey};
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey, LineEnding};
use thiserror::Error;

/// multicodec `ed25519-pub` (0xed), varint-encoded.
const ED25519_PUB_PREFIX: [u8; 2] = [0xed, 0x01];
/// multicodec `ed25519-priv` (0x1300), varint-encoded.
const ED25519_PRIV_PREFIX: [u8; 2] = [0x80, 0x26];
/// multicodec `rsa-pub` (0x1205), varint-encoded.
const RSA_PUB_PREFIX: [u8; 2] = [0x85, 0x24];
/// multicodec `mldsa-44-pub` (0x1210), varint-encoded.
const ML_DSA_44_PUB_PREFIX: [u8; 2] = [0x90, 0x24];
pub const ML_DSA_44_PUBLIC_LEN: usize = 1_312;

/// A decoded FEP-521a verification method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicMultikey {
    /// SPKI PEM, normalized from the encoded PKCS#1 key.
    Rsa(String),
    Ed25519([u8; 32]),
    MlDsa44(Box<[u8; ML_DSA_44_PUBLIC_LEN]>),
}

impl PublicMultikey {
    #[must_use]
    pub const fn algorithm(&self) -> &'static str {
        match self {
            Self::Rsa(_) => "rsa",
            Self::Ed25519(_) => "ed25519",
            Self::MlDsa44(_) => "ml-dsa-44",
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MultikeyError {
    #[error("multibase value does not use the base58btc ('z') prefix")]
    NotBase58Btc,
    #[error("multibase value is not valid base58")]
    BadBase58,
    #[error("multibase value is not valid base64url")]
    BadBase64,
    #[error("multikey is not an Ed25519 key")]
    NotEd25519,
    #[error("multikey uses an unsupported multicodec")]
    UnsupportedCodec,
    #[error("Ed25519 multikey has the wrong length")]
    BadLength,
    #[error("ML-DSA-44 multikey has the wrong length")]
    BadMlDsaLength,
    #[error("RSA multikey does not contain a valid PKCS#1 public key")]
    BadRsa,
}

fn encode(prefix: [u8; 2], key: &[u8; 32]) -> String {
    let mut bytes = Vec::with_capacity(2 + key.len());
    bytes.extend_from_slice(&prefix);
    bytes.extend_from_slice(key);
    format!("z{}", bs58::encode(bytes).into_string())
}

fn decode(prefix: [u8; 2], value: &str) -> Result<[u8; 32], MultikeyError> {
    let encoded = value.strip_prefix('z').ok_or(MultikeyError::NotBase58Btc)?;
    let bytes = bs58::decode(encoded)
        .into_vec()
        .map_err(|_| MultikeyError::BadBase58)?;
    let key = bytes
        .strip_prefix(&prefix[..])
        .ok_or(MultikeyError::NotEd25519)?;
    key.try_into().map_err(|_| MultikeyError::BadLength)
}

/// Decodes a base58btc (`z`) or base64url-nopad (`u`) Multibase value. The
/// 2026 ML-DSA cryptosuite requires `u`; deployed Ed25519/RSA Multikeys use
/// `z`, so the bounded decoder deliberately supports both.
pub fn decode_multibase(value: &str) -> Result<Vec<u8>, MultikeyError> {
    if let Some(encoded) = value.strip_prefix('z') {
        bs58::decode(encoded)
            .into_vec()
            .map_err(|_| MultikeyError::BadBase58)
    } else if let Some(encoded) = value.strip_prefix('u') {
        URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| MultikeyError::BadBase64)
    } else {
        Err(MultikeyError::NotBase58Btc)
    }
}

/// Decodes the complete Mastodon-4.7/FEP-521a public-key input set.
pub fn decode_public(value: &str) -> Result<PublicMultikey, MultikeyError> {
    let bytes = decode_multibase(value)?;
    if let Some(raw) = bytes.strip_prefix(&ED25519_PUB_PREFIX) {
        return raw
            .try_into()
            .map(PublicMultikey::Ed25519)
            .map_err(|_| MultikeyError::BadLength);
    }
    if let Some(raw) = bytes.strip_prefix(&ML_DSA_44_PUB_PREFIX) {
        let key: [u8; ML_DSA_44_PUBLIC_LEN] =
            raw.try_into().map_err(|_| MultikeyError::BadMlDsaLength)?;
        return Ok(PublicMultikey::MlDsa44(Box::new(key)));
    }
    if let Some(raw) = bytes.strip_prefix(&RSA_PUB_PREFIX) {
        let key = RsaPublicKey::from_pkcs1_der(raw).map_err(|_| MultikeyError::BadRsa)?;
        let pem = key
            .to_public_key_pem(LineEnding::LF)
            .map_err(|_| MultikeyError::BadRsa)?;
        return Ok(PublicMultikey::Rsa(pem));
    }
    Err(MultikeyError::UnsupportedCodec)
}

/// Decodes an RSA public Multikey to normalized SPKI PEM.
pub fn decode_rsa_public(value: &str) -> Result<String, MultikeyError> {
    match decode_public(value)? {
        PublicMultikey::Rsa(pem) => Ok(pem),
        _ => Err(MultikeyError::UnsupportedCodec),
    }
}

/// Decodes an ML-DSA-44 public Multikey to its FIPS-204 1,312-byte `pkEncode`
/// representation.
pub fn decode_ml_dsa_44_public(
    value: &str,
) -> Result<Box<[u8; ML_DSA_44_PUBLIC_LEN]>, MultikeyError> {
    match decode_public(value)? {
        PublicMultikey::MlDsa44(key) => Ok(key),
        _ => Err(MultikeyError::UnsupportedCodec),
    }
}

/// Encodes an ML-DSA-44 FIPS-204 public key using the base64url-nopad
/// Multibase form mandated by the 2026 W3C cryptosuite draft.
#[must_use]
pub fn encode_ml_dsa_44_public(key: &[u8; ML_DSA_44_PUBLIC_LEN]) -> String {
    let mut bytes = Vec::with_capacity(ML_DSA_44_PUB_PREFIX.len() + key.len());
    bytes.extend_from_slice(&ML_DSA_44_PUB_PREFIX);
    bytes.extend_from_slice(key);
    format!("u{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Encodes an Ed25519 public key as a Multikey (`z6Mk…`).
#[must_use]
pub fn encode_ed25519_public(key: &[u8; 32]) -> String {
    encode(ED25519_PUB_PREFIX, key)
}

/// Decodes an Ed25519 public Multikey (`z6Mk…`).
pub fn decode_ed25519_public(value: &str) -> Result<[u8; 32], MultikeyError> {
    match decode_public(value) {
        Ok(PublicMultikey::Ed25519(key)) => Ok(key),
        Ok(_)
        | Err(
            MultikeyError::UnsupportedCodec | MultikeyError::BadRsa | MultikeyError::BadMlDsaLength,
        ) => Err(MultikeyError::NotEd25519),
        Err(other) => Err(other),
    }
}

/// Encodes an Ed25519 secret key as a Multikey (`z3u2…`).
#[must_use]
pub fn encode_ed25519_private(key: &[u8; 32]) -> String {
    encode(ED25519_PRIV_PREFIX, key)
}

/// Decodes an Ed25519 secret Multikey (`z3u2…`).
pub fn decode_ed25519_private(value: &str) -> Result<[u8; 32], MultikeyError> {
    decode(ED25519_PRIV_PREFIX, value)
}

/// Encodes an RSA public key, given as the SPKI PEM we publish under
/// `publicKey`, as an `rsa-pub` Multikey (`z4MX…`). The key material is the
/// PKCS#1 `RSAPublicKey` DER the `rsa-pub` multicodec (and did:key) specifies.
///
/// `None` when the PEM does not parse — an actor document is still perfectly
/// serviceable without the FEP-521a mirror of its legacy key.
#[must_use]
pub fn encode_rsa_public(public_key_pem: &str) -> Option<String> {
    let der = RsaPublicKey::from_public_key_pem(public_key_pem)
        .ok()?
        .to_pkcs1_der()
        .ok()?;
    let mut bytes = Vec::with_capacity(RSA_PUB_PREFIX.len() + der.as_bytes().len());
    bytes.extend_from_slice(&RSA_PUB_PREFIX);
    bytes.extend_from_slice(der.as_bytes());
    Some(format!("z{}", bs58::encode(bytes).into_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_key_roundtrip_with_did_key_prefix() {
        let key = [7u8; 32];
        let encoded = encode_ed25519_public(&key);
        // The ed25519-pub multicodec always base58-encodes to a `z6Mk` head —
        // the shape peers (and the did:key method) expect.
        assert!(encoded.starts_with("z6Mk"), "{encoded}");
        assert_eq!(decode_ed25519_public(&encoded).unwrap(), key);
    }

    #[test]
    fn private_key_roundtrip() {
        let key = [42u8; 32];
        let encoded = encode_ed25519_private(&key);
        assert!(encoded.starts_with("z3u2"), "{encoded}");
        assert_eq!(decode_ed25519_private(&encoded).unwrap(), key);
    }

    /// The did:key test vector from the Multikey spec: this exact multibase
    /// value must decode to a valid Ed25519 public key.
    #[test]
    fn decodes_w3c_test_vector() {
        let vector = "z6MkiTBz1ymuepAQ4HEHYSF1H8quG5GLVVQR3djdX3mDooWp";
        let key = decode_ed25519_public(vector).unwrap();
        assert_eq!(encode_ed25519_public(&key), vector);
    }

    #[test]
    fn rejects_foreign_encodings() {
        assert_eq!(
            decode_ed25519_public("xAAAA"),
            Err(MultikeyError::NotBase58Btc),
            "unknown multibase prefix is rejected"
        );
        assert_eq!(
            decode_ed25519_public("z0O"),
            Err(MultikeyError::BadBase58),
            "0 and O are outside the base58 alphabet"
        );
        // An RSA multikey (multicodec 0x1205) must be reported as foreign,
        // not decoded as garbage Ed25519 bytes.
        let rsa_prefixed = format!("z{}", bs58::encode([0x85u8, 0x24, 1, 2, 3]).into_string());
        assert_eq!(
            decode_ed25519_public(&rsa_prefixed),
            Err(MultikeyError::NotEd25519)
        );
        // A private-key multikey is not a public key.
        let private = encode_ed25519_private(&[1u8; 32]);
        assert_eq!(
            decode_ed25519_public(&private),
            Err(MultikeyError::NotEd25519)
        );
        // Truncated key material.
        let short = format!("z{}", bs58::encode([0xedu8, 0x01, 9, 9]).into_string());
        assert_eq!(decode_ed25519_public(&short), Err(MultikeyError::BadLength));
    }

    /// The `rsa-pub` Multikey must carry the PKCS#1 DER of the same key the
    /// legacy `publicKey` PEM holds — that byte-for-byte identity is what lets
    /// a peer that keeps only FEP-521a keys still verify our `#main-key`
    /// signatures.
    #[test]
    fn rsa_multikey_carries_the_public_key_pem_verbatim() {
        use rsa::pkcs1::DecodeRsaPublicKey;
        use rsa::pkcs8::EncodePublicKey;

        let pair = crate::keys::generate_keypair().unwrap();
        let encoded = encode_rsa_public(&pair.public_pem).unwrap();
        // The rsa-pub multicodec always base58-encodes to a `z4MX` head.
        assert!(encoded.starts_with("z4MX"), "{encoded}");

        let der = bs58::decode(encoded.strip_prefix('z').unwrap())
            .into_vec()
            .unwrap();
        let der = der.strip_prefix(&RSA_PUB_PREFIX[..]).unwrap();
        let decoded = rsa::RsaPublicKey::from_pkcs1_der(der).unwrap();
        assert_eq!(
            decoded
                .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
                .unwrap(),
            pair.public_pem
        );
    }

    /// An Ed25519 decoder must not be confused by our own RSA entry, and a
    /// malformed PEM must degrade to `None` rather than panic.
    #[test]
    fn rsa_multikey_is_not_an_ed25519_key() {
        let pair = crate::keys::generate_keypair().unwrap();
        let encoded = encode_rsa_public(&pair.public_pem).unwrap();
        assert_eq!(
            decode_ed25519_public(&encoded),
            Err(MultikeyError::NotEd25519)
        );
        assert_eq!(
            encode_rsa_public("-----BEGIN PUBLIC KEY-----\nnope\n"),
            None
        );
    }

    #[test]
    fn decodes_rsa_and_ml_dsa_44_public_keys() {
        let pair = crate::keys::generate_keypair().unwrap();
        let rsa = encode_rsa_public(&pair.public_pem).unwrap();
        assert_eq!(decode_rsa_public(&rsa).unwrap(), pair.public_pem);

        let raw = [23u8; ML_DSA_44_PUBLIC_LEN];
        let encoded = encode_ml_dsa_44_public(&raw);
        assert!(encoded.starts_with('u'));
        assert_eq!(decode_ml_dsa_44_public(&encoded).unwrap().as_ref(), &raw);
        assert!(matches!(
            decode_public(&encoded),
            Ok(PublicMultikey::MlDsa44(_))
        ));
    }
}
