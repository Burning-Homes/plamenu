//! SHA-256 hashlinks used by the FEP-ae97 media gateway.

use sha2::{Digest as _, Sha256};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HashlinkError {
    #[error("invalid hashlink")]
    Invalid,
}

/// Creates a base58btc SHA2-256 multihash (`hl:zQm…`).
#[must_use]
pub fn encode(bytes: &[u8]) -> (String, [u8; 32]) {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let mut multihash = Vec::with_capacity(34);
    // sha2-256 multicodec (0x12), followed by its 32-byte digest length.
    multihash.extend_from_slice(&[0x12, 0x20]);
    multihash.extend_from_slice(&digest);
    (
        format!("hl:z{}", bs58::encode(multihash).into_string()),
        digest,
    )
}

/// Parses the exact SHA2-256 hashlink form emitted by [`encode`].
pub fn decode(value: &str) -> Result<[u8; 32], HashlinkError> {
    let encoded = value
        .strip_prefix("hl:z")
        .filter(|value| !value.is_empty())
        .ok_or(HashlinkError::Invalid)?;
    let decoded = bs58::decode(encoded)
        .into_vec()
        .map_err(|_| HashlinkError::Invalid)?;
    let digest: [u8; 32] = decoded
        .strip_prefix(&[0x12, 0x20])
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(HashlinkError::Invalid)?;
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_hashlink_spec_vector() {
        let (value, digest) = encode(b"Hello World!");
        assert_eq!(value, "hl:zQmWvQxTqbG2Z9HPJgG57jjwR154cKhbtJenbyYTWkjgF3e");
        assert_eq!(decode(&value).unwrap(), digest);
    }

    #[test]
    fn rejects_non_sha256_and_non_hashlink_values() {
        assert_eq!(
            decode("https://example.test/x"),
            Err(HashlinkError::Invalid)
        );
        assert_eq!(
            decode("hl:z3vQB7B6MrGQZaxCuFg4oh"),
            Err(HashlinkError::Invalid)
        );
    }
}
