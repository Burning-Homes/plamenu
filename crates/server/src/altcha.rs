//! Self-hosted ALTCHA Proof-of-Work v2 for the browser sign-up form.
//!
//! Challenges are short-lived, HMAC-signed, purpose-bound, and deterministic:
//! the browser searches for a hidden counter while the server verifies the
//! submitted derived key through ALTCHA's key-signature fast path. Successfully
//! used challenge signatures are retained until expiry so a captured solution
//! cannot create more than one account.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use altcha::{
    Challenge, CreateChallengeOptions, Payload, VerifySolutionOptions, create_challenge,
    verify_solution,
};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use serde_json::Value;
use sha2::Sha256;
use zeroize::Zeroize;

use crate::config::{AltchaConfig, Config};

const PURPOSE: &str = "plamenu-signup-v1";
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const MAX_REPLAY_ENTRIES: usize = 50_000;

/// A verification failure safe to collapse into one user-facing form error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    Missing,
    Malformed,
    Invalid,
    Replayed,
}

/// Process-wide challenge signer and bounded replay ledger.
pub struct Altcha {
    challenge_secret: String,
    key_secret: String,
    config: AltchaConfig,
    used: Mutex<HashMap<String, u64>>,
}

impl Drop for Altcha {
    fn drop(&mut self) {
        self.challenge_secret.zeroize();
        self.key_secret.zeroize();
    }
}

impl Altcha {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        // Production configuration requires this secret. The fallback keeps
        // directly-constructed legacy/test Config values safe: challenges then
        // survive for this process only rather than using a fixed key.
        let ephemeral;
        let master = if let Some(secret) = &config.encryption_secret {
            secret.expose().as_bytes()
        } else {
            ephemeral = crate::auth::generate_secret();
            ephemeral.as_bytes()
        };
        Self::from_master(master, config.altcha)
    }

    fn from_master(master: &[u8], config: AltchaConfig) -> Self {
        let hkdf = Hkdf::<Sha256>::new(None, master);
        let mut challenge_key = [0_u8; 32];
        let mut solution_key = [0_u8; 32];
        hkdf.expand(b"plamenu:altcha:challenge-signature:v1", &mut challenge_key)
            .expect("32-byte HKDF output is valid");
        hkdf.expand(b"plamenu:altcha:solution-signature:v1", &mut solution_key)
            .expect("32-byte HKDF output is valid");
        let challenge_secret = URL_SAFE_NO_PAD.encode(challenge_key);
        let key_secret = URL_SAFE_NO_PAD.encode(solution_key);
        challenge_key.zeroize();
        solution_key.zeroize();
        Self {
            challenge_secret,
            key_secret,
            config,
            used: Mutex::new(HashMap::new()),
        }
    }

    /// Issues a fresh signed challenge. Generation performs one KDF operation;
    /// the counter search remains entirely in the browser's Web Workers.
    pub fn challenge(&self) -> altcha::Result<Challenge> {
        let now = now_epoch();
        let mut data = BTreeMap::new();
        data.insert("purpose".to_owned(), Value::String(PURPOSE.to_owned()));
        create_challenge(CreateChallengeOptions {
            algorithm: "PBKDF2/SHA-256".to_owned(),
            counter: Some(random_counter(
                self.config.min_counter,
                self.config.max_counter,
            )),
            cost: self.config.cost,
            data: Some(data),
            expires_at: Some(now.saturating_add(self.config.expires_seconds)),
            hmac_signature_secret: Some(self.challenge_secret.clone()),
            hmac_key_signature_secret: Some(self.key_secret.clone()),
            ..CreateChallengeOptions::default()
        })
    }

    /// Verifies and atomically consumes the widget's Base64-encoded payload.
    pub fn verify_and_consume(&self, encoded: &str) -> Result<(), VerifyError> {
        let encoded = encoded.trim();
        if encoded.is_empty() {
            return Err(VerifyError::Missing);
        }
        if encoded.len() > MAX_PAYLOAD_BYTES {
            return Err(VerifyError::Malformed);
        }
        let decoded = STANDARD
            .decode(encoded)
            .map_err(|_| VerifyError::Malformed)?;
        if decoded.len() > MAX_PAYLOAD_BYTES {
            return Err(VerifyError::Malformed);
        }
        let payload: Payload =
            serde_json::from_slice(&decoded).map_err(|_| VerifyError::Malformed)?;
        let parameters = &payload.challenge.parameters;
        if parameters
            .data
            .as_ref()
            .and_then(|data| data.get("purpose"))
            .and_then(Value::as_str)
            != Some(PURPOSE)
            || parameters.expires_at.is_none()
            || parameters.key_signature.is_none()
        {
            return Err(VerifyError::Invalid);
        }
        let signature = payload
            .challenge
            .signature
            .clone()
            .ok_or(VerifyError::Invalid)?;
        let result = verify_solution(VerifySolutionOptions {
            hmac_key_signature_secret: Some(self.key_secret.clone()),
            ..VerifySolutionOptions::new(
                &payload.challenge,
                &payload.solution,
                &self.challenge_secret,
            )
        })
        .map_err(|_| VerifyError::Invalid)?;
        if !result.verified {
            return Err(VerifyError::Invalid);
        }

        let now = now_epoch();
        let expires_at = parameters.expires_at.unwrap_or(now);
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        used.retain(|_, expiry| *expiry >= now);
        if used.contains_key(&signature) {
            return Err(VerifyError::Replayed);
        }
        if used.len() >= MAX_REPLAY_ENTRIES
            && let Some(oldest) = used
                .iter()
                .min_by_key(|(_, expiry)| **expiry)
                .map(|(signature, _)| signature.clone())
        {
            used.remove(&oldest);
        }
        used.insert(signature, expires_at);
        Ok(())
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_counter(min: u32, max: u32) -> u32 {
    let mut bytes = [0_u8; 4];
    getrandom::fill(&mut bytes).expect("OS entropy source failed");
    let width = max - min + 1;
    min + u32::from_le_bytes(bytes) % width
}

#[cfg(test)]
mod tests {
    use super::*;
    use altcha::{SolveChallengeOptions, solve_challenge};

    fn service() -> Altcha {
        Altcha::from_master(
            b"unit-test-master-secret-at-least-32-bytes",
            AltchaConfig {
                cost: 1,
                min_counter: 1,
                max_counter: 2,
                expires_seconds: 60,
            },
        )
    }

    fn solve(service: &Altcha) -> String {
        let challenge = service.challenge().unwrap();
        let solution = solve_challenge(SolveChallengeOptions::new(&challenge))
            .unwrap()
            .unwrap();
        STANDARD.encode(
            serde_json::to_vec(&Payload {
                challenge,
                solution,
            })
            .unwrap(),
        )
    }

    #[test]
    fn valid_proof_is_single_use() {
        let service = service();
        let payload = solve(&service);
        assert_eq!(service.verify_and_consume(&payload), Ok(()));
        assert_eq!(
            service.verify_and_consume(&payload),
            Err(VerifyError::Replayed)
        );
    }

    #[test]
    fn malformed_and_tampered_proofs_are_rejected() {
        let service = service();
        assert_eq!(service.verify_and_consume(""), Err(VerifyError::Missing));
        assert_eq!(
            service.verify_and_consume("not base64"),
            Err(VerifyError::Malformed)
        );
        let mut payload = solve(&service);
        payload.replace_range(0..1, "A");
        assert!(service.verify_and_consume(&payload).is_err());
    }
}
