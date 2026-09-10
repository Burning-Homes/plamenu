//! FEP-c390 identity statements using Ed25519 `did:key` identifiers.

use serde_json::Value;
use time::OffsetDateTime;

use crate::proof::PreparedProof;

pub const MAX_PROOFS: usize = 10;
pub const MAX_PROOF_BYTES: usize = 16 * 1024;

/// Verify both the signature and its binding to this actor. No network key
/// resolution is involved: the subject contains the public key itself.
pub fn verify<'a>(
    document: &'a Value,
    actor_id: &str,
    now: OffsetDateTime,
) -> Result<&'a str, &'static str> {
    if serde_json::to_vec(document).map_or(true, |v| v.len() > MAX_PROOF_BYTES) {
        return Err("Identity statement exceeds 16 KiB");
    }
    if document["type"] != "VerifiableIdentityStatement"
        || document["alsoKnownAs"].as_str() != Some(actor_id)
    {
        return Err("Identity statement must name this account's actor ID");
    }
    let subject = document["subject"].as_str().ok_or("Missing subject DID")?;
    let key = subject
        .strip_prefix("did:key:")
        .ok_or("Expected an Ed25519 did:key")?;
    crate::multikey::decode_ed25519_public(key).map_err(|_| "Invalid Ed25519 did:key")?;
    let proof = PreparedProof::from_document_at(document, now)
        .map_err(|_| "Invalid or expired identity proof")?;
    // The bare DID is used in the FEP example; Mitra uses the canonical
    // did:key verification-method fragment. No arbitrary DID URL is accepted.
    if proof.verification_method() != subject
        && proof.verification_method() != format!("{subject}#{key}")
    {
        return Err("Proof verification method does not match its subject");
    }
    proof
        .verify(key)
        .map_err(|_| "Invalid identity signature")?;
    Ok(subject)
}

/// Preserve the original signed documents, discard invalid attachments and
/// duplicates, and bound signature work even for hostile actor documents.
#[must_use]
pub fn verified(documents: &[Value], actor_id: &str, now: OffsetDateTime) -> Vec<Value> {
    let mut subjects = std::collections::HashSet::new();
    documents
        .iter()
        .filter(|d| d["type"] == "VerifiableIdentityStatement")
        .take(MAX_PROOFS)
        .filter(|d| verify(d, actor_id, now).is_ok_and(|s| subjects.insert(s.to_owned())))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ACTOR: &str = "https://server.example/users/alice";
    const KEY: &str = "z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2";
    const SECRET: &str = "z3u2en7t5LR2WtQH5PfFqMqwVHBeXouLzo6haApm8XHqvjxq";

    fn statement(fragment: &str) -> Value {
        let did = format!("did:key:{KEY}");
        crate::proof::sign_document(
            &json!({"type":"VerifiableIdentityStatement", "subject":did, "alsoKnownAs":ACTOR}),
            SECRET,
            &format!("{did}{fragment}"),
            "2023-02-24T23:36:38Z",
        )
        .unwrap()
    }

    #[test]
    fn mitra_vector_and_bare_did() {
        let proof = statement(&format!("#{KEY}"));
        assert_eq!(
            proof["proof"]["proofValue"],
            "zFQMTB8kZ1vExLhBUAGe4r3sc37onbdW8m3tdgsHYugh99Khzx87TbthqpSLcq45agip25v1mBvYW8u2GMKSMbpk"
        );
        for doc in [proof, statement("")] {
            assert!(verify(&doc, ACTOR, OffsetDateTime::now_utc()).is_ok());
        }
    }

    #[test]
    fn rejects_wrong_binding_tampering_and_lifecycle() {
        let now = OffsetDateTime::now_utc();
        let valid = statement("");
        assert!(verify(&valid, "https://other.example/users/alice", now).is_err());
        assert!(verify(&statement("#other"), ACTOR, now).is_err());
        for (pointer, value) in [
            ("/subject", json!("did:key:zInvalid")),
            ("/proof/proofValue", json!("z123")),
            ("/proof/proofPurpose", json!("authentication")),
            ("/proof/cryptosuite", json!("unknown")),
            ("/proof/created", json!("2999-01-01T00:00:00Z")),
        ] {
            let mut bad = valid.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(verify(&bad, ACTOR, now).is_err(), "{pointer}");
        }
        let mut expired = valid.clone();
        expired["proof"]["expires"] = json!("2020-01-01T00:00:00Z");
        assert!(verify(&expired, ACTOR, now).is_err());
        let mut oversized = valid.clone();
        oversized["extra"] = json!("x".repeat(MAX_PROOF_BYTES));
        assert!(verify(&oversized, ACTOR, now).is_err());
        assert_eq!(
            verified(&[valid.clone(), valid.clone(), expired], ACTOR, now),
            vec![valid]
        );
    }
}
