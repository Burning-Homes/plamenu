//! RFC 6238 TOTP with Mastodon's parameters (ROTP defaults): HMAC-SHA1,
//! 30-second timestep, 6 digits, ±1 step of clock drift, 160-bit secrets
//! rendered as 32 unpadded base32 characters.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

/// Seconds per timestep.
pub const PERIOD_SECONDS: u64 = 30;

/// Accepted clock drift, in timesteps either side of "now".
const DRIFT_STEPS: i64 = 1;

const BASE32: base32::Alphabet = base32::Alphabet::Rfc4648 { padding: false };

/// A fresh 160-bit secret as unpadded base32 (32 chars) — what the otpauth
/// URI carries and authenticator apps accept as the manual entry key.
#[must_use]
pub fn generate_secret() -> String {
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes).expect("system randomness available");
    base32::encode(BASE32, &bytes)
}

/// The 6-digit code for one timestep.
#[must_use]
pub fn code_for(secret: &[u8], timestep: u64) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&timestep.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    // RFC 4226 dynamic truncation.
    let offset = usize::from(digest[digest.len() - 1] & 0xf);
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    format!("{:06}", binary % 1_000_000)
}

/// Verifies `code` against the drift window around `now_epoch`; returns the
/// matched timestep so the caller can consume it (replay protection).
#[must_use]
pub fn verify(secret_base32: &str, code: &str, now_epoch: u64) -> Option<i64> {
    let secret = base32::decode(BASE32, secret_base32.trim())?;
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 {
        return None;
    }
    let current = i64::try_from(now_epoch / PERIOD_SECONDS).ok()?;
    (current - DRIFT_STEPS..=current + DRIFT_STEPS)
        .filter(|step| *step >= 0)
        .find(|step| {
            constant_time_eq(
                code.as_bytes(),
                code_for(&secret, step.cast_unsigned()).as_bytes(),
            )
        })
}

/// The provisioning URI encoded into the enrollment QR code, in ROTP's shape:
/// `otpauth://totp/{issuer}:{account}?secret=…&issuer={issuer}`.
#[must_use]
pub fn provisioning_uri(secret_base32: &str, issuer: &str, account: &str) -> String {
    let label = urlencode(&format!("{issuer}:{account}"));
    let issuer = urlencode(issuer);
    format!("otpauth://totp/{label}?secret={secret_base32}&issuer={issuer}")
}

fn urlencode(value: &str) -> String {
    // Percent-encode everything outside RFC 3986 unreserved; plenty for
    // e-mail addresses and hostnames in a label.
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0xf)]));
            }
        }
    }
    encoded
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B SHA-1 vectors (their last 6 digits — the RFC
    /// tabulates 8-digit codes for the ASCII secret `12345678901234567890`).
    #[test]
    fn rfc6238_sha1_vectors() {
        let secret = b"12345678901234567890";
        for (time, expected) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(code_for(secret, time / PERIOD_SECONDS), expected, "{time}");
        }
    }

    #[test]
    fn verify_accepts_one_step_of_drift() {
        let secret_b32 = base32::encode(BASE32, b"12345678901234567890");
        // T=59 is timestep 1; its code must pass at steps 0..=2 and fail
        // beyond.
        assert_eq!(verify(&secret_b32, "287082", 59), Some(1));
        assert_eq!(verify(&secret_b32, "287082", 30), Some(1)); // step 0, +1 drift
        assert_eq!(verify(&secret_b32, "287082", 89), Some(1)); // step 2, -1 drift
        assert_eq!(verify(&secret_b32, "287082", 120), None); // step 4
        assert_eq!(verify(&secret_b32, "287 082", 59), Some(1)); // spaces ok
        assert_eq!(verify(&secret_b32, "000000", 59), None);
        assert_eq!(verify(&secret_b32, "28708", 59), None);
        assert_eq!(verify("not base32!!", "287082", 59), None);
    }

    #[test]
    fn secrets_are_32_base32_chars() {
        let secret = generate_secret();
        assert_eq!(secret.len(), 32);
        assert!(base32::decode(BASE32, &secret).is_some_and(|b| b.len() == 20));
        assert_ne!(secret, generate_secret());
    }

    #[test]
    fn provisioning_uri_is_rotp_shaped() {
        assert_eq!(
            provisioning_uri("ABC234", "plamenu.local", "alice@example.com"),
            "otpauth://totp/plamenu.local%3Aalice%40example.com?secret=ABC234&issuer=plamenu.local"
        );
    }
}
