//! Quote interaction policies (FEP-044f / Mastodon `quote_approval_policy`).
//!
//! The policy is an `i32` bitmap matching Mastodon's encoding exactly: the high
//! 16 bits are the *automatic*-approval sub-policy, the low 16 the *manual* one.
//! Each sub-policy is itself a small bitmap of audience flags. A value of `0`
//! means nobody may quote.
//!
//! Mastodon's client API only ever sets automatic policies (`public`,
//! `followers`, `nobody`); manual policies arrive solely over federation, so
//! local posts never use them.

use serde_json::{Value, json};

use crate::activity::PUBLIC;

/// Audience flags within a sub-policy (Mastodon's `InteractionPolicy::POLICY_FLAGS`).
pub mod flag {
    /// An audience Mastodon (and we) cannot evaluate.
    pub const UNSUPPORTED: i32 = 1 << 0;
    /// Everyone may interact.
    pub const PUBLIC: i32 = 1 << 1;
    /// Only the author's followers may interact.
    pub const FOLLOWERS: i32 = 1 << 2;
    /// Only accounts the author follows may interact.
    pub const FOLLOWING: i32 = 1 << 3;
    /// All interaction explicitly disabled (only the author themselves).
    pub const DISABLED: i32 = 1 << 4;
}

/// The automatic-`public` policy: anyone may quote without asking. This is the
/// default Mastodon (and we) apply to a freshly-posted distributable status.
pub const AUTOMATIC_PUBLIC: i32 = flag::PUBLIC << 16;
/// The automatic-`followers` policy: the author's followers may quote.
pub const AUTOMATIC_FOLLOWERS: i32 = flag::FOLLOWERS << 16;

/// A decoded quote-approval policy bitmap.
#[derive(Debug, Clone, Copy)]
pub struct QuotePolicy {
    automatic: i32,
    manual: i32,
}

impl QuotePolicy {
    /// Decodes the stored bitmap.
    #[must_use]
    pub fn from_bitmap(bitmap: i32) -> Self {
        Self {
            automatic: (bitmap >> 16) & 0xFFFF,
            manual: bitmap & 0xFFFF,
        }
    }

    /// The automatic sub-policy flags.
    #[must_use]
    pub fn automatic(self) -> SubPolicy {
        SubPolicy(self.automatic)
    }

    /// The manual sub-policy flags.
    #[must_use]
    pub fn manual(self) -> SubPolicy {
        SubPolicy(self.manual)
    }
}

/// One sub-policy (automatic or manual): a bitmap of audience flags.
#[derive(Debug, Clone, Copy)]
pub struct SubPolicy(i32);

impl SubPolicy {
    #[must_use]
    pub fn public(self) -> bool {
        self.0 & flag::PUBLIC != 0
    }

    #[must_use]
    pub fn followers(self) -> bool {
        self.0 & flag::FOLLOWERS != 0
    }

    #[must_use]
    pub fn following(self) -> bool {
        self.0 & flag::FOLLOWING != 0
    }

    #[must_use]
    pub fn unsupported(self) -> bool {
        self.0 & flag::UNSUPPORTED != 0
    }

    /// The Mastodon `quote_approval` key list for this sub-policy, e.g.
    /// `["public"]`. Order matches Mastodon's `POLICY_FLAGS` declaration.
    #[must_use]
    pub fn keys(self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if self.0 & flag::UNSUPPORTED != 0 {
            keys.push("unsupported_policy");
        }
        if self.public() {
            keys.push("public");
        }
        if self.followers() {
            keys.push("followers");
        }
        if self.following() {
            keys.push("following");
        }
        if self.0 & flag::DISABLED != 0 {
            keys.push("disabled");
        }
        keys
    }
}

/// Maps the REST `quote_approval_policy` string (the only values Mastodon's
/// `interaction_policy` endpoint accepts) onto a stored bitmap. Returns `None`
/// for an unrecognized value.
#[must_use]
pub fn from_client_string(value: &str) -> Option<i32> {
    match value {
        "public" => Some(AUTOMATIC_PUBLIC),
        "followers" => Some(AUTOMATIC_FOLLOWERS),
        "nobody" => Some(0),
        _ => None,
    }
}

/// An outgoing `interactionPolicy` branch (`canQuote`, `canFeature`, ...).
/// Only automatic approval is federated (Mastodon does the same); when no
/// audience is granted the actor's own IRI is listed as the sole approver,
/// like Mastodon's self-fallback.
#[must_use]
pub fn capability_policy_json(
    capability: &str,
    bitmap: i32,
    actor_uri: &str,
    followers_url: &str,
    following_url: &str,
) -> Value {
    let automatic = QuotePolicy::from_bitmap(bitmap).automatic();
    let mut approved = Vec::new();
    if automatic.public() {
        approved.push(PUBLIC.to_owned());
    }
    if automatic.followers() {
        approved.push(followers_url.to_owned());
    }
    if automatic.following() {
        approved.push(following_url.to_owned());
    }
    if approved.is_empty() {
        approved.push(actor_uri.to_owned());
    }
    json!({ capability: { "automaticApproval": approved } })
}

/// The Note's outgoing `interactionPolicy.canQuote` object.
#[must_use]
pub fn interaction_policy_json(
    bitmap: i32,
    actor_uri: &str,
    followers_url: &str,
    following_url: &str,
) -> Value {
    capability_policy_json("canQuote", bitmap, actor_uri, followers_url, following_url)
}

/// Parses one inbound `interactionPolicy` branch into a stored bitmap.
/// `actor_uri`/`followers_url`/`following_url` identify the note author's known
/// audience IRIs; any other listed actor becomes the `unsupported` flag, and a
/// sub-policy naming only the author themselves becomes `disabled` — exactly
/// Mastodon's `InteractionPolicyParser`.
#[must_use]
pub fn parse_capability_policy(
    interaction_policy: &Value,
    capability: &str,
    actor_uri: &str,
    followers_url: &str,
    following_url: &str,
) -> i32 {
    let Some(branch) = interaction_policy.get(capability) else {
        return 0;
    };
    let sub = |key: &str| -> i32 {
        sub_policy_flags(branch.get(key), actor_uri, followers_url, following_url)
    };
    let automatic = sub("automaticApproval");
    let manual = sub("manualApproval");
    (automatic << 16) | manual
}

/// Parses an inbound note's `interactionPolicy.canQuote`.
#[must_use]
pub fn parse_interaction_policy(
    interaction_policy: &Value,
    actor_uri: &str,
    followers_url: &str,
    following_url: &str,
) -> i32 {
    parse_capability_policy(
        interaction_policy,
        "canQuote",
        actor_uri,
        followers_url,
        following_url,
    )
}

fn sub_policy_flags(
    audience: Option<&Value>,
    actor_uri: &str,
    followers_url: &str,
    following_url: &str,
) -> i32 {
    let is_public = |s: &str| s == PUBLIC || s == "as:Public" || s == "Public";
    let mut flags = 0;
    let mut includes_self = false;
    let mut has_unknown = false;
    for item in audience_iter(audience) {
        if is_public(item) {
            flags |= flag::PUBLIC;
        } else if !followers_url.is_empty() && item == followers_url {
            flags |= flag::FOLLOWERS;
        } else if !following_url.is_empty() && item == following_url {
            flags |= flag::FOLLOWING;
        } else if item == actor_uri {
            includes_self = true;
        } else {
            has_unknown = true;
        }
    }
    if has_unknown {
        flags |= flag::UNSUPPORTED;
    }
    if flags == 0 && includes_self {
        flags |= flag::DISABLED;
    }
    flags
}

/// The audience of a sub-policy can be a single IRI or an array of them; each
/// item may be a bare string or an object with an `id`.
fn audience_iter(audience: Option<&Value>) -> Vec<&str> {
    match audience {
        Some(Value::Array(items)) => items.iter().filter_map(value_iri).collect(),
        Some(other) => value_iri(other).into_iter().collect(),
        None => Vec::new(),
    }
}

fn value_iri(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s.as_str()),
        Value::Object(map) => map.get("id").and_then(Value::as_str),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLLOWERS: &str = "https://plamenu.test/users/alice/followers";
    const FOLLOWING: &str = "https://plamenu.test/users/alice/following";
    const ACTOR: &str = "https://plamenu.test/users/alice";

    #[test]
    fn client_string_maps_to_automatic_bitmap() {
        assert_eq!(from_client_string("public"), Some(flag::PUBLIC << 16));
        assert_eq!(from_client_string("followers"), Some(flag::FOLLOWERS << 16));
        assert_eq!(from_client_string("nobody"), Some(0));
        assert_eq!(from_client_string("everyone"), None);
    }

    #[test]
    fn public_policy_round_trips() {
        let bitmap = AUTOMATIC_PUBLIC;
        let policy = QuotePolicy::from_bitmap(bitmap);
        assert!(policy.automatic().public());
        assert_eq!(policy.automatic().keys(), vec!["public"]);
        assert!(policy.manual().keys().is_empty());
        let json = interaction_policy_json(bitmap, ACTOR, FOLLOWERS, FOLLOWING);
        assert_eq!(json["canQuote"]["automaticApproval"], json!([PUBLIC]));
    }

    #[test]
    fn followers_policy_emits_followers_url() {
        let json = interaction_policy_json(AUTOMATIC_FOLLOWERS, ACTOR, FOLLOWERS, FOLLOWING);
        assert_eq!(json["canQuote"]["automaticApproval"], json!([FOLLOWERS]));
        let policy = QuotePolicy::from_bitmap(AUTOMATIC_FOLLOWERS);
        assert_eq!(policy.automatic().keys(), vec!["followers"]);
    }

    #[test]
    fn nobody_policy_falls_back_to_self() {
        let json = interaction_policy_json(0, ACTOR, FOLLOWERS, FOLLOWING);
        assert_eq!(json["canQuote"]["automaticApproval"], json!([ACTOR]));
    }

    #[test]
    fn parse_public() {
        let policy = json!({ "canQuote": { "automaticApproval": [PUBLIC] } });
        let bitmap = parse_interaction_policy(&policy, ACTOR, FOLLOWERS, FOLLOWING);
        assert_eq!(bitmap, AUTOMATIC_PUBLIC);
    }

    #[test]
    fn parse_followers_and_manual() {
        let policy = json!({
            "canQuote": {
                "automaticApproval": FOLLOWERS,
                "manualApproval": [PUBLIC],
            }
        });
        let bitmap = parse_interaction_policy(&policy, ACTOR, FOLLOWERS, FOLLOWING);
        let decoded = QuotePolicy::from_bitmap(bitmap);
        assert!(decoded.automatic().followers());
        assert!(decoded.manual().public());
    }

    #[test]
    fn parse_unknown_actor_is_unsupported() {
        let policy = json!({
            "canQuote": { "automaticApproval": ["https://other.test/users/bob"] }
        });
        let bitmap = parse_interaction_policy(&policy, ACTOR, FOLLOWERS, FOLLOWING);
        assert!(QuotePolicy::from_bitmap(bitmap).automatic().unsupported());
    }

    #[test]
    fn parse_self_only_is_disabled() {
        let policy = json!({ "canQuote": { "automaticApproval": [ACTOR] } });
        let bitmap = parse_interaction_policy(&policy, ACTOR, FOLLOWERS, FOLLOWING);
        assert_eq!(
            QuotePolicy::from_bitmap(bitmap).automatic().keys(),
            vec!["disabled"]
        );
    }

    #[test]
    fn parse_missing_policy_is_zero() {
        assert_eq!(
            parse_interaction_policy(&json!({}), ACTOR, FOLLOWERS, FOLLOWING),
            0
        );
    }
}
