//! FEP-8b32 object integrity proofs: `eddsa-jcs-2022` `DataIntegrityProof`s
//! over JCS-canonicalized (RFC 8785) activity JSON.
//!
//! The signed bytes are `SHA-256(JCS(proof config)) ‖ SHA-256(JCS(document))`
//! per the W3C `eddsa-jcs-2022` cryptosuite, where the proof config is the
//! `proof` object minus `proofValue` and the document is the object minus
//! `proof` (and any legacy LD `signature`). Mitra's earlier `jcs-eddsa-2022`
//! suite is byte-identical apart from the cryptosuite name, so verification
//! accepts both.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::multikey;

pub const DATA_INTEGRITY_PROOF: &str = "DataIntegrityProof";
pub const CRYPTOSUITE_EDDSA_JCS: &str = "eddsa-jcs-2022";
/// Mitra's pre-standard name for the same construction.
pub const CRYPTOSUITE_EDDSA_JCS_LEGACY: &str = "jcs-eddsa-2022";
pub const CRYPTOSUITE_MLDSA44_JCS: &str = "mldsa44-jcs-2024";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofAlgorithm {
    Ed25519,
    MlDsa44,
}

#[derive(Debug, Error)]
pub enum ProofError {
    #[error("document is not a JSON object")]
    NotAnObject,
    #[error("document already carries a proof")]
    AlreadySigned,
    #[error("document has no integrity proof")]
    NoProof,
    #[error("unsupported proof type or cryptosuite")]
    UnsupportedSuite,
    #[error("unsupported proof purpose")]
    UnsupportedPurpose,
    #[error("proof context is not a prefix of the document context")]
    InvalidProofContext,
    #[error("proof timestamp is not valid RFC 3339")]
    BadTimestamp,
    #[error("proof has expired")]
    Expired,
    #[error("proof creation time is too far in the future")]
    CreatedInFuture,
    #[error("proof has no verificationMethod")]
    NoVerificationMethod,
    #[error("proofValue is not a supported multibase string")]
    BadProofValue,
    #[error("proof key is not a usable Ed25519 multikey: {0}")]
    BadKey(#[from] multikey::MultikeyError),
    #[error("document cannot be canonicalized: {0}")]
    Canonicalize(#[from] serde_json::Error),
    #[error("proof signature is invalid")]
    Invalid,
}

/// `SHA-256(JCS(proof config)) ‖ SHA-256(JCS(document))` — the
/// `eddsa-jcs-2022` hash-data construction.
fn hash_data(document: &Value, proof_config: &Value) -> Result<[u8; 64], ProofError> {
    let canonical_config = serde_json_canonicalizer::to_vec(proof_config)?;
    let canonical_document = serde_json_canonicalizer::to_vec(document)?;
    let mut data = [0u8; 64];
    data[..32].copy_from_slice(&Sha256::digest(&canonical_config));
    data[32..].copy_from_slice(&Sha256::digest(&canonical_document));
    Ok(data)
}

/// The document as covered by a proof: without the proof itself, and without
/// any legacy LD `signature` block a forwarding server may have added.
fn unsecured(document: &Value) -> Result<Map<String, Value>, ProofError> {
    let mut map = document.as_object().ok_or(ProofError::NotAnObject)?.clone();
    map.remove("proof");
    map.remove("signature");
    Ok(map)
}

/// Signs `document` with an Ed25519 secret Multikey, returning a copy
/// carrying an `eddsa-jcs-2022` proof bound to `verification_method`
/// (`<actor>#ed25519-key`). `created` must be RFC 3339 with whole seconds
/// (`…Z`) — receivers that round-trip the timestamp through their own types
/// reproduce that form exactly.
pub fn sign_document(
    document: &Value,
    private_key_multibase: &str,
    verification_method: &str,
    created: &str,
) -> Result<Value, ProofError> {
    if document.get("proof").is_some() {
        return Err(ProofError::AlreadySigned);
    }
    let proof_config = json!({
        "type": DATA_INTEGRITY_PROOF,
        "cryptosuite": CRYPTOSUITE_EDDSA_JCS,
        "verificationMethod": verification_method,
        "proofPurpose": "assertionMethod",
        "created": created,
    });
    let secret = multikey::decode_ed25519_private(private_key_multibase)?;
    let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
    let unsecured_document = Value::Object(unsecured(document)?);
    let data = hash_data(&unsecured_document, &proof_config)?;
    let signature = ed25519_dalek::Signer::sign(&signing, &data);

    let mut proof = proof_config;
    proof["proofValue"] = Value::String(format!(
        "z{}",
        bs58::encode(signature.to_bytes()).into_string()
    ));
    let mut signed = document.clone();
    signed
        .as_object_mut()
        .ok_or(ProofError::NotAnObject)?
        .insert("proof".to_owned(), proof);
    Ok(signed)
}

/// Signs with ML-DSA-44 using the `mldsa44-jcs-2024` construction Plamenu
/// accepts from Mastodon 4.7-era peers. Local actor provisioning does not
/// enable this algorithm by default yet; exposing the complete primitive gives
/// protocol tests and future opt-in key policies one standards-identical path.
pub fn sign_document_ml_dsa_44(
    document: &Value,
    signing_key: &ml_dsa::SigningKey<ml_dsa::MlDsa44>,
    verification_method: &str,
    created: &str,
) -> Result<Value, ProofError> {
    use base64::Engine as _;
    use ml_dsa::Signer as _;

    if document.get("proof").is_some() {
        return Err(ProofError::AlreadySigned);
    }
    let unsecured_document = Value::Object(unsecured(document)?);
    let proof_config = json!({
        "@context": unsecured_document.get("@context").cloned().unwrap_or(Value::Null),
        "type": DATA_INTEGRITY_PROOF,
        "cryptosuite": CRYPTOSUITE_MLDSA44_JCS,
        "verificationMethod": verification_method,
        "proofPurpose": "assertionMethod",
        "created": created,
    });
    let data = hash_data(&unsecured_document, &proof_config)?;
    let signature: ml_dsa::Signature<ml_dsa::MlDsa44> = signing_key.sign(&data);
    let mut proof = proof_config;
    proof["proofValue"] = Value::String(format!(
        "u{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.encode())
    ));
    let mut signed = document.clone();
    signed
        .as_object_mut()
        .ok_or(ProofError::NotAnObject)?
        .insert("proof".to_owned(), proof);
    Ok(signed)
}

/// A structurally-validated inbound proof, ready to be checked against a
/// public key. Mirrors `PreparedVerification` for HTTP signatures: the caller
/// resolves [`Self::verification_method`] to the actor's Ed25519 Multikey and
/// then calls [`Self::verify`].
pub struct PreparedProof {
    verification_method: String,
    signature: Vec<u8>,
    data: [u8; 64],
    algorithm: ProofAlgorithm,
}

impl PreparedProof {
    /// Validates the proof's shape and computes the signed hash data from the
    /// *raw* proof JSON (minus `proofValue`), so senders' timestamp formats
    /// and extension fields survive canonicalization untouched.
    pub fn from_document(document: &Value) -> Result<Self, ProofError> {
        Self::from_document_at(document, time::OffsetDateTime::now_utc())
    }

    /// Deterministic form of [`Self::from_document`] used by boundary tests.
    pub fn from_document_at(
        document: &Value,
        now: time::OffsetDateTime,
    ) -> Result<Self, ProofError> {
        let proof = document.get("proof").ok_or(ProofError::NoProof)?;
        let proof_map = proof.as_object().ok_or(ProofError::NoProof)?;

        let kind = proof_map.get("type").and_then(Value::as_str);
        let suite = proof_map.get("cryptosuite").and_then(Value::as_str);
        let algorithm = match (kind, suite) {
            (
                Some(DATA_INTEGRITY_PROOF),
                Some(CRYPTOSUITE_EDDSA_JCS | CRYPTOSUITE_EDDSA_JCS_LEGACY),
            ) => ProofAlgorithm::Ed25519,
            (Some(DATA_INTEGRITY_PROOF), Some(CRYPTOSUITE_MLDSA44_JCS)) => ProofAlgorithm::MlDsa44,
            _ => return Err(ProofError::UnsupportedSuite),
        };
        if kind != Some(DATA_INTEGRITY_PROOF) {
            return Err(ProofError::UnsupportedSuite);
        }
        match proof_map.get("proofPurpose").and_then(Value::as_str) {
            Some("assertionMethod") => {}
            _ => return Err(ProofError::UnsupportedPurpose),
        }
        for field in ["created", "expires"] {
            if let Some(value) = proof_map.get(field) {
                let raw = value.as_str().ok_or(ProofError::BadTimestamp)?;
                let parsed = time::OffsetDateTime::parse(
                    raw,
                    &time::format_description::well_known::Rfc3339,
                )
                .map_err(|_| ProofError::BadTimestamp)?;
                match field {
                    "created" if parsed > now + time::Duration::minutes(5) => {
                        return Err(ProofError::CreatedInFuture);
                    }
                    "expires" if parsed <= now => return Err(ProofError::Expired),
                    _ => {}
                }
            }
        }
        let verification_method = proof_map
            .get("verificationMethod")
            .and_then(Value::as_str)
            // FEP-ef61 portable objects are self-certifying: their proof
            // names the did:key authority directly instead of an HTTP actor
            // verification method. Ownership is still the caller's job.
            .filter(|m| {
                m.starts_with("https://") || m.starts_with("http://") || m.starts_with("did:key:z")
            })
            .ok_or(ProofError::NoVerificationMethod)?
            .to_owned();
        let signature = proof_map
            .get("proofValue")
            .and_then(Value::as_str)
            .and_then(|value| multikey::decode_multibase(value).ok())
            .ok_or(ProofError::BadProofValue)?;

        let mut proof_config = proof_map.clone();
        proof_config.remove("proofValue");
        let mut unsecured_document = Value::Object(unsecured(document)?);
        if algorithm == ProofAlgorithm::Ed25519
            && let Some(proof_context) = proof_config.get("@context")
        {
            let proof_context = proof_context
                .as_array()
                .ok_or(ProofError::InvalidProofContext)?;
            let document_context = unsecured_document
                .get("@context")
                .and_then(Value::as_array)
                .ok_or(ProofError::InvalidProofContext)?;
            if document_context.get(..proof_context.len()) != Some(proof_context.as_slice()) {
                return Err(ProofError::InvalidProofContext);
            }
            unsecured_document["@context"] = Value::Array(proof_context.clone());
        }
        // The ML-DSA proof-configuration algorithm requires the unsecured
        // document's context to be copied into the proof config before JCS.
        // This deliberately differs from eddsa-jcs-2022 and matches the W3C
        // 2026 working draft and Mastodon 4.7 implementation.
        if algorithm == ProofAlgorithm::MlDsa44 {
            proof_config.insert(
                "@context".to_owned(),
                unsecured_document
                    .get("@context")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        let data = hash_data(&unsecured_document, &Value::Object(proof_config))?;
        Ok(Self {
            verification_method,
            signature,
            data,
            algorithm,
        })
    }

    /// The key the proof claims to be signed with; same-origin and ownership
    /// checks against the activity's actor are the caller's job.
    #[must_use]
    pub fn verification_method(&self) -> &str {
        &self.verification_method
    }

    #[must_use]
    pub const fn algorithm(&self) -> ProofAlgorithm {
        self.algorithm
    }

    /// Checks the proof against an Ed25519 public Multikey (`z6Mk…`).
    pub fn verify(&self, public_key_multibase: &str) -> Result<(), ProofError> {
        if self.algorithm != ProofAlgorithm::Ed25519 {
            return Err(ProofError::BadKey(multikey::MultikeyError::NotEd25519));
        }
        let key_bytes = multikey::decode_ed25519_public(public_key_multibase)?;
        let key =
            ed25519_dalek::VerifyingKey::from_bytes(&key_bytes).map_err(|_| ProofError::Invalid)?;
        let signature = ed25519_dalek::Signature::from_slice(&self.signature)
            .map_err(|_| ProofError::Invalid)?;
        ed25519_dalek::Verifier::verify(&key, &self.data, &signature)
            .map_err(|_| ProofError::Invalid)
    }

    /// Checks an `mldsa44-jcs-2024` proof against an ML-DSA-44 public
    /// Multikey (`u…`, multicodec `0x1210`).
    pub fn verify_ml_dsa_44(&self, public_key_multibase: &str) -> Result<(), ProofError> {
        if self.algorithm != ProofAlgorithm::MlDsa44 {
            return Err(ProofError::UnsupportedSuite);
        }
        let raw = multikey::decode_ml_dsa_44_public(public_key_multibase)?;
        let encoded = ml_dsa::EncodedVerifyingKey::<ml_dsa::MlDsa44>::try_from(raw.as_slice())
            .map_err(|_| ProofError::Invalid)?;
        let key = ml_dsa::VerifyingKey::<ml_dsa::MlDsa44>::decode(&encoded);
        let signature = ml_dsa::Signature::<ml_dsa::MlDsa44>::try_from(self.signature.as_slice())
            .map_err(|_| ProofError::Invalid)?;
        ml_dsa::Verifier::verify(&key, &self.data, &signature).map_err(|_| ProofError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::keys::generate_ed25519_keypair;

    /// The FEP-8b32 test vector key pair.
    const VECTOR_SECRET: &str = "z3u2en7t5LR2WtQH5PfFqMqwVHBeXouLzo6haApm8XHqvjxq";
    const VECTOR_PUBLIC: &str = "z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2";

    #[test]
    fn sign_verify_roundtrip() {
        let pair = generate_ed25519_keypair();
        let activity = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": "https://plamenu.test/users/alice/statuses/1/activity",
            "type": "Create",
            "actor": "https://plamenu.test/users/alice",
            "object": {"type": "Note", "content": "hi"},
        });
        let signed = sign_document(
            &activity,
            &pair.private_multibase,
            "https://plamenu.test/users/alice#ed25519-key",
            "2026-07-11T00:00:00Z",
        )
        .unwrap();
        assert_eq!(signed["proof"]["cryptosuite"], json!(CRYPTOSUITE_EDDSA_JCS));
        assert_eq!(signed["proof"]["proofPurpose"], json!("assertionMethod"));

        let prepared = PreparedProof::from_document(&signed).unwrap();
        assert_eq!(
            prepared.verification_method(),
            "https://plamenu.test/users/alice#ed25519-key"
        );
        prepared.verify(&pair.public_multibase).unwrap();

        // A different key must not verify.
        let other = generate_ed25519_keypair();
        assert!(matches!(
            PreparedProof::from_document(&signed)
                .unwrap()
                .verify(&other.public_multibase),
            Err(ProofError::Invalid)
        ));
    }

    #[test]
    fn verifies_portable_did_key_proof() {
        let pair = generate_ed25519_keypair();
        let did = format!("did:key:{}", pair.public_multibase);
        let document = json!({
            "@context": [
                crate::AS_CONTEXT,
                "https://w3id.org/security/data-integrity/v1"
            ],
            "id": format!("ap://{did}/objects/1"),
            "type": "Note",
            "attributedTo": format!("ap://{did}/actors/1"),
        });
        let signed = sign_document(
            &document,
            &pair.private_multibase,
            &did,
            "2025-01-01T00:00:00Z",
        )
        .unwrap();
        let prepared = PreparedProof::from_document(&signed).unwrap();
        assert_eq!(prepared.verification_method(), did);
        prepared.verify(&pair.public_multibase).unwrap();
    }

    #[test]
    fn ml_dsa_44_roundtrip_and_tamper_rejection() {
        use ml_dsa::{Generate, Keypair};

        let signing = ml_dsa::SigningKey::<ml_dsa::MlDsa44>::generate();
        let public = signing.verifying_key().encode();
        let public: [u8; crate::multikey::ML_DSA_44_PUBLIC_LEN] =
            public.as_slice().try_into().unwrap();
        let multikey = crate::multikey::encode_ml_dsa_44_public(&public);
        let document = json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/data-integrity/v2"
            ],
            "id": "https://remote.example/activities/44",
            "type": "Create",
            "actor": "https://remote.example/actors/alice",
            "object": {"type": "Note", "content": "post-quantum"},
        });
        let mut document = sign_document_ml_dsa_44(
            &document,
            &signing,
            "https://remote.example/actors/alice#mldsa-44",
            "2026-08-12T00:00:00Z",
        )
        .unwrap();

        let prepared = PreparedProof::from_document(&document).unwrap();
        assert_eq!(prepared.algorithm(), ProofAlgorithm::MlDsa44);
        prepared.verify_ml_dsa_44(&multikey).unwrap();
        assert!(matches!(
            prepared.verify(&multikey),
            Err(ProofError::BadKey(_))
        ));

        document["object"]["content"] = json!("tampered");
        assert!(matches!(
            PreparedProof::from_document(&document)
                .unwrap()
                .verify_ml_dsa_44(&multikey),
            Err(ProofError::Invalid)
        ));
    }

    #[test]
    fn rejects_expired_and_malformed_proof_times() {
        let pair = generate_ed25519_keypair();
        let activity = json!({"id": "https://example/a", "type": "Like"});
        let mut signed = sign_document(
            &activity,
            &pair.private_multibase,
            "https://example/actor#key",
            "2026-08-12T00:00:00Z",
        )
        .unwrap();
        signed["proof"]["expires"] = json!("2026-08-13T00:00:00Z");
        let before = time::OffsetDateTime::parse(
            "2026-08-12T23:59:59Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        PreparedProof::from_document_at(&signed, before).unwrap();
        let at = time::OffsetDateTime::parse(
            "2026-08-13T00:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert!(matches!(
            PreparedProof::from_document_at(&signed, at),
            Err(ProofError::Expired)
        ));
        signed["proof"]["expires"] = json!("not-a-time");
        assert!(matches!(
            PreparedProof::from_document_at(&signed, before),
            Err(ProofError::BadTimestamp)
        ));
        signed["proof"]["expires"] = json!("2026-08-13T00:00:00Z");
        signed["proof"]["created"] = json!("2026-08-12T00:05:01Z");
        let now = time::OffsetDateTime::parse(
            "2026-08-12T00:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert!(matches!(
            PreparedProof::from_document_at(&signed, now),
            Err(ProofError::CreatedInFuture)
        ));
        signed["proof"]["created"] = json!("2026-08-12T00:05:00Z");
        PreparedProof::from_document_at(&signed, now).unwrap();
        signed["proof"]["created"] = json!(1234);
        assert!(matches!(
            PreparedProof::from_document_at(&signed, now),
            Err(ProofError::BadTimestamp)
        ));
    }

    #[test]
    fn eddsa_proof_context_must_be_a_document_context_prefix() {
        let pair = generate_ed25519_keypair();
        let document = json!({
            "@context": ["https://www.w3.org/ns/activitystreams", "https://example/extra"],
            "id": "https://example/activity",
            "type": "Like",
        });
        let mut signed = sign_document(
            &document,
            &pair.private_multibase,
            "https://example/actor#key",
            "2026-08-12T00:00:00Z",
        )
        .unwrap();
        signed["proof"]["@context"] = json!(["https://example/not-a-prefix"]);
        assert!(matches!(
            PreparedProof::from_document(&signed),
            Err(ProofError::InvalidProofContext)
        ));
    }

    #[test]
    fn tampered_document_fails() {
        let pair = generate_ed25519_keypair();
        let activity = json!({
            "id": "https://plamenu.test/a/1",
            "type": "Create",
            "actor": "https://plamenu.test/users/alice",
            "object": {"type": "Note", "content": "hi"},
        });
        let mut signed = sign_document(
            &activity,
            &pair.private_multibase,
            "https://plamenu.test/users/alice#ed25519-key",
            "2026-07-11T00:00:00Z",
        )
        .unwrap();
        signed["object"]["content"] = json!("tampered");
        assert!(matches!(
            PreparedProof::from_document(&signed)
                .unwrap()
                .verify(&pair.public_multibase),
            Err(ProofError::Invalid)
        ));
    }

    /// The FEP-8b32 test vector (fep-8b32.feature): our verifier must accept
    /// the exact signed document other implementations produce. The vector's
    /// proof carries an `@context` we never emit — the raw-JSON config path
    /// must cover it transparently.
    #[test]
    #[allow(clippy::unreadable_literal, reason = "the vector's exact text")]
    fn verifies_fep_8b32_test_vector() {
        let signed = json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/data-integrity/v2"
            ],
            "id": "https://server.example/activities/1",
            "type": "Create",
            "actor": "https://server.example/users/alice",
            "object": {
                "id": "https://server.example/objects/1",
                "type": "Note",
                "attributedTo": "https://server.example/users/alice",
                "content": "Hello world",
                "location": {
                    "type": "Place",
                    "longitude": -71.184902,
                    "latitude": 25.273962
                }
            },
            "proof": {
                "@context": [
                    "https://www.w3.org/ns/activitystreams",
                    "https://w3id.org/security/data-integrity/v2"
                ],
                "type": "DataIntegrityProof",
                "cryptosuite": "eddsa-jcs-2022",
                "verificationMethod": "https://server.example/users/alice#ed25519-key",
                "proofPurpose": "assertionMethod",
                "proofValue": "z42ffGu6AUKPCFcFPiabmUvnGLPJzC7e4DGWC52NUasSSH37UMa9c58tdgVszUcZfytxa4fQ5TYHaJENCxUDe9SdL",
                "created": "2023-02-24T23:36:38Z"
            }
        });
        let prepared = PreparedProof::from_document(&signed).unwrap();
        assert_eq!(
            prepared.verification_method(),
            "https://server.example/users/alice#ed25519-key"
        );
        prepared.verify(VECTOR_PUBLIC).unwrap();
    }

    /// Signing the FEP-8b32 vector's document with its key and timestamp must
    /// reproduce the vector's proofValue, modulo the proof `@context` the
    /// vector includes (we sign activities without one, like Mitra): with the
    /// context manually spliced into the config, the bytes must match.
    #[test]
    #[allow(clippy::unreadable_literal, reason = "the vector's exact text")]
    fn reproduces_fep_8b32_vector_signature() {
        let document = json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/data-integrity/v2"
            ],
            "id": "https://server.example/activities/1",
            "type": "Create",
            "actor": "https://server.example/users/alice",
            "object": {
                "id": "https://server.example/objects/1",
                "type": "Note",
                "attributedTo": "https://server.example/users/alice",
                "content": "Hello world",
                "location": {
                    "type": "Place",
                    "longitude": -71.184902,
                    "latitude": 25.273962
                }
            }
        });
        // Sign with a proof config carrying the document @context, exactly
        // like the vector, then check the signature bytes.
        let proof_config = json!({
            "@context": document["@context"],
            "type": DATA_INTEGRITY_PROOF,
            "cryptosuite": CRYPTOSUITE_EDDSA_JCS,
            "verificationMethod": "https://server.example/users/alice#ed25519-key",
            "proofPurpose": "assertionMethod",
            "created": "2023-02-24T23:36:38Z",
        });
        let secret = crate::multikey::decode_ed25519_private(VECTOR_SECRET).unwrap();
        let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
        let data = hash_data(&document, &proof_config).unwrap();
        let signature = ed25519_dalek::Signer::sign(&signing, &data);
        assert_eq!(
            format!("z{}", bs58::encode(signature.to_bytes()).into_string()),
            "z42ffGu6AUKPCFcFPiabmUvnGLPJzC7e4DGWC52NUasSSH37UMa9c58tdgVszUcZfytxa4fQ5TYHaJENCxUDe9SdL"
        );
    }

    /// Mitra's legacy `jcs-eddsa-2022` cryptosuite name is byte-identical to
    /// `eddsa-jcs-2022` — a proof differing only in the name must verify.
    #[test]
    fn accepts_legacy_cryptosuite_name() {
        let pair = generate_ed25519_keypair();
        let activity = json!({
            "id": "https://mitra.example/a/1",
            "type": "Like",
            "actor": "https://mitra.example/users/erin",
            "object": "https://plamenu.test/users/alice/statuses/1",
        });
        let mut signed = sign_document(
            &activity,
            &pair.private_multibase,
            "https://mitra.example/users/erin#ed25519-key",
            "2026-07-11T00:00:00Z",
        )
        .unwrap();
        // Rewriting the cryptosuite name changes the proof config, so re-sign
        // manually with the legacy name.
        let mut proof_config = signed["proof"].as_object().unwrap().clone();
        proof_config.remove("proofValue");
        proof_config.insert(
            "cryptosuite".to_owned(),
            json!(CRYPTOSUITE_EDDSA_JCS_LEGACY),
        );
        let secret = crate::multikey::decode_ed25519_private(&pair.private_multibase).unwrap();
        let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
        let data = hash_data(&activity, &Value::Object(proof_config.clone())).unwrap();
        let signature = ed25519_dalek::Signer::sign(&signing, &data);
        proof_config.insert(
            "proofValue".to_owned(),
            json!(format!(
                "z{}",
                bs58::encode(signature.to_bytes()).into_string()
            )),
        );
        signed["proof"] = Value::Object(proof_config);

        PreparedProof::from_document(&signed)
            .unwrap()
            .verify(&pair.public_multibase)
            .unwrap();
    }

    /// A legacy LD `signature` block (Mastodon's `RsaSignature2017`, attached by
    /// forwarding relays) is outside the proof's coverage and must not break
    /// verification.
    #[test]
    fn ignores_ld_signature_block() {
        let pair = generate_ed25519_keypair();
        let activity = json!({
            "id": "https://plamenu.test/a/1",
            "type": "Announce",
            "actor": "https://plamenu.test/users/alice",
            "object": "https://remote.example/note/1",
        });
        let mut signed = sign_document(
            &activity,
            &pair.private_multibase,
            "https://plamenu.test/users/alice#ed25519-key",
            "2026-07-11T00:00:00Z",
        )
        .unwrap();
        signed["signature"] = json!({
            "type": "RsaSignature2017",
            "creator": "https://relay.example/actor#main-key",
            "signatureValue": "opaque",
        });
        PreparedProof::from_document(&signed)
            .unwrap()
            .verify(&pair.public_multibase)
            .unwrap();
    }

    #[test]
    fn rejects_malformed_proofs() {
        let no_proof = json!({"id": "https://x.example/1"});
        assert!(matches!(
            PreparedProof::from_document(&no_proof),
            Err(ProofError::NoProof)
        ));

        let bad_suite = json!({
            "id": "https://x.example/1",
            "proof": {
                "type": "DataIntegrityProof",
                "cryptosuite": "ecdsa-rdfc-2019",
                "verificationMethod": "https://x.example/1#key",
                "proofPurpose": "assertionMethod",
                "proofValue": "z3sig",
                "created": "2026-07-11T00:00:00Z",
            },
        });
        assert!(matches!(
            PreparedProof::from_document(&bad_suite),
            Err(ProofError::UnsupportedSuite)
        ));

        let unsupported_method = json!({
            "id": "https://x.example/1",
            "proof": {
                "type": "DataIntegrityProof",
                "cryptosuite": "eddsa-jcs-2022",
                "verificationMethod": "urn:uuid:2fcbceda-26bb-45d3-bab5-8dd02cc7c1a9",
                "proofPurpose": "assertionMethod",
                "proofValue": "z3sig",
                "created": "2026-07-11T00:00:00Z",
            },
        });
        assert!(matches!(
            PreparedProof::from_document(&unsupported_method),
            Err(ProofError::NoVerificationMethod)
        ));

        let bad_purpose = json!({
            "id": "https://x.example/1",
            "proof": {
                "type": "DataIntegrityProof",
                "cryptosuite": "eddsa-jcs-2022",
                "verificationMethod": "https://x.example/1#key",
                "proofPurpose": "capabilityInvocation",
                "proofValue": "z3sig",
                "created": "2026-07-11T00:00:00Z",
            },
        });
        assert!(matches!(
            PreparedProof::from_document(&bad_purpose),
            Err(ProofError::UnsupportedPurpose)
        ));
    }

    #[test]
    fn refuses_to_double_sign() {
        let pair = generate_ed25519_keypair();
        let activity = json!({"id": "https://plamenu.test/a/1", "type": "Create"});
        let signed = sign_document(
            &activity,
            &pair.private_multibase,
            "https://plamenu.test/users/alice#ed25519-key",
            "2026-07-11T00:00:00Z",
        )
        .unwrap();
        assert!(matches!(
            sign_document(
                &signed,
                &pair.private_multibase,
                "https://plamenu.test/users/alice#ed25519-key",
                "2026-07-11T00:00:00Z",
            ),
            Err(ProofError::AlreadySigned)
        ));
    }
}
