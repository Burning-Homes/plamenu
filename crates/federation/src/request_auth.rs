//! Dialect dispatch for inbound signed requests: RFC 9421 when the request
//! carries `Signature-Input` (Mastodon's rule — its mere presence selects
//! the code path), draft-cavage otherwise.

use std::time::SystemTime;

use crate::rfc9421::{PreparedRfc9421, RequestFacts};
use crate::signature::{PreparedVerification, SignatureError};

/// A structurally-validated inbound signature of either dialect. The caller
/// resolves [`Self::key_id`] to an actor and verifies against the key type
/// the signature asks for.
pub enum PreparedRequestAuth {
    Cavage(PreparedVerification),
    Rfc9421(PreparedRfc9421),
}

impl PreparedRequestAuth {
    /// Validates a signed POST (inbox delivery).
    pub fn from_post_request(
        facts: &RequestFacts<'_>,
        body: &[u8],
        now: SystemTime,
    ) -> Result<Self, SignatureError> {
        if facts.headers.contains_key("signature-input") {
            Ok(Self::Rfc9421(PreparedRfc9421::from_request(
                facts,
                Some(body),
                now,
            )?))
        } else {
            Ok(Self::Cavage(PreparedVerification::from_request(
                facts.method,
                facts.path_and_query,
                facts.headers,
                body,
                now,
            )?))
        }
    }

    /// Validates a signed GET (authorized fetch).
    pub fn from_get_request(
        facts: &RequestFacts<'_>,
        now: SystemTime,
    ) -> Result<Self, SignatureError> {
        Self::from_bodyless_request(facts, now, false)
    }

    /// Validates a signed GET/DELETE without a body. The optional path-only
    /// fallback is used by FEP-ae97 clients that omit query parameters from
    /// draft-cavage `(request-target)` signatures.
    pub fn from_bodyless_request(
        facts: &RequestFacts<'_>,
        now: SystemTime,
        allow_query_omission: bool,
    ) -> Result<Self, SignatureError> {
        if facts.headers.contains_key("signature-input") {
            Ok(Self::Rfc9421(PreparedRfc9421::from_request(
                facts, None, now,
            )?))
        } else {
            Ok(Self::Cavage(PreparedVerification::from_bodyless_request(
                facts.method,
                facts.path_and_query,
                facts.headers,
                now,
                allow_query_omission,
            )?))
        }
    }

    /// Whether this request was signed with RFC 9421 (its `Signature-Input`
    /// header selected the RFC 9421 path). A verified RFC 9421 inbound is the
    /// only positive signal that a peer speaks the dialect.
    #[must_use]
    pub fn is_rfc9421(&self) -> bool {
        matches!(self, Self::Rfc9421(_))
    }

    /// The keyId the sender claims.
    #[must_use]
    pub fn key_id(&self) -> &str {
        match self {
            Self::Cavage(prepared) => prepared.key_id(),
            Self::Rfc9421(prepared) => prepared.key_id(),
        }
    }

    /// Whether verification needs the actor's Ed25519 Multikey instead of
    /// the RSA PEM (RFC 9421 with `alg="ed25519"` or an `#ed25519-key`
    /// keyid; draft-cavage is always RSA in the fediverse).
    #[must_use]
    pub fn wants_ed25519(&self) -> bool {
        match self {
            Self::Cavage(_) => false,
            Self::Rfc9421(prepared) => prepared.wants_ed25519(),
        }
    }

    /// Verifies against the actor's RSA public key PEM.
    pub fn verify_rsa_pem(&self, public_key_pem: &str) -> Result<(), SignatureError> {
        match self {
            Self::Cavage(prepared) => prepared.verify_pem(public_key_pem),
            Self::Rfc9421(prepared) => prepared.verify_rsa_pem(public_key_pem),
        }
    }

    /// Verifies against the actor's Ed25519 public Multikey; a draft-cavage
    /// signature never matches one.
    pub fn verify_ed25519_multikey(&self, multikey: &str) -> Result<(), SignatureError> {
        match self {
            Self::Cavage(_) => Err(SignatureError::Invalid),
            Self::Rfc9421(prepared) => prepared.verify_ed25519_multikey(multikey),
        }
    }
}
