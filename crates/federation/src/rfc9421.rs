//! RFC 9421 HTTP Message Signatures (with RFC 9530 `Content-Digest`).
//!
//! Outbound: signs POSTs with `rsa-v1_5-sha256` over
//! `("@method" "@target-uri" "content-digest");created=…;keyid=…;alg=…` —
//! exactly the strength Mastodon's verifier demands (`SignedRequest::
//! HttpMessageSignature#verify_signature_strength!`). Inbound: verifies both
//! RSA and Ed25519 signatures over a superset of derived components.
//!
//! Deliberately hand-rolled structured-field handling: only the dictionary/
//! inner-list subset RFC 9421 uses, with the `@signature-params` line taken
//! *verbatim* from the received `Signature-Input` member so re-serialization
//! differences can never break the signature base.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::HeaderMap;
use rsa::RsaPublicKey;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs1v15::Signature as RsaSignature;
use rsa::pkcs8::DecodePublicKey;
use rsa::sha2::Sha256 as RsaSha256;
use rsa::signature::Verifier;
use sha2::{Digest, Sha256};

use crate::signature::{RequestSigner, SignatureError};

/// Mastodon's acceptance window for draft-cavage `Date` headers, applied to
/// the `created` parameter the same way.
const MAX_AGE: Duration = Duration::from_hours(12);
const MAX_CLOCK_SKEW: Duration = Duration::from_hours(1);

/// Computes the RFC 9530 `Content-Digest` header value for a request body.
#[must_use]
pub fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", BASE64.encode(Sha256::digest(body)))
}

/// Whether a received `Content-Digest` header matches the body: the
/// dictionary must carry a `sha-256` member (Mastodon's rule — sha-512-only
/// senders are rejected there too) whose byte-sequence value is the body's
/// SHA-256.
#[must_use]
pub fn content_digest_matches(header_value: &str, body: &[u8]) -> bool {
    dict_members(header_value).any(|(key, value)| {
        key == "sha-256"
            && value
                .strip_prefix(':')
                .and_then(|v| v.strip_suffix(':'))
                .and_then(|v| BASE64.decode(v).ok())
                .is_some_and(|decoded| decoded == Sha256::digest(body).as_slice())
    })
}

/// Headers attached to an RFC 9421-signed POST.
#[derive(Debug, Clone)]
pub struct SignedPostRfc9421 {
    pub date: String,
    pub content_digest: String,
    pub signature_input: String,
    pub signature: String,
}

/// Headers attached to an RFC 9421-signed GET (no body, so no
/// `Content-Digest`).
#[derive(Debug, Clone)]
pub struct SignedGetRfc9421 {
    pub date: String,
    pub signature_input: String,
    pub signature: String,
}

impl RequestSigner {
    /// Signs a POST of `body` to the absolute `target_uri` at time `now`
    /// with `rsa-v1_5-sha256` under the label `sig1`.
    #[must_use]
    pub fn sign_post_rfc9421(
        &self,
        target_uri: &str,
        body: &[u8],
        now: SystemTime,
    ) -> SignedPostRfc9421 {
        let created = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        let digest = content_digest(body);
        let params = format!(
            "(\"@method\" \"@target-uri\" \"content-digest\");created={created};keyid=\"{}\";alg=\"rsa-v1_5-sha256\"",
            self.key_id(),
        );
        let base = format!(
            "\"@method\": POST\n\"@target-uri\": {target_uri}\n\"content-digest\": {digest}\n\"@signature-params\": {params}",
        );
        let signature = BASE64.encode(self.sign_bytes(base.as_bytes()));
        SignedPostRfc9421 {
            date: httpdate::fmt_http_date(now),
            content_digest: digest,
            signature_input: format!("sig1={params}"),
            signature: format!("sig1=:{signature}:"),
        }
    }

    /// Signs a GET of the absolute `target_uri` at time `now` with the
    /// actor's Ed25519 key, under the FEP-521a `#ed25519-key` id.
    ///
    /// This is the fallback dialect for a peer that answered our draft-cavage
    /// GET `401`: the covered set is `("@method" "@target-uri")` with
    /// `created`, exactly what Mastodon's
    /// `SignedRequest::HttpMessageSignature#verify_signature_strength!`
    /// demands of a body-less request. `None` when the actor has no Ed25519
    /// key — there is nothing to fall back to.
    #[must_use]
    pub fn sign_get_rfc9421_ed25519(
        &self,
        target_uri: &str,
        now: SystemTime,
    ) -> Option<SignedGetRfc9421> {
        let signer = self.ed25519()?;
        let created = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        let params = format!(
            "(\"@method\" \"@target-uri\");created={created};keyid=\"{}\";alg=\"ed25519\"",
            signer.key_id,
        );
        let base = format!(
            "\"@method\": GET\n\"@target-uri\": {target_uri}\n\"@signature-params\": {params}",
        );
        let signature = BASE64
            .encode(ed25519_dalek::Signer::sign(&signer.signing_key, base.as_bytes()).to_bytes());
        Some(SignedGetRfc9421 {
            date: httpdate::fmt_http_date(now),
            signature_input: format!("sig1={params}"),
            signature: format!("sig1=:{signature}:"),
        })
    }
}

/// Splits a structured-field dictionary into `(key, raw member value)`
/// pairs, honoring quoted strings when scanning for top-level commas. Raw
/// values keep their parameters (`("a" "b");p=1` stays intact).
fn dict_members(value: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = value.trim();
    std::iter::from_fn(move || {
        while let Some(stripped) = rest.strip_prefix(',') {
            rest = stripped.trim_start();
        }
        if rest.is_empty() {
            return None;
        }
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        let after = &rest[eq + 1..];
        let end = member_end(after);
        let member = after[..end].trim();
        rest = after[end..].trim_start();
        Some((key, member))
    })
}

/// The byte offset where a dictionary member's raw value ends: the next
/// top-level comma outside quoted strings, parens and byte sequences.
fn member_end(value: &str) -> usize {
    let mut in_quotes = false;
    let mut in_bytes = false;
    let mut escaped = false;
    let mut depth = 0usize;
    for (index, byte) in value.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_quotes => escaped = true,
            b'"' => in_quotes = !in_quotes,
            b':' if !in_quotes => in_bytes = !in_bytes,
            b'(' if !in_quotes && !in_bytes => depth += 1,
            b')' if !in_quotes && !in_bytes => depth = depth.saturating_sub(1),
            b',' if !in_quotes && !in_bytes && depth == 0 => return index,
            _ => {}
        }
    }
    value.len()
}

/// One parsed `Signature-Input` member: the covered components, the
/// parameters we care about, and the member's raw text (the exact bytes the
/// sender signed as `@signature-params`).
struct SignatureInputMember<'a> {
    raw: &'a str,
    components: Vec<String>,
    key_id: String,
    algorithm: Option<String>,
    created: Option<u64>,
    expires: Option<u64>,
}

fn parse_signature_input_member(raw: &str) -> Result<SignatureInputMember<'_>, SignatureError> {
    let inner = raw.strip_prefix('(').ok_or(SignatureError::Malformed(
        "signature input is not an inner list",
    ))?;
    let close = inner
        .find(')')
        .ok_or(SignatureError::Malformed("unterminated inner list"))?;
    let mut components = Vec::new();
    for item in inner[..close].split_whitespace() {
        let name = item
            .strip_prefix('"')
            .and_then(|i| i.strip_suffix('"'))
            .ok_or(SignatureError::Malformed(
                "component is not a quoted string",
            ))?;
        if name.contains('"') || name.contains(';') {
            // Component parameters (`"name";key=v`) change the base line's
            // serialization; nobody in the fediverse sends them.
            return Err(SignatureError::Malformed(
                "component parameters unsupported",
            ));
        }
        components.push(name.to_ascii_lowercase());
    }

    let mut key_id = None;
    let mut algorithm = None;
    let mut created = None;
    let mut expires = None;
    for param in ParamIter(&inner[close + 1..]) {
        let (name, value) = param?;
        match name {
            "keyid" => key_id = Some(unquote(value)?),
            "alg" => algorithm = Some(unquote(value)?),
            "created" => created = Some(parse_integer(value)?),
            "expires" => expires = Some(parse_integer(value)?),
            // `nonce`, `tag`… don't affect verification; the raw text keeps
            // them in the signature base.
            _ => {}
        }
    }
    Ok(SignatureInputMember {
        raw,
        components,
        key_id: key_id.ok_or(SignatureError::Malformed("missing keyid parameter"))?,
        algorithm,
        created,
        expires,
    })
}

/// Iterates `;name=value` parameters after an inner list.
struct ParamIter<'a>(&'a str);

impl<'a> Iterator for ParamIter<'a> {
    type Item = Result<(&'a str, &'a str), SignatureError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.0.trim_start();
        let stripped = rest.strip_prefix(';')?;
        let stripped = stripped.trim_start();
        let Some(eq) = stripped.find('=') else {
            self.0 = "";
            return Some(Err(SignatureError::Malformed("parameter without value")));
        };
        let name = &stripped[..eq];
        let after = &stripped[eq + 1..];
        let end = if let Some(quoted) = after.strip_prefix('"') {
            let Some(close) = quoted.find('"') else {
                self.0 = "";
                return Some(Err(SignatureError::Malformed("unterminated quoted value")));
            };
            close + 2
        } else {
            after.find(';').unwrap_or(after.len())
        };
        self.0 = &after[end..];
        Some(Ok((name.trim(), after[..end].trim())))
    }
}

fn unquote(value: &str) -> Result<String, SignatureError> {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .map(|v| v.replace("\\\"", "\"").replace("\\\\", "\\"))
        .ok_or(SignatureError::Malformed("expected a quoted string"))
}

fn parse_integer(value: &str) -> Result<u64, SignatureError> {
    value
        .parse()
        .map_err(|_| SignatureError::Malformed("expected an integer parameter"))
}

/// The request-shape inputs a signature base may draw derived components
/// from. `target_uri` is the absolute URI the client used (reconstructed by
/// the server from its external scheme + `Host` + path).
pub struct RequestFacts<'a> {
    pub method: &'a str,
    pub target_uri: &'a str,
    pub path_and_query: &'a str,
    pub headers: &'a HeaderMap,
}

impl RequestFacts<'_> {
    fn derived_component(&self, name: &str) -> Result<String, SignatureError> {
        let (path, query) = match self.path_and_query.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (self.path_and_query, None),
        };
        match name {
            "@method" => Ok(self.method.to_ascii_uppercase()),
            "@target-uri" => Ok(self.target_uri.to_owned()),
            "@request-target" => Ok(self.path_and_query.to_owned()),
            "@path" => Ok(path.to_owned()),
            "@query" => Ok(format!("?{}", query.unwrap_or(""))),
            "@authority" => self
                .headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_ascii_lowercase())
                .ok_or_else(|| SignatureError::MissingRequestHeader("host".to_owned())),
            "@scheme" => Ok(self
                .target_uri
                .split_once("://")
                .map_or("https", |(scheme, _)| scheme)
                .to_owned()),
            other => Err(SignatureError::Malformed(match other {
                "@status" | "@query-param" => "unsupported derived component",
                _ => "unknown derived component",
            })),
        }
    }

    fn header_component(&self, name: &str) -> Result<String, SignatureError> {
        let values: Vec<&str> = self
            .headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::trim)
            .collect();
        if values.is_empty() {
            return Err(SignatureError::MissingRequestHeader(name.to_owned()));
        }
        Ok(values.join(", "))
    }
}

/// A parsed and structurally-validated RFC 9421 signature, ready to be
/// checked against a public key — the `Signature-Input` twin of
/// [`crate::signature::PreparedVerification`].
pub struct PreparedRfc9421 {
    key_id: String,
    algorithm: Option<String>,
    base: String,
    signature: Vec<u8>,
}

impl PreparedRfc9421 {
    /// Validates everything that does not need the public key. Enforces the
    /// same strength Mastodon does: `keyid` + `created` parameters, the
    /// `@method` and `@target-uri` components, and — when the request has a
    /// body — a covered, matching `Content-Digest`.
    pub fn from_request(
        facts: &RequestFacts<'_>,
        body: Option<&[u8]>,
        now: SystemTime,
    ) -> Result<Self, SignatureError> {
        let input_header = required_header(facts.headers, "signature-input")?;
        let signature_header = required_header(facts.headers, "signature")?;

        // Pair the first `Signature-Input` member with its signature; peers
        // send exactly one in practice.
        let (label, raw_member) = dict_members(&input_header)
            .next()
            .ok_or(SignatureError::Malformed("empty Signature-Input"))?;
        let member = parse_signature_input_member(raw_member)?;
        let signature = dict_members(&signature_header)
            .find(|(key, _)| *key == label)
            .map(|(_, value)| value)
            .ok_or(SignatureError::Malformed("no signature for the label"))?;
        let signature = signature
            .strip_prefix(':')
            .and_then(|v| v.strip_suffix(':'))
            .and_then(|v| BASE64.decode(v).ok())
            .ok_or(SignatureError::Malformed(
                "signature is not a byte sequence",
            ))?;

        for required in ["@method", "@target-uri"] {
            if !member.components.iter().any(|c| c == required) {
                return Err(SignatureError::UncoveredHeader(match required {
                    "@method" => "@method",
                    _ => "@target-uri",
                }));
            }
        }
        let created = member
            .created
            .ok_or(SignatureError::Malformed("missing created parameter"))?;
        check_created_window(created, member.expires, now)?;

        if let Some(body) = body {
            if !member.components.iter().any(|c| c == "content-digest") {
                return Err(SignatureError::UncoveredHeader("content-digest"));
            }
            let digest_header = required_header(facts.headers, "content-digest")
                .map_err(|_| SignatureError::DigestMismatch)?;
            if !content_digest_matches(&digest_header, body) {
                return Err(SignatureError::DigestMismatch);
            }
        }

        let mut lines = Vec::with_capacity(member.components.len() + 1);
        for component in &member.components {
            let value = if component.starts_with('@') {
                facts.derived_component(component)?
            } else {
                facts.header_component(component)?
            };
            lines.push(format!("\"{component}\": {value}"));
        }
        lines.push(format!("\"@signature-params\": {}", member.raw));
        Ok(Self {
            key_id: member.key_id,
            algorithm: member.algorithm,
            base: lines.join("\n"),
            signature,
        })
    }

    /// The keyId the sender claims; resolve it to a key and call the
    /// matching verify method.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Whether this signature needs an Ed25519 key rather than the actor's
    /// RSA one: declared via `alg`, or implied by Mitra's `#ed25519-key`
    /// keyid fragment when `alg` is omitted.
    #[must_use]
    pub fn wants_ed25519(&self) -> bool {
        match self.algorithm.as_deref() {
            Some("ed25519") => true,
            Some(_) => false,
            None => self.key_id.ends_with("#ed25519-key"),
        }
    }

    /// Checks the signature against an RSA public key PEM
    /// (`rsa-v1_5-sha256`).
    pub fn verify_rsa_pem(&self, public_key_pem: &str) -> Result<(), SignatureError> {
        if let Some(algorithm) = self.algorithm.as_deref()
            && algorithm != "rsa-v1_5-sha256"
        {
            return Err(SignatureError::Invalid);
        }
        // Tolerate the same PEM sloppiness as draft-cavage verification.
        let normalized = public_key_pem.replace('\r', "");
        let trimmed = normalized.trim();
        let key = RsaPublicKey::from_public_key_pem(trimmed)
            .or_else(|_| RsaPublicKey::from_pkcs1_pem(trimmed))
            .map_err(|_| SignatureError::BadPublicKey)?;
        let signature = RsaSignature::try_from(self.signature.as_slice())
            .map_err(|_| SignatureError::Invalid)?;
        rsa::pkcs1v15::VerifyingKey::<RsaSha256>::new(key)
            .verify(self.base.as_bytes(), &signature)
            .map_err(|_| SignatureError::Invalid)
    }

    /// Checks the signature against an Ed25519 public Multikey (`z6Mk…`).
    pub fn verify_ed25519_multikey(&self, multikey: &str) -> Result<(), SignatureError> {
        let key_bytes = plamenu_ap::multikey::decode_ed25519_public(multikey)
            .map_err(|_| SignatureError::BadPublicKey)?;
        let key = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes)
            .map_err(|_| SignatureError::BadPublicKey)?;
        let signature = ed25519_dalek::Signature::from_slice(&self.signature)
            .map_err(|_| SignatureError::Invalid)?;
        ed25519_dalek::Verifier::verify(&key, self.base.as_bytes(), &signature)
            .map_err(|_| SignatureError::Invalid)
    }
}

fn required_header(headers: &HeaderMap, name: &str) -> Result<String, SignatureError> {
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .collect();
    if values.is_empty() {
        return Err(SignatureError::MissingSignatureHeader);
    }
    Ok(values.join(", "))
}

fn check_created_window(
    created: u64,
    expires: Option<u64>,
    now: SystemTime,
) -> Result<(), SignatureError> {
    let created = UNIX_EPOCH + Duration::from_secs(created);
    let too_old = now.duration_since(created).is_ok_and(|age| age > MAX_AGE);
    let too_new = created
        .duration_since(now)
        .is_ok_and(|ahead| ahead > MAX_CLOCK_SKEW);
    if too_old || too_new {
        return Err(SignatureError::DateOutOfWindow);
    }
    if let Some(expires) = expires {
        let expires = UNIX_EPOCH + Duration::from_secs(expires);
        if now > expires {
            return Err(SignatureError::DateOutOfWindow);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use http::header::{HeaderName, HeaderValue};
    use plamenu_ap::keys::{generate_ed25519_keypair, generate_keypair};

    use super::*;

    fn header_map(entries: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in entries {
            map.append(
                name.parse::<HeaderName>().unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn signed_request(
        signer: &RequestSigner,
        body: &[u8],
        now: SystemTime,
    ) -> (HeaderMap, &'static str) {
        let target = "https://plamenu.test/inbox";
        let produced = signer.sign_post_rfc9421(target, body, now);
        let headers = header_map(&[
            ("host", "plamenu.test"),
            ("date", &produced.date),
            ("content-digest", &produced.content_digest),
            ("signature-input", &produced.signature_input),
            ("signature", &produced.signature),
        ]);
        (headers, target)
    }

    fn facts<'a>(target: &'a str, headers: &'a HeaderMap) -> RequestFacts<'a> {
        RequestFacts {
            method: "POST",
            target_uri: target,
            path_and_query: "/inbox",
            headers,
        }
    }

    #[test]
    fn content_digest_roundtrip() {
        // RFC 9530's own sha-256 example for `{"hello": "world"}`.
        let body = br#"{"hello": "world"}"#;
        assert_eq!(
            content_digest(body),
            "sha-256=:X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=:"
        );
        assert!(content_digest_matches(&content_digest(body), body));
        assert!(!content_digest_matches(&content_digest(body), b"tampered"));
        // A sha-512-only dictionary is not acceptable (Mastodon parity)…
        assert!(!content_digest_matches("sha-512=:AAAA=:", body));
        // …but sha-256 alongside other members is found.
        let both = format!("sha-512=:AAAA=:, {}", content_digest(body));
        assert!(content_digest_matches(&both, body));
    }

    #[test]
    fn sign_verify_roundtrip_rsa() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://plamenu.test/users/alice#main-key".into(),
        )
        .unwrap();
        let body = br#"{"type":"Create"}"#;
        let now = SystemTime::now();
        let (headers, target) = signed_request(&signer, body, now);

        let prepared =
            PreparedRfc9421::from_request(&facts(target, &headers), Some(body), now).unwrap();
        assert_eq!(
            prepared.key_id(),
            "https://plamenu.test/users/alice#main-key"
        );
        assert!(!prepared.wants_ed25519());
        prepared.verify_rsa_pem(&pair.public_pem).unwrap();

        // The wrong key fails.
        let other = generate_keypair().unwrap();
        assert!(matches!(
            PreparedRfc9421::from_request(&facts(target, &headers), Some(body), now)
                .unwrap()
                .verify_rsa_pem(&other.public_pem),
            Err(SignatureError::Invalid)
        ));
    }

    #[test]
    fn tampered_body_fails_digest() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, target) = signed_request(&signer, b"original", now);
        assert!(matches!(
            PreparedRfc9421::from_request(&facts(target, &headers), Some(b"tampered"), now),
            Err(SignatureError::DigestMismatch)
        ));
    }

    #[test]
    fn replayed_signature_on_another_target_fails() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, _) = signed_request(&signer, b"body", now);
        let moved = RequestFacts {
            method: "POST",
            target_uri: "https://other.test/inbox",
            path_and_query: "/inbox",
            headers: &headers,
        };
        let prepared = PreparedRfc9421::from_request(&moved, Some(b"body"), now).unwrap();
        assert!(matches!(
            prepared.verify_rsa_pem(&pair.public_pem),
            Err(SignatureError::Invalid)
        ));
    }

    #[test]
    fn stale_created_is_rejected() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, target) = signed_request(&signer, b"body", now);
        assert!(matches!(
            PreparedRfc9421::from_request(
                &facts(target, &headers),
                Some(b"body"),
                now + Duration::from_hours(13),
            ),
            Err(SignatureError::DateOutOfWindow)
        ));
    }

    #[test]
    fn post_signature_must_cover_content_digest() {
        // A signature covering only @method/@target-uri is too weak for a
        // body-bearing request (Mastodon rejects it identically).
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let created = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let params = format!(
            "(\"@method\" \"@target-uri\");created={created};keyid=\"k\";alg=\"rsa-v1_5-sha256\""
        );
        let base = format!(
            "\"@method\": POST\n\"@target-uri\": https://plamenu.test/inbox\n\"@signature-params\": {params}",
        );
        let signature = BASE64.encode(signer.sign_bytes(base.as_bytes()));
        let headers = header_map(&[
            ("host", "plamenu.test"),
            ("signature-input", &format!("sig1={params}")),
            ("signature", &format!("sig1=:{signature}:")),
        ]);
        assert!(matches!(
            PreparedRfc9421::from_request(
                &facts("https://plamenu.test/inbox", &headers),
                Some(b"body"),
                now,
            ),
            Err(SignatureError::UncoveredHeader("content-digest"))
        ));
    }

    /// An Ed25519 signature the way Mitra would send one: `alg="ed25519"`,
    /// keyid `#ed25519-key`, key published as a Multikey.
    #[test]
    fn verifies_ed25519_signatures() {
        let pair = generate_ed25519_keypair();
        let secret = plamenu_ap::multikey::decode_ed25519_private(&pair.private_multibase).unwrap();
        let signing = ed25519_dalek::SigningKey::from_bytes(&secret);

        let now = SystemTime::now();
        let created = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let body = br#"{"type":"Like"}"#;
        let digest = content_digest(body);
        let key_id = "https://mitra.example/users/erin#ed25519-key";
        let params = format!(
            "(\"@method\" \"@target-uri\" \"content-digest\");created={created};keyid=\"{key_id}\";alg=\"ed25519\"",
        );
        let base = format!(
            "\"@method\": POST\n\"@target-uri\": https://plamenu.test/inbox\n\"content-digest\": {digest}\n\"@signature-params\": {params}",
        );
        let signature =
            BASE64.encode(ed25519_dalek::Signer::sign(&signing, base.as_bytes()).to_bytes());
        let headers = header_map(&[
            ("host", "plamenu.test"),
            ("content-digest", &digest),
            ("signature-input", &format!("sig1={params}")),
            ("signature", &format!("sig1=:{signature}:")),
        ]);

        let prepared = PreparedRfc9421::from_request(
            &facts("https://plamenu.test/inbox", &headers),
            Some(body),
            now,
        )
        .unwrap();
        assert!(prepared.wants_ed25519());
        assert_eq!(prepared.key_id(), key_id);
        prepared
            .verify_ed25519_multikey(&pair.public_multibase)
            .unwrap();

        let other = generate_ed25519_keypair();
        let again = PreparedRfc9421::from_request(
            &facts("https://plamenu.test/inbox", &headers),
            Some(body),
            now,
        )
        .unwrap();
        assert!(matches!(
            again.verify_ed25519_multikey(&other.public_multibase),
            Err(SignatureError::Invalid)
        ));
    }

    /// The Ed25519 GET fallback we send when a peer refuses our draft-cavage
    /// `keyId` must verify under our own RFC 9421 reader — the same reader
    /// that models Mastodon's strength requirements — and must name the
    /// FEP-521a `#ed25519-key` id rather than the RSA one.
    #[test]
    fn ed25519_get_fallback_roundtrips() {
        let rsa = generate_keypair().unwrap();
        let ed = generate_ed25519_keypair();
        let signer = RequestSigner::from_pkcs8_pem(
            &rsa.private_pem,
            "https://plamenu.test/actor#main-key".into(),
        )
        .unwrap()
        .with_ed25519(
            "https://plamenu.test/actor#ed25519-key".into(),
            &ed.private_multibase,
        );

        let target = "https://mastodon.example/ap/users/1";
        let now = SystemTime::now();
        let produced = signer.sign_get_rfc9421_ed25519(target, now).unwrap();
        let headers = header_map(&[
            ("host", "mastodon.example"),
            ("date", &produced.date),
            ("signature-input", &produced.signature_input),
            ("signature", &produced.signature),
        ]);
        let facts = RequestFacts {
            method: "GET",
            target_uri: target,
            path_and_query: "/ap/users/1",
            headers: &headers,
        };

        let prepared = PreparedRfc9421::from_request(&facts, None, now).unwrap();
        assert!(prepared.wants_ed25519());
        assert_eq!(prepared.key_id(), "https://plamenu.test/actor#ed25519-key");
        prepared
            .verify_ed25519_multikey(&ed.public_multibase)
            .unwrap();

        // A different key must not verify it.
        let other = generate_ed25519_keypair();
        assert!(matches!(
            PreparedRfc9421::from_request(&facts, None, now)
                .unwrap()
                .verify_ed25519_multikey(&other.public_multibase),
            Err(SignatureError::Invalid)
        ));

        // Without an Ed25519 key there is no fallback to offer.
        let rsa_only = RequestSigner::from_pkcs8_pem(
            &rsa.private_pem,
            "https://plamenu.test/actor#main-key".into(),
        )
        .unwrap();
        assert!(rsa_only.sign_get_rfc9421_ed25519(target, now).is_none());
        // An unusable stored key degrades the same way rather than panicking.
        assert!(
            RequestSigner::from_pkcs8_pem(
                &rsa.private_pem,
                "https://plamenu.test/actor#main-key".into(),
            )
            .unwrap()
            .with_ed25519("https://plamenu.test/actor#ed25519-key".into(), "not-a-key")
            .sign_get_rfc9421_ed25519(target, now)
            .is_none()
        );
    }

    /// A GET with no body needs no Content-Digest but keeps the component
    /// strength requirements.
    #[test]
    fn verifies_bodyless_gets() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let created = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let target = "https://plamenu.test/users/alice";
        let params = format!(
            "(\"@method\" \"@target-uri\");created={created};keyid=\"k\";alg=\"rsa-v1_5-sha256\"",
        );
        let base = format!(
            "\"@method\": GET\n\"@target-uri\": {target}\n\"@signature-params\": {params}",
        );
        let signature = BASE64.encode(signer.sign_bytes(base.as_bytes()));
        let headers = header_map(&[
            ("host", "plamenu.test"),
            ("signature-input", &format!("sig1={params}")),
            ("signature", &format!("sig1=:{signature}:")),
        ]);
        let prepared = PreparedRfc9421::from_request(
            &RequestFacts {
                method: "GET",
                target_uri: target,
                path_and_query: "/users/alice",
                headers: &headers,
            },
            None,
            now,
        )
        .unwrap();
        prepared.verify_rsa_pem(&pair.public_pem).unwrap();
    }

    /// Unknown parameters (`nonce`, `tag`) must survive verbatim in the
    /// signature base — verification succeeds because the raw member text is
    /// used, not a re-serialization.
    #[test]
    fn preserves_unknown_parameters_in_base() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let created = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let body = b"body";
        let digest = content_digest(body);
        let params = format!(
            "(\"@method\" \"@target-uri\" \"content-digest\");created={created};keyid=\"k\";nonce=\"abc,def\";alg=\"rsa-v1_5-sha256\"",
        );
        let base = format!(
            "\"@method\": POST\n\"@target-uri\": https://plamenu.test/inbox\n\"content-digest\": {digest}\n\"@signature-params\": {params}",
        );
        let signature = BASE64.encode(signer.sign_bytes(base.as_bytes()));
        let headers = header_map(&[
            ("host", "plamenu.test"),
            ("content-digest", &digest),
            ("signature-input", &format!("sig1={params}")),
            ("signature", &format!("sig1=:{signature}:")),
        ]);
        PreparedRfc9421::from_request(
            &facts("https://plamenu.test/inbox", &headers),
            Some(body),
            now,
        )
        .unwrap()
        .verify_rsa_pem(&pair.public_pem)
        .unwrap();
    }

    #[test]
    fn missing_pieces_are_distinct_errors() {
        let headers = header_map(&[("host", "plamenu.test")]);
        assert!(matches!(
            PreparedRfc9421::from_request(
                &facts("https://plamenu.test/inbox", &headers),
                Some(b""),
                SystemTime::now(),
            ),
            Err(SignatureError::MissingSignatureHeader)
        ));

        // Signature-Input without a matching Signature member.
        let headers = header_map(&[
            ("host", "plamenu.test"),
            (
                "signature-input",
                "sig1=(\"@method\" \"@target-uri\");created=1;keyid=\"k\"",
            ),
            ("signature", "other=:AAAA:"),
        ]);
        assert!(matches!(
            PreparedRfc9421::from_request(
                &facts("https://plamenu.test/inbox", &headers),
                None,
                SystemTime::now(),
            ),
            Err(SignatureError::Malformed("no signature for the label"))
        ));

        // Missing created parameter.
        let headers = header_map(&[
            ("host", "plamenu.test"),
            (
                "signature-input",
                "sig1=(\"@method\" \"@target-uri\");keyid=\"k\"",
            ),
            ("signature", "sig1=:AAAA:"),
        ]);
        assert!(matches!(
            PreparedRfc9421::from_request(
                &facts("https://plamenu.test/inbox", &headers),
                None,
                SystemTime::now(),
            ),
            Err(SignatureError::Malformed("missing created parameter"))
        ));
    }
}
