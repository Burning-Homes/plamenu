//! Draft-cavage HTTP signatures, RSA-SHA256 — the dialect Mastodon and the
//! rest of the fediverse actually speak.
//!
//! Signing string: one line per signed header, `name: value`, joined with
//! `\n`, where the pseudo-header `(request-target)` expands to
//! `{lowercase method} {path[?query]}`.

use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::HeaderMap;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use thiserror::Error;

use crate::digest;

/// Mastodon rejects signatures whose `Date` is more than 12 hours old…
const MAX_AGE: Duration = Duration::from_hours(12);
/// …or unreasonably far in the future (clock skew allowance).
const MAX_CLOCK_SKEW: Duration = Duration::from_hours(1);

#[derive(Debug, Error)]
pub enum SignatureError {
    #[error("request has no Signature header")]
    MissingSignatureHeader,
    #[error("malformed Signature header: {0}")]
    Malformed(&'static str),
    #[error("signature does not cover required header {0}")]
    UncoveredHeader(&'static str),
    #[error("signed header {0} is missing from the request")]
    MissingRequestHeader(String),
    #[error("Date header is missing or unparsable")]
    DateInvalid,
    #[error("Date header is outside the acceptance window")]
    DateOutOfWindow,
    #[error("Digest header does not match the request body")]
    DigestMismatch,
    #[error("public key is not valid PEM (SPKI or PKCS#1)")]
    BadPublicKey,
    #[error("private key is not valid PKCS#8 PEM")]
    BadPrivateKey,
    #[error("signature verification failed")]
    Invalid,
}

/// Parsed `Signature` header parameters.
#[derive(Debug, Clone)]
pub struct SignatureParams {
    pub key_id: String,
    pub algorithm: Option<String>,
    /// Draft-cavage `hs2019` pseudo-component values. Discourse signs these
    /// from the Signature header parameters rather than sending physical
    /// `Created` / `Expires` request headers.
    pub created: Option<String>,
    pub expires: Option<String>,
    /// Lowercased signed-header names, in signing order.
    pub headers: Vec<String>,
    pub signature: Vec<u8>,
}

/// Parses `keyId="…",algorithm="…",headers="…",signature="…"`.
pub fn parse_signature_header(value: &str) -> Result<SignatureParams, SignatureError> {
    let mut key_id = None;
    let mut algorithm = None;
    let mut created = None;
    let mut expires = None;
    let mut headers = None;
    let mut signature = None;

    let mut rest = value.trim();
    while !rest.is_empty() {
        let eq = rest
            .find('=')
            .ok_or(SignatureError::Malformed("expected key=\"value\" pair"))?;
        let name = rest[..eq].trim();
        let after = &rest[eq + 1..];
        let unquoted = after
            .strip_prefix('"')
            .ok_or(SignatureError::Malformed("value is not quoted"))?;
        let close = unquoted
            .find('"')
            .ok_or(SignatureError::Malformed("unterminated quoted value"))?;
        let val = &unquoted[..close];
        match name {
            "keyId" => key_id = Some(val.to_owned()),
            "algorithm" => algorithm = Some(val.to_owned()),
            "created" => created = Some(val.to_owned()),
            "expires" => expires = Some(val.to_owned()),
            "headers" => {
                headers = Some(
                    val.split_whitespace()
                        .map(str::to_ascii_lowercase)
                        .collect(),
                );
            }
            "signature" => {
                signature = Some(
                    BASE64
                        .decode(val)
                        .map_err(|_| SignatureError::Malformed("signature is not base64"))?,
                );
            }
            // Unknown parameters are ignored per spec.
            _ => {}
        }
        rest = unquoted[close + 1..]
            .trim_start()
            .trim_start_matches(',')
            .trim_start();
    }

    Ok(SignatureParams {
        key_id: key_id.ok_or(SignatureError::Malformed("missing keyId"))?,
        algorithm,
        created,
        expires,
        // Per draft-cavage the default header list is just `date`.
        headers: headers.unwrap_or_else(|| vec!["date".to_owned()]),
        signature: signature.ok_or(SignatureError::Malformed("missing signature"))?,
    })
}

/// Builds the signing string for the given signed-header list.
fn build_signing_string(
    params: &SignatureParams,
    method: &str,
    path_and_query: &str,
    request_headers: &HeaderMap,
) -> Result<String, SignatureError> {
    let mut lines = Vec::with_capacity(params.headers.len());
    for name in &params.headers {
        if name == "(request-target)" {
            lines.push(format!(
                "(request-target): {} {path_and_query}",
                method.to_ascii_lowercase()
            ));
        } else if name == "(created)" {
            let value = params
                .created
                .as_deref()
                .ok_or_else(|| SignatureError::MissingRequestHeader(name.clone()))?;
            lines.push(format!("(created): {value}"));
        } else if name == "(expires)" {
            let value = params
                .expires
                .as_deref()
                .ok_or_else(|| SignatureError::MissingRequestHeader(name.clone()))?;
            lines.push(format!("(expires): {value}"));
        } else {
            let value = request_headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| SignatureError::MissingRequestHeader(name.clone()))?;
            lines.push(format!("{name}: {}", value.trim()));
        }
    }
    Ok(lines.join("\n"))
}

/// Signs outbound requests with a local actor's RSA key, and — once
/// [`RequestSigner::with_ed25519`] has supplied one — its FEP-521a Ed25519
/// key as well, for the RFC 9421 fallback.
pub struct RequestSigner {
    key_id: String,
    signing_key: SigningKey<Sha256>,
    ed25519: Option<Ed25519Signer>,
}

/// The actor's Ed25519 verification method: the key and the `#ed25519-key` id
/// that names it in the actor's `assertionMethod`.
pub(crate) struct Ed25519Signer {
    pub(crate) key_id: String,
    pub(crate) signing_key: ed25519_dalek::SigningKey,
}

/// Headers to attach to a signed POST.
#[derive(Debug, Clone)]
pub struct SignedPostHeaders {
    pub host: String,
    pub date: String,
    pub digest: String,
    pub signature: String,
}

/// Headers to attach to a signed GET (no body, so no Digest).
#[derive(Debug, Clone)]
pub struct SignedGetHeaders {
    pub date: String,
    pub signature: String,
}

impl RequestSigner {
    pub fn from_pkcs8_pem(private_key_pem: &str, key_id: String) -> Result<Self, SignatureError> {
        let key = RsaPrivateKey::from_pkcs8_pem(private_key_pem)
            .map_err(|_| SignatureError::BadPrivateKey)?;
        Ok(Self {
            key_id,
            signing_key: SigningKey::new(key),
            ed25519: None,
        })
    }

    /// Adds the actor's FEP-521a Ed25519 key, given as the stored secret
    /// Multikey (`z3u2…`), so signed requests can fall back to RFC 9421 when
    /// a peer cannot resolve the RSA `keyId`. A key that does not decode is
    /// dropped: the RSA dialect alone is still a working signer.
    #[must_use]
    pub fn with_ed25519(mut self, key_id: String, private_multibase: &str) -> Self {
        self.ed25519 = plamenu_ap::multikey::decode_ed25519_private(private_multibase)
            .ok()
            .map(|secret| Ed25519Signer {
                key_id,
                signing_key: ed25519_dalek::SigningKey::from_bytes(&secret),
            });
        self
    }

    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub(crate) fn ed25519(&self) -> Option<&Ed25519Signer> {
        self.ed25519.as_ref()
    }

    /// Raw RSA-PKCS1v15-SHA256 over arbitrary bytes — the shared primitive
    /// under both the draft-cavage signing string and the RFC 9421 base.
    pub(crate) fn sign_bytes(&self, data: &[u8]) -> Vec<u8> {
        self.signing_key.sign(data).to_bytes().into()
    }

    /// Signs a POST of `body` to `path_and_query` on `host`, at time `now`.
    #[must_use]
    pub fn sign_post(
        &self,
        host: &str,
        path_and_query: &str,
        body: &[u8],
        now: SystemTime,
    ) -> SignedPostHeaders {
        let date = httpdate::fmt_http_date(now);
        let digest = digest::compute(body);
        let signing_string = format!(
            "(request-target): post {path_and_query}\nhost: {host}\ndate: {date}\ndigest: {digest}",
        );
        let signature = BASE64.encode(self.signing_key.sign(signing_string.as_bytes()).to_bytes());
        let signature = format!(
            "keyId=\"{}\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date digest\",signature=\"{signature}\"",
            self.key_id,
        );
        SignedPostHeaders {
            host: host.to_owned(),
            date,
            digest,
            signature,
        }
    }

    /// Signs a GET of `path_and_query` on `host`, at time `now`. The `Accept`
    /// header is part of the signature (Mastodon signs every header it sends
    /// except `User-Agent` and `Accept-Encoding`), so the caller must send
    /// `accept` verbatim.
    #[must_use]
    pub fn sign_get(
        &self,
        host: &str,
        path_and_query: &str,
        accept: &str,
        now: SystemTime,
    ) -> SignedGetHeaders {
        let date = httpdate::fmt_http_date(now);
        let signing_string = format!(
            "(request-target): get {path_and_query}\nhost: {host}\ndate: {date}\naccept: {accept}",
        );
        let signature = BASE64.encode(self.signing_key.sign(signing_string.as_bytes()).to_bytes());
        let signature = format!(
            "keyId=\"{}\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date accept\",signature=\"{signature}\"",
            self.key_id,
        );
        SignedGetHeaders { date, signature }
    }

    /// Signs a bodyless DELETE request. FEP-ae97's media deletion endpoint
    /// uses the same draft-cavage covered set as Minimitra: request target,
    /// host and date, with no synthetic Digest for an absent body.
    #[must_use]
    pub fn sign_delete(
        &self,
        host: &str,
        path_and_query: &str,
        now: SystemTime,
    ) -> SignedGetHeaders {
        let date = httpdate::fmt_http_date(now);
        let signing_string =
            format!("(request-target): delete {path_and_query}\nhost: {host}\ndate: {date}");
        let signature = BASE64.encode(self.signing_key.sign(signing_string.as_bytes()).to_bytes());
        let signature = format!(
            "keyId=\"{}\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date\",signature=\"{signature}\"",
            self.key_id,
        );
        SignedGetHeaders { date, signature }
    }
}

/// A parsed and structurally-validated inbound signature, ready to be checked
/// against a public key. Splitting verification in two phases keeps this
/// crate free of storage concerns: the caller resolves `key_id()` to a key
/// (cache or fetch) and then calls [`Self::verify_pem`].
pub struct PreparedVerification {
    params: SignatureParams,
    signing_string: String,
    /// An alternative signing string accepted in addition to `signing_string`.
    /// Pleroma signs a GET's `(request-target)` with the URL *path only*
    /// (Elixir's `URI.path`), dropping the query string even though it still
    /// sends it in the request line — so its signed fetch of a paginated
    /// collection page (`…/followers?page=1`) never matches a path+query
    /// reconstruction. When the request carried a query we keep the path-only
    /// variant here so either form verifies. This is restricted to Pleroma's
    /// dedicated `/internal/fetch` actor: accepting it from ordinary actors
    /// would let an intermediary alter an otherwise signed query string.
    /// `None` when there is no query (the two forms are identical), the signer
    /// is not an internal-fetch actor, or for POSTs (inbox targets carry no
    /// query).
    signing_string_alt: Option<String>,
}

impl PreparedVerification {
    /// Validates everything that does not need the public key: header
    /// structure, required covered headers, `Date` window, body `Digest`.
    pub fn from_request(
        method: &str,
        path_and_query: &str,
        headers: &HeaderMap,
        body: &[u8],
        now: SystemTime,
    ) -> Result<Self, SignatureError> {
        let params = Self::parsed_params(headers, &["(request-target)", "date", "digest"])?;
        Self::check_date_window(headers, now)?;

        let digest_value = headers
            .get("digest")
            .and_then(|v| v.to_str().ok())
            .ok_or(SignatureError::DigestMismatch)?;
        if !digest::matches(digest_value, body) {
            return Err(SignatureError::DigestMismatch);
        }

        let signing_string = build_signing_string(&params, method, path_and_query, headers)?;
        Ok(Self {
            params,
            signing_string,
            signing_string_alt: None,
        })
    }

    /// Like [`Self::from_request`] for a bodyless GET: no `Digest`, but the
    /// `Host` header must be covered (Mastodon's rule for signed GETs).
    pub fn from_get_request(
        path_and_query: &str,
        headers: &HeaderMap,
        now: SystemTime,
    ) -> Result<Self, SignatureError> {
        Self::from_bodyless_request("GET", path_and_query, headers, now, false)
    }

    /// Validates a bodyless signed request. `allow_query_omission` accepts the
    /// legacy signer behavior that covers only the URL path even when the
    /// request URI carries a query. It is deliberately opt-in: FEP-ae97 inbox
    /// cursors need it for Minimitra, while general authorized fetch keeps its
    /// narrower Pleroma-only exception.
    pub fn from_bodyless_request(
        method: &str,
        path_and_query: &str,
        headers: &HeaderMap,
        now: SystemTime,
        allow_query_omission: bool,
    ) -> Result<Self, SignatureError> {
        let params = Self::parsed_params(headers, &["(request-target)", "host", "date"])?;
        Self::check_date_window(headers, now)?;
        let signing_string = build_signing_string(&params, method, path_and_query, headers)?;
        // Pleroma signs the path only with its dedicated internal-fetch actor.
        // Keep that exact compatibility case narrow so normal actors cannot
        // authenticate one query string and have it replayed as another.
        let internal_fetch = params
            .key_id
            .split('#')
            .next()
            .is_some_and(|actor| actor.ends_with("/internal/fetch"));
        let signing_string_alt = if internal_fetch || allow_query_omission {
            path_and_query
                .split_once('?')
                .map(|(path, _query)| build_signing_string(&params, method, path, headers))
                .transpose()?
        } else {
            None
        };
        Ok(Self {
            params,
            signing_string,
            signing_string_alt,
        })
    }

    /// Parses the `Signature` header and checks it covers `required`.
    fn parsed_params(
        headers: &HeaderMap,
        required: &[&'static str],
    ) -> Result<SignatureParams, SignatureError> {
        let header = headers
            .get("signature")
            .and_then(|v| v.to_str().ok())
            .ok_or(SignatureError::MissingSignatureHeader)?;
        let params = parse_signature_header(header)?;
        for name in required {
            if !params.headers.iter().any(|h| h == name) {
                return Err(SignatureError::UncoveredHeader(name));
            }
        }
        Ok(params)
    }

    /// Rejects requests whose `Date` is too old or too far in the future.
    fn check_date_window(headers: &HeaderMap, now: SystemTime) -> Result<(), SignatureError> {
        let date_value = headers
            .get("date")
            .and_then(|v| v.to_str().ok())
            .ok_or(SignatureError::DateInvalid)?;
        let date =
            httpdate::parse_http_date(date_value).map_err(|_| SignatureError::DateInvalid)?;
        let too_old = now.duration_since(date).is_ok_and(|age| age > MAX_AGE);
        let too_new = date
            .duration_since(now)
            .is_ok_and(|ahead| ahead > MAX_CLOCK_SKEW);
        if too_old || too_new {
            return Err(SignatureError::DateOutOfWindow);
        }
        Ok(())
    }

    /// The keyId the sender claims; resolve it to a public key and call
    /// [`Self::verify_pem`].
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.params.key_id
    }

    /// Checks the cryptographic signature against a PEM public key — SPKI
    /// (`BEGIN PUBLIC KEY`, what Mastodon publishes) or PKCS#1
    /// (`BEGIN RSA PUBLIC KEY`, what several relay implementations and
    /// rss-parrot publish; Mastodon accepts both via OpenSSL).
    pub fn verify_pem(&self, public_key_pem: &str) -> Result<(), SignatureError> {
        // The RFC 7468 PEM decoder is strict about the wire form, but actor
        // keys in the wild are not: Pleroma appends a blank line after the
        // post-encapsulation boundary, and wafrn/Minds serve CRLF line
        // endings. Normalize both before parsing, like OpenSSL would.
        let normalized = public_key_pem.replace('\r', "");
        let trimmed = normalized.trim();
        let key = RsaPublicKey::from_public_key_pem(trimmed)
            .or_else(|_| RsaPublicKey::from_pkcs1_pem(trimmed))
            .map_err(|_| SignatureError::BadPublicKey)?;
        let signature = Signature::try_from(self.params.signature.as_slice())
            .map_err(|_| SignatureError::Invalid)?;
        let verifying = VerifyingKey::<Sha256>::new(key);
        if verifying
            .verify(self.signing_string.as_bytes(), &signature)
            .is_ok()
        {
            return Ok(());
        }
        // Fall back to the path-only `(request-target)` form (Pleroma's).
        if let Some(alt) = &self.signing_string_alt
            && verifying.verify(alt.as_bytes(), &signature).is_ok()
        {
            return Ok(());
        }
        Err(SignatureError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use http::header::{HeaderName, HeaderValue};
    use plamenu_ap::keys::generate_keypair;

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

    #[test]
    fn bodyless_delete_roundtrip() {
        let keys = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &keys.private_pem,
            "https://gateway.example/actor#main-key".to_owned(),
        )
        .unwrap();
        let path = "/.well-known/apgateway-media/hl:zQmTest";
        let request = signer.sign_delete("gateway.example", path, SystemTime::now());
        let headers = header_map(&[
            ("host", "gateway.example"),
            ("date", &request.date),
            ("signature", &request.signature),
        ]);
        let prepared = PreparedVerification::from_bodyless_request(
            "DELETE",
            path,
            &headers,
            SystemTime::now(),
            false,
        )
        .unwrap();
        prepared.verify_pem(&keys.public_pem).unwrap();
    }

    /// Signs a POST and converts the produced headers into a request map.
    fn signed_request(
        signer: &RequestSigner,
        body: &[u8],
        now: SystemTime,
    ) -> (HeaderMap, &'static str) {
        let path = "/users/alice/inbox";
        let signed_headers = signer.sign_post("plamenu.test", path, body, now);
        let headers = header_map(&[
            ("host", &signed_headers.host),
            ("date", &signed_headers.date),
            ("digest", &signed_headers.digest),
            ("signature", &signed_headers.signature),
        ]);
        (headers, path)
    }

    #[test]
    fn parses_mastodon_style_signature_header() {
        let value = r#"keyId="https://mastodon.local/users/alice#main-key",algorithm="rsa-sha256",headers="(request-target) host date digest",signature="c2ln""#;
        let params = parse_signature_header(value).unwrap();
        assert_eq!(params.key_id, "https://mastodon.local/users/alice#main-key");
        assert_eq!(params.algorithm.as_deref(), Some("rsa-sha256"));
        assert_eq!(params.created, None);
        assert_eq!(params.expires, None);
        assert_eq!(
            params.headers,
            ["(request-target)", "host", "date", "digest"]
        );
        assert_eq!(params.signature, b"sig");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_signature_header("not a signature").is_err());
        assert!(parse_signature_header(r#"keyId="unterminated"#).is_err());
        assert!(
            parse_signature_header(r#"signature="c2ln""#).is_err(),
            "missing keyId"
        );
        assert!(
            parse_signature_header(r#"keyId="k",signature="@@@""#).is_err(),
            "bad base64"
        );
    }

    #[test]
    fn sign_verify_roundtrip() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://plamenu.test/users/alice#main-key".into(),
        )
        .unwrap();
        let body = br#"{"type":"Follow"}"#;
        let now = SystemTime::now();
        let (headers, path) = signed_request(&signer, body, now);

        let prepared =
            PreparedVerification::from_request("POST", path, &headers, body, now).unwrap();
        assert_eq!(
            prepared.key_id(),
            "https://plamenu.test/users/alice#main-key"
        );
        prepared.verify_pem(&pair.public_pem).unwrap();
    }

    /// Discourse's `ActivityPub` plugin uses the draft-cavage `hs2019` profile:
    /// `(created)` and `(expires)` are covered pseudo-components whose values
    /// live in Signature header parameters, not physical request headers.
    #[test]
    fn verifies_discourse_hs2019_pseudo_components() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://discourse.test/ap/actor/key#main-key".into(),
        )
        .unwrap();
        let now = SystemTime::now();
        let created = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let expires = (now + Duration::from_hours(1))
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let path = "/users/alice/inbox";
        let host = "plamenu.test";
        let date = httpdate::fmt_http_date(now);
        let body = br#"{"type":"Accept"}"#;
        let digest = digest::compute(body);
        let signing_string = format!(
            "host: {host}\ndate: {date}\ndigest: {digest}\n(request-target): post {path}\n(created): {created}\n(expires): {expires}",
        );
        let encoded = BASE64.encode(signer.sign_bytes(signing_string.as_bytes()));
        let signature = format!(
            "keyId=\"{}\",algorithm=\"hs2019\",headers=\"host date digest (request-target) (created) (expires)\",signature=\"{encoded}\",created=\"{created}\",expires=\"{expires}\"",
            signer.key_id(),
        );
        let headers = header_map(&[
            ("host", host),
            ("date", &date),
            ("digest", &digest),
            ("signature", &signature),
        ]);

        let prepared =
            PreparedVerification::from_request("POST", path, &headers, body, now).unwrap();
        prepared
            .verify_pem(&pair.public_pem)
            .expect("Discourse hs2019 signature must verify");
    }

    #[test]
    fn discourse_pseudo_component_requires_its_signature_parameter() {
        let signature = r#"keyId="k",algorithm="hs2019",headers="(request-target) host date (created)",signature="c2ln""#;
        let date = httpdate::fmt_http_date(SystemTime::now());
        let headers = header_map(&[
            ("host", "plamenu.test"),
            ("date", &date),
            ("signature", signature),
        ]);
        assert!(matches!(
            PreparedVerification::from_get_request("/users/alice", &headers, SystemTime::now()),
            Err(SignatureError::MissingRequestHeader(name)) if name == "(created)"
        ));
    }

    /// Pleroma serves its actor key with a trailing blank line after the PEM
    /// footer (`-----END PUBLIC KEY-----\n\n`), which the strict RFC 7468
    /// decoder rejects as trailing data. `verify_pem` must tolerate it, or
    /// every inbound Pleroma activity fails signature verification.
    #[test]
    fn verify_tolerates_pleroma_trailing_blank_line() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, path) = signed_request(&signer, b"body", now);
        let prepared =
            PreparedVerification::from_request("POST", path, &headers, b"body", now).unwrap();

        // The unmodified key verifies, and so must the Pleroma-style variant
        // with a blank line appended after the footer.
        prepared.verify_pem(&pair.public_pem).unwrap();
        let pleroma_style = format!("{}\n\n", pair.public_pem.trim_end());
        prepared
            .verify_pem(&pleroma_style)
            .expect("a trailing blank line must not break verification");
    }

    /// Several relay implementations (relay.toot.io & co.) and rss-parrot
    /// publish their actor key as PKCS#1 (`BEGIN RSA PUBLIC KEY`) rather than
    /// SPKI. Mastodon accepts both via OpenSSL; so must we, or those relays'
    /// signed Accepts are rejected and the subscription never activates.
    #[test]
    fn verify_accepts_pkcs1_public_key() {
        use rsa::pkcs1::EncodeRsaPublicKey;

        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, path) = signed_request(&signer, b"body", now);
        let prepared =
            PreparedVerification::from_request("POST", path, &headers, b"body", now).unwrap();

        let key = RsaPublicKey::from_public_key_pem(pair.public_pem.trim()).unwrap();
        let pkcs1_pem = key.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        assert!(pkcs1_pem.starts_with("-----BEGIN RSA PUBLIC KEY-----"));
        prepared
            .verify_pem(&pkcs1_pem)
            .expect("a PKCS#1 public key must verify");
    }

    /// Signs a GET the way Pleroma does: the `(request-target)` covers the URL
    /// PATH only (Elixir's `URI.path`), dropping any query string even though
    /// the request line still carries it. Returns the `Signature` header value.
    fn pleroma_path_only_get(
        signer: &RequestSigner,
        host: &str,
        signed_path: &str,
        date: &str,
    ) -> String {
        let signing_string =
            format!("(request-target): get {signed_path}\nhost: {host}\ndate: {date}");
        let signature = BASE64.encode(signer.sign_bytes(signing_string.as_bytes()));
        format!(
            "keyId=\"{}\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date\",signature=\"{signature}\"",
            signer.key_id(),
        )
    }

    /// Pleroma signs a GET's `(request-target)` with the path only, so its
    /// signed fetch of a paginated collection page (`…/followers?page=1`) is
    /// signed over `/users/alice/followers` while the request line carries the
    /// query. `from_get_request` must verify it anyway, or every Pleroma/Akkoma
    /// `internal.fetch` GET of our collections 401s under authorized fetch.
    #[test]
    fn verify_get_accepts_pleroma_path_only_request_target() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://pleroma.test/internal/fetch#main-key".into(),
        )
        .unwrap();
        let now = SystemTime::now();
        let host = "plamenu.test";
        let date = httpdate::fmt_http_date(now);
        let full = "/users/alice/followers?page=1";

        // Signed over the path only; the actual request carries `?page=1`.
        let sig = pleroma_path_only_get(&signer, host, "/users/alice/followers", &date);
        let headers = header_map(&[("host", host), ("date", &date), ("signature", &sig)]);
        let prepared = PreparedVerification::from_get_request(full, &headers, now).unwrap();
        prepared
            .verify_pem(&pair.public_pem)
            .expect("Pleroma's path-only request-target must verify a ?page= request");

        // A normal actor does not get the same leniency: leaving its query
        // unsigned would allow replay against another query on the same path.
        let normal_signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://pleroma.test/users/alice#main-key".into(),
        )
        .unwrap();
        let normal = pleroma_path_only_get(&normal_signer, host, "/users/alice/followers", &date);
        let normal_headers = header_map(&[("host", host), ("date", &date), ("signature", &normal)]);
        let normal_prepared =
            PreparedVerification::from_get_request(full, &normal_headers, now).unwrap();
        assert!(
            normal_prepared.verify_pem(&pair.public_pem).is_err(),
            "ordinary actor signatures must continue to cover the query string"
        );

        // The leniency is scoped to the SAME path: a signature over a different
        // collection's path must NOT verify the followers request.
        let wrong = pleroma_path_only_get(&signer, host, "/users/alice/following", &date);
        let headers_wrong = header_map(&[("host", host), ("date", &date), ("signature", &wrong)]);
        let prepared_wrong =
            PreparedVerification::from_get_request(full, &headers_wrong, now).unwrap();
        assert!(
            prepared_wrong.verify_pem(&pair.public_pem).is_err(),
            "a path-only signature for a different path must still be rejected"
        );
    }

    /// wafrn and Minds serve their actor key with CRLF line endings, which the
    /// strict RFC 7468 decoder rejects. `verify_pem` must normalize them.
    #[test]
    fn verify_tolerates_crlf_line_endings() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, path) = signed_request(&signer, b"body", now);
        let prepared =
            PreparedVerification::from_request("POST", path, &headers, b"body", now).unwrap();

        let crlf_pem = pair.public_pem.replace('\n', "\r\n");
        prepared
            .verify_pem(&crlf_pem)
            .expect("CRLF line endings must not break verification");
    }

    /// Signs a GET and converts the produced headers into a request map.
    fn signed_get_request(
        signer: &RequestSigner,
        accept: &str,
        now: SystemTime,
    ) -> (HeaderMap, &'static str) {
        let path = "/users/alice";
        let signed_headers = signer.sign_get("plamenu.test", path, accept, now);
        let headers = header_map(&[
            ("host", "plamenu.test"),
            ("date", &signed_headers.date),
            ("accept", accept),
            ("signature", &signed_headers.signature),
        ]);
        (headers, path)
    }

    #[test]
    fn get_sign_verify_roundtrip() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://plamenu.test/actor#main-key".into(),
        )
        .unwrap();
        let now = SystemTime::now();
        let (headers, path) = signed_get_request(&signer, "application/activity+json", now);

        let prepared = PreparedVerification::from_get_request(path, &headers, now).unwrap();
        assert_eq!(prepared.key_id(), "https://plamenu.test/actor#main-key");
        prepared.verify_pem(&pair.public_pem).unwrap();

        // A different path (replay elsewhere) must not verify.
        let moved = PreparedVerification::from_get_request("/users/bob", &headers, now).unwrap();
        assert!(matches!(
            moved.verify_pem(&pair.public_pem),
            Err(SignatureError::Invalid)
        ));
    }

    #[test]
    fn get_signature_must_cover_host() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (mut headers, path) = signed_get_request(&signer, "application/activity+json", now);

        let original = headers
            .get("signature")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let weakened = original.replace(
            r#"headers="(request-target) host date accept""#,
            r#"headers="(request-target) date accept""#,
        );
        headers.insert("signature", HeaderValue::from_str(&weakened).unwrap());
        assert!(matches!(
            PreparedVerification::from_get_request(path, &headers, now),
            Err(SignatureError::UncoveredHeader("host"))
        ));
    }

    #[test]
    fn get_verification_rejects_stale_dates() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, path) = signed_get_request(&signer, "application/activity+json", now);
        assert!(matches!(
            PreparedVerification::from_get_request(path, &headers, now + Duration::from_hours(13)),
            Err(SignatureError::DateOutOfWindow)
        ));
    }

    #[test]
    fn verification_fails_with_wrong_key() {
        let pair = generate_keypair().unwrap();
        let other = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, path) = signed_request(&signer, b"body", now);

        let prepared =
            PreparedVerification::from_request("POST", path, &headers, b"body", now).unwrap();
        assert!(matches!(
            prepared.verify_pem(&other.public_pem),
            Err(SignatureError::Invalid)
        ));
    }

    #[test]
    fn tampered_body_fails_digest_check() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, path) = signed_request(&signer, b"original", now);

        let result = PreparedVerification::from_request("POST", path, &headers, b"tampered", now);
        assert!(matches!(result, Err(SignatureError::DigestMismatch)));
    }

    #[test]
    fn stale_and_future_dates_are_rejected() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();

        let (headers, path) = signed_request(&signer, b"body", now);
        let later = now + Duration::from_hours(13);
        assert!(matches!(
            PreparedVerification::from_request("POST", path, &headers, b"body", later),
            Err(SignatureError::DateOutOfWindow)
        ));

        let (headers, path) = signed_request(&signer, b"body", now + Duration::from_hours(2));
        assert!(matches!(
            PreparedVerification::from_request("POST", path, &headers, b"body", now),
            Err(SignatureError::DateOutOfWindow)
        ));
    }

    #[test]
    fn signature_must_cover_required_headers() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (mut headers, path) = signed_request(&signer, b"body", now);

        // Rewrite the Signature header to claim it only covers `date`.
        let original = headers
            .get("signature")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let weakened = original.replace(
            r#"headers="(request-target) host date digest""#,
            r#"headers="date""#,
        );
        headers.insert("signature", HeaderValue::from_str(&weakened).unwrap());
        assert!(matches!(
            PreparedVerification::from_request("POST", path, &headers, b"body", now),
            Err(SignatureError::UncoveredHeader("(request-target)"))
        ));
    }

    #[test]
    fn moved_request_target_fails_verification() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(&pair.private_pem, "k".into()).unwrap();
        let now = SystemTime::now();
        let (headers, _) = signed_request(&signer, b"body", now);

        // Same headers replayed against a different path.
        let prepared =
            PreparedVerification::from_request("POST", "/inbox", &headers, b"body", now).unwrap();
        assert!(matches!(
            prepared.verify_pem(&pair.public_pem),
            Err(SignatureError::Invalid)
        ));
    }

    #[test]
    fn missing_signature_header_is_distinct_error() {
        let headers = HeaderMap::new();
        assert!(matches!(
            PreparedVerification::from_request("POST", "/inbox", &headers, b"", SystemTime::now()),
            Err(SignatureError::MissingSignatureHeader)
        ));
    }
}
