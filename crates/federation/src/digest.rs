//! The `Digest` request header (RFC 3230 style, as Mastodon sends/expects).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest as _, Sha256};

/// Computes the `Digest` header value for a request body.
#[must_use]
pub fn compute(body: &[u8]) -> String {
    format!("SHA-256={}", BASE64.encode(Sha256::digest(body)))
}

/// Checks a received `Digest` header against the actual body. The header may
/// list several algorithms; only `SHA-256` (case-insensitive) is considered.
#[must_use]
pub fn matches(header_value: &str, body: &[u8]) -> bool {
    header_value.split(',').any(|entry| {
        entry
            .trim()
            .split_once('=')
            .is_some_and(|(algorithm, value)| {
                algorithm.eq_ignore_ascii_case("sha-256")
                    && BASE64
                        .decode(value)
                        .is_ok_and(|decoded| decoded == Sha256::digest(body).as_slice())
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_known_vector() {
        // sha256("hello") base64.
        assert_eq!(
            compute(b"hello"),
            "SHA-256=LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ="
        );
    }

    #[test]
    fn roundtrip_matches() {
        let body = br#"{"type":"Follow"}"#;
        assert!(matches(&compute(body), body));
    }

    #[test]
    fn rejects_tampered_body_and_garbage() {
        let header = compute(b"original");
        assert!(!matches(&header, b"tampered"));
        assert!(!matches("SHA-256=!!!not-base64!!!", b"original"));
        assert!(!matches("unixsum=30637", b"original"));
        assert!(!matches("", b"original"));
    }

    #[test]
    fn accepts_multi_algorithm_and_case_variants() {
        let body = b"hello";
        let b64 = compute(body);
        let value = b64.strip_prefix("SHA-256=").unwrap();
        assert!(matches(&format!("unixsum=123, sha-256={value}"), body));
    }
}
