//! `ActivityPub` protocol surface shared across Plamenu.
//!
//! This crate owns everything a remote server can see: the JSON vocabulary
//! (actors, and later activities/objects), webfinger and nodeinfo documents,
//! URL layout for local resources, and actor key material.

pub mod acct;
pub mod activity;
pub mod actor;
pub mod collection;
pub mod emoji;
pub mod featured;
pub mod hashlink;
pub mod identity;
pub mod keys;
pub mod multikey;
pub mod nodeinfo;
pub mod proof;
pub mod quote_policy;
pub mod text;
pub mod urls;
pub mod webfinger;

/// Media type for `ActivityPub` payloads (the wire default used by Mastodon).
pub const ACTIVITY_JSON: &str = "application/activity+json";
/// Full response content type for `ActivityPub` payloads.
pub const ACTIVITY_JSON_UTF8: &str = "application/activity+json; charset=utf-8";
/// The JSON-LD media type with the `ActivityStreams` profile, also accepted on the wire.
pub const LD_JSON_AS: &str =
    "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"";
/// Response content type for webfinger JRD documents.
pub const JRD_JSON_UTF8: &str = "application/jrd+json; charset=utf-8";
/// Response content type for the XRD form of host-meta documents.
pub const XRD_XML_UTF8: &str = "application/xrd+xml; charset=utf-8";
/// Response content type for a per-account Atom syndication feed.
pub const ATOM_XML_UTF8: &str = "application/atom+xml; charset=utf-8";

/// The `ActivityStreams` JSON-LD context IRI.
pub const AS_CONTEXT: &str = "https://www.w3.org/ns/activitystreams";
/// The security vocabulary context IRI (HTTP signature public keys).
pub const SECURITY_CONTEXT: &str = "https://w3id.org/security/v1";
/// The W3C Controlled Identifiers context IRI (`Multikey`, FEP-521a).
pub const CID_CONTEXT: &str = "https://www.w3.org/ns/cid/v1";
/// The W3C Data Integrity context IRI (`DataIntegrityProof`, FEP-8b32).
pub const DATA_INTEGRITY_CONTEXT: &str = "https://w3id.org/security/data-integrity/v2";
/// The namespace for terms this project originates, named after the source
/// repository the way Lemmy (`join-lemmy.org/ns#`) and `GoToSocial`
/// (`gotosocial.org/ns#`) name theirs. Only for facts no deployed vocabulary
/// can express — every term that has a borrowed spelling keeps it, so peers
/// reading the common vocabularies lose nothing.
pub const PLAMENU_NS: &str = "https://codefloe.com/plamenu/plamenu/ns#";

/// Returns `true` when an HTTP `Accept` header value asks for an `ActivityPub`
/// representation (`activity+json`, `ActivityStreams` `ld+json`, or plain JSON).
#[must_use]
pub fn accepts_activity_json(accept: &str) -> bool {
    accept.split(',').any(|part| {
        let mime = part.split(';').next().unwrap_or("").trim();
        matches!(
            mime,
            "application/activity+json" | "application/ld+json" | "application/json"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::accepts_activity_json;

    #[test]
    fn accepts_ap_media_types() {
        assert!(accepts_activity_json("application/activity+json"));
        assert!(accepts_activity_json(
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
        ));
        assert!(accepts_activity_json("application/json"));
        assert!(accepts_activity_json(
            "text/html, application/activity+json;q=0.9"
        ));
    }

    #[test]
    fn rejects_non_ap_media_types() {
        assert!(!accepts_activity_json("text/html"));
        assert!(!accepts_activity_json("*/*"));
        assert!(!accepts_activity_json("text/html, application/xhtml+xml"));
        assert!(!accepts_activity_json(""));
    }
}
