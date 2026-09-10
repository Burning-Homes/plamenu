//! `ActivityPub` actor documents.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::acct::Acct;
use crate::urls::LocalUserUrls;
use crate::{AS_CONTEXT, CID_CONTEXT, DATA_INTEGRITY_CONTEXT, PLAMENU_NS, SECURITY_CONTEXT};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActorType {
    Person,
    Service,
    Application,
    Group,
    Organization,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicKey {
    pub id: String,
    pub owner: String,
    pub public_key_pem: String,
}

#[derive(Debug, Clone)]
enum RemotePublicKeyEntry {
    Inline(PublicKey),
    Reference(String),
}

/// The lenient classic `publicKey` input shape used by remote actors.
///
/// Deployed actors use either one inline object, an array of objects, or key
/// document references. `Deref` preserves the long-standing first-inline-key
/// compatibility API while verification ingestion consumes every entry.
#[derive(Debug, Clone, Default)]
pub struct RemotePublicKeys {
    entries: Vec<RemotePublicKeyEntry>,
    fallback: PublicKey,
}

impl From<PublicKey> for RemotePublicKeys {
    fn from(value: PublicKey) -> Self {
        Self {
            entries: vec![RemotePublicKeyEntry::Inline(value)],
            fallback: PublicKey::default(),
        }
    }
}

impl std::ops::Deref for RemotePublicKeys {
    type Target = PublicKey;

    fn deref(&self) -> &Self::Target {
        self.entries
            .iter()
            .find_map(|entry| match entry {
                RemotePublicKeyEntry::Inline(key) => Some(key),
                RemotePublicKeyEntry::Reference(_) => None,
            })
            .unwrap_or(&self.fallback)
    }
}

impl std::ops::DerefMut for RemotePublicKeys {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.entries
            .iter_mut()
            .find_map(|entry| match entry {
                RemotePublicKeyEntry::Inline(key) => Some(key),
                RemotePublicKeyEntry::Reference(_) => None,
            })
            .unwrap_or(&mut self.fallback)
    }
}

impl<'de> Deserialize<'de> for RemotePublicKeys {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<Value>::deserialize(deserializer)?;
        let values = match value {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => values,
            Some(value) => vec![value],
        };
        let mut entries = Vec::with_capacity(values.len());
        for value in values {
            match value {
                Value::String(uri) => entries.push(RemotePublicKeyEntry::Reference(uri)),
                Value::Object(map) => {
                    let key = serde_json::from_value(Value::Object(map))
                        .map_err(serde::de::Error::custom)?;
                    entries.push(RemotePublicKeyEntry::Inline(key));
                }
                _ => return Err(serde::de::Error::custom("invalid publicKey entry")),
            }
        }
        Ok(Self {
            entries,
            fallback: PublicKey::default(),
        })
    }
}

/// One bounded remote verification method ready for normalized storage, or a
/// reference that the guarded federation fetcher must resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteVerificationMethod {
    pub key_uri: String,
    pub controller_uri: String,
    pub algorithm: Option<String>,
    /// PEM for RSA; original public Multikey for Ed25519/ML-DSA-44. `None`
    /// means `assertionMethod` named an external key document.
    pub public_key: Option<String>,
    pub source: &'static str,
    pub expires_at: Option<time::OffsetDateTime>,
    pub revoked: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerificationMethodError {
    #[error("actor publishes more than ten FEP-521a or ten classic verification methods")]
    TooMany,
    #[error("verification method has a wrong controller or malformed id")]
    WrongController,
    #[error("verification method id is duplicated with conflicting material")]
    ConflictingDuplicate,
    #[error("verification method contains invalid key material: {0}")]
    InvalidKey(#[from] crate::multikey::MultikeyError),
    #[error("verification method contains an invalid lifecycle timestamp")]
    InvalidLifecycle,
}

/// A FEP-521a verification method: a Multikey under `assertionMethod`. We
/// publish the Ed25519 key this way, matching Mitra's `#ed25519-key`
/// convention, and mirror the RSA key here too under its existing
/// `#main-key` id — see [`Multikey::rsa`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Multikey {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub controller: String,
    pub public_key_multibase: String,
}

impl Multikey {
    #[must_use]
    pub fn ed25519(id: String, controller: String, public_key_multibase: &str) -> Self {
        Self {
            id,
            kind: "Multikey".to_owned(),
            controller,
            public_key_multibase: public_key_multibase.to_owned(),
        }
    }

    /// The RSA signing key, republished as an `rsa-pub` Multikey under the
    /// same `#main-key` id its legacy `publicKey` block uses.
    ///
    /// Mastodon 4.7 stores a remote actor's keys in one table keyed by the
    /// verification-method id, and builds it from `assertionMethod`; builds
    /// between 2026-06-19 and 2026-07-06 (mastodon#39725) let that *replace*
    /// `publicKey` outright, dropping the RSA key of every actor that
    /// publishes a FEP-521a method and answering its draft-cavage signatures
    /// with `401 Public key not found for key …#main-key`. Publishing the
    /// same key under both shapes keeps `#main-key` resolvable there; on
    /// fixed builds the two entries carry identical material and dedupe by
    /// id. `None` when the PEM does not parse.
    #[must_use]
    pub fn rsa(id: String, controller: String, public_key_pem: &str) -> Option<Self> {
        Some(Self {
            id,
            kind: "Multikey".to_owned(),
            controller,
            public_key_multibase: crate::multikey::encode_rsa_public(public_key_pem)?,
        })
    }
}

/// The `assertionMethod` list an actor document publishes: the FEP-521a
/// Ed25519 Multikey (absent until the backfill has run) followed by the RSA
/// signing key mirrored under its `#main-key` id.
///
/// The Ed25519 entry stays first so a peer that takes "the first
/// `assertionMethod` entry" as the FEP-8b32 verification method still finds
/// it; the RSA mirror is only ever looked up by id.
fn verification_methods(
    actor_id: &str,
    key_id: String,
    ed25519_key_id: String,
    public_key_pem: &str,
    ed25519_public_multibase: Option<&str>,
) -> Vec<Multikey> {
    let mut methods = Vec::with_capacity(2);
    if let Some(key) = ed25519_public_multibase {
        methods.push(Multikey::ed25519(ed25519_key_id, actor_id.to_owned(), key));
    }
    methods.extend(Multikey::rsa(key_id, actor_id.to_owned(), public_key_pem));
    methods
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoints {
    pub shared_inbox: String,
}

/// An avatar (`icon`) or header (`image`) on an actor document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub url: String,
    /// Alt text, published as `summary` like Mastodon's `ImageSerializer`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl Image {
    #[must_use]
    pub fn new(url: String, media_type: &str) -> Self {
        Self {
            kind: "Image".to_owned(),
            media_type: Some(media_type.to_owned()),
            url,
            summary: None,
        }
    }

    /// Attaches alt text (`summary`), dropping an empty description.
    #[must_use]
    pub fn with_summary(mut self, description: &str) -> Self {
        self.summary = (!description.is_empty()).then(|| description.to_owned());
        self
    }
}

/// A profile metadata field, the way Mastodon publishes them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyValue {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub value: String,
}

impl PropertyValue {
    #[must_use]
    pub fn new(name: String, value: String) -> Self {
        Self {
            kind: "PropertyValue".to_owned(),
            name,
            value,
        }
    }
}

/// An `ActivityPub` actor document, shaped the way Mastodon publishes and
/// expects them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Mastodon's actor flags"
)]
pub struct Actor {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ActorType,
    pub preferred_username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub url: String,
    pub inbox: String,
    pub outbox: String,
    pub followers: String,
    pub following: String,
    /// The pinned-statuses collection (`toot:featured`).
    pub featured: String,
    /// The featured-hashtags collection (`toot:featuredTags`), which peers
    /// dereference to sync a profile's featured tags.
    pub featured_tags: String,
    /// The account's `featuredCollections` endpoint (FEP-7aa9). Emitted as a
    /// bare property, like Mastodon (no JSON-LD term defines it).
    pub featured_collections: String,
    /// RFC 3339 timestamp of account creation.
    pub published: String,
    pub manually_approves_followers: bool,
    /// Discoverability opt-in (`toot:discoverable`); Mastodon always emits it
    /// as a bool (`discoverable || false`).
    pub discoverable: bool,
    /// Public-post search opt-in (`toot:indexable`); Mastodon always emits it
    /// as a bool (`indexable || false`).
    pub indexable: bool,
    /// Domains authorized to attribute web content to this account via
    /// `fediverse:creator` (`toot:attributionDomains`); Mastodon emits the
    /// property only when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attribution_domains: Vec<String>,
    /// In-memoriam marker (`toot:memorial`); Mastodon always emits it.
    pub memorial: bool,
    /// Temporary-suspension marker (`toot:suspended`). Mastodon emits it only
    /// on suspended actors (never `suspended: false`), alongside a blanked
    /// document — see `LocalActorParams::suspended`.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub suspended: bool,
    /// Profile-tab settings (`toot:showFeatured` / `toot:showMedia` /
    /// `toot:showRepliesInMedia`), always emitted like Mastodon.
    pub show_featured: bool,
    pub show_media: bool,
    pub show_replies_in_media: bool,
    /// Account-level interaction policy (FEP-7aa9 `canFeature`).
    pub interaction_policy: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<Image>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
    #[serde(default)]
    pub attachment: Vec<Value>,
    /// `Emoji` tag objects for custom emoji in the name/bio/fields, plus a
    /// `Hashtag` object per featured tag — the same mix Mastodon's
    /// `virtual_tags` emits.
    #[serde(default)]
    pub tag: Vec<Value>,
    #[serde(default)]
    pub public_key: PublicKey,
    /// FEP-521a verification methods: the Ed25519 Multikey (used by FEP-8b32
    /// proofs and RFC 9421 Ed25519 signatures). Omitted while the account has
    /// no Ed25519 key (pre-backfill).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertion_method: Vec<Multikey>,
    pub endpoints: Endpoints,
    /// Other actors the same person controls (`alsoKnownAs`); omitted when
    /// empty, like Mastodon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_known_as: Vec<String>,
    /// The actor this one has migrated to (`movedTo`); omitted until set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<String>,
    /// FEP-844e capability discovery: the publishing software as an
    /// `Application` whose `implements` lists supported mechanisms. User
    /// actors carry it under `generator` (Mitra's convention); the instance
    /// actor lists `implements` at the top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<Value>,
    /// Group actors only: the moderators collection (FEP-1b12; Lemmy reads
    /// a community's mod list from here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributed_to: Option<String>,
    /// Group actors only: the FEP-5219 `affiliations` collection. Emitted as
    /// a bare property, like Mitra (no JSON-LD term defines it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affiliations: Option<String>,
    /// Group actors only: only moderators may start threads (Lemmy's
    /// `postingRestrictedToMods` extension).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posting_restricted_to_mods: Option<bool>,
    /// Group actors only: community-wide NSFW flag (`as:sensitive`, the way
    /// Lemmy publishes it on communities).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitive: Option<bool>,
    /// Group actors only: the posting policy as a tri-state
    /// (`anyone`/`members`/`mods`), under our own term because no deployed
    /// vocabulary expresses "members only" — see [`GroupActorParams`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posting_policy: Option<String>,
}

/// The Group-specific knobs of a local group actor; `Some` switches
/// [`Actor::local_person`] into its Group branch.
#[derive(Debug, Clone, Copy)]
pub struct GroupActorParams<'a> {
    pub posting_restricted_to_mods: bool,
    pub sensitive: bool,
    /// The full posting policy (`anyone`/`members`/`mods`), published under
    /// our own term. Lemmy's vocabulary has only the mods-or-not boolean
    /// above, so a members-only community is indistinguishable on the wire
    /// from an open one — including to another Plamenu, which is what this
    /// term fixes. Peers that don't know it fall back to the boolean.
    pub posting_policy: &'a str,
}

/// Everything needed to render a local account as an actor document.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Mastodon's actor flags"
)]
pub struct LocalActorParams<'a> {
    pub domain: &'a str,
    pub username: &'a str,
    /// Persisted canonical `ActivityPub` actor ID. `None` is the legacy
    /// username-derived layout used by old fixtures and pre-backfill rows.
    pub actor_id: Option<&'a str>,
    pub display_name: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub public_key_pem: &'a str,
    /// RFC 3339.
    pub published: String,
    /// Avatar / header, when uploaded.
    pub icon: Option<Image>,
    pub image: Option<Image>,
    /// Profile metadata fields, values already rendered to HTML.
    pub fields: Vec<PropertyValue>,
    /// Pre-built `Emoji` tag objects for custom emoji in the profile text,
    /// plus `Hashtag` objects for featured tags ([`profile_hashtag_tag`]).
    pub emoji_tags: Vec<Value>,
    /// Locked account: followers must be approved manually.
    pub locked: bool,
    /// Discoverability opt-in (`toot:discoverable`).
    pub discoverable: bool,
    /// Automated account: serialized as actor `type: Service`.
    pub bot: bool,
    /// Public-post search opt-in (`toot:indexable`).
    pub indexable: bool,
    /// Domains authorized for `fediverse:creator` attribution
    /// (`toot:attributionDomains`).
    pub attribution_domains: Vec<String>,
    /// In-memoriam marker (`toot:memorial`).
    pub memorial: bool,
    /// Temporarily suspended (`toot:suspended`). The caller passes a blanked
    /// profile alongside (Mastodon's `unavailable?` serialization) — this
    /// struct doesn't blank anything itself.
    pub suspended: bool,
    /// Profile-tab settings (`toot:showFeatured` / `toot:showMedia` /
    /// `toot:showRepliesInMedia`).
    pub show_featured: bool,
    pub show_media: bool,
    pub show_media_replies: bool,
    /// Declared aliases (`alsoKnownAs`): other actors this person controls.
    pub also_known_as: Vec<String>,
    /// The actor this account has migrated to (`movedTo`), if any.
    pub moved_to: Option<String>,
    /// The account's Ed25519 public Multikey (`z6Mk…`), published as a
    /// FEP-521a `assertionMethod`; `None` while the backfill hasn't run.
    pub ed25519_public_multibase: Option<&'a str>,
    /// `Some` for a local group: serializes as `type: Group` with the
    /// moderators/affiliations collections and Lemmy's community flags.
    pub group: Option<GroupActorParams<'a>>,
}

/// FEP-844e: the mechanisms this server implements, advertised as
/// `{name, href}` features (Mitra's wire shape — the only deployed consumer
/// convention). Only list what is actually shipped and e2e-verified; peers
/// may use this to decide protocol variants without probing.
#[must_use]
pub fn implemented_capabilities() -> Vec<Value> {
    vec![
        json!({
            "name": "FEP-8fcf: Followers collection synchronization across servers",
            "href": "https://w3id.org/fep/8fcf",
        }),
        json!({
            "name": "FEP-7aa9: Account collections and featuring policy",
            "href": "https://w3id.org/fep/7aa9",
        }),
        json!({
            "name": "FEP-521a: Representing actor's public keys",
            "href": "https://w3id.org/fep/521a",
        }),
        json!({
            "name": "FEP-8b32: Object Integrity Proofs",
            "href": "https://w3id.org/fep/8b32",
        }),
        json!({
            "name": "FEP-044f: Consent-respecting quote posts",
            "href": "https://w3id.org/fep/044f",
        }),
        json!({
            "name": "FEP-e232: Object Links",
            "href": "https://w3id.org/fep/e232",
        }),
        json!({
            "name": "RFC-9421: HTTP Message Signatures",
            "href": "https://datatracker.ietf.org/doc/html/rfc9421",
        }),
        json!({
            "name": "RFC-9421 signatures using the Ed25519 algorithm",
            "href": "https://datatracker.ietf.org/doc/html/rfc9421#name-eddsa-using-curve-edwards25",
        }),
    ]
}

/// A profile `Hashtag` tag object for a featured tag, the shape Mastodon's
/// `HashtagSerializer` emits in the actor `tag` array and the `featuredTags`
/// collection: `href` is the profile's tagged view, `name` the
/// `#`-prefixed tag.
#[must_use]
pub fn profile_hashtag_tag(domain: &str, username: &str, tag_name: &str) -> Value {
    json!({
        "type": "Hashtag",
        "href": format!("https://{domain}/@{username}/tagged/{tag_name}"),
        "name": format!("#{tag_name}"),
    })
}

/// The FEP-844e `generator` object for user actors: the software as an
/// `Application` carrying `implements`.
#[must_use]
pub fn capability_generator() -> Value {
    json!({
        "type": "Application",
        "implements": implemented_capabilities(),
    })
}

impl Actor {
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "one linear mapping from params to the wire document"
    )]
    pub fn local_person(params: &LocalActorParams<'_>) -> Self {
        let urls = LocalUserUrls::for_account(params.domain, params.username, params.actor_id);
        let feature_policy = if params.discoverable {
            if params.locked {
                crate::quote_policy::AUTOMATIC_FOLLOWERS
            } else {
                crate::quote_policy::AUTOMATIC_PUBLIC
            }
        } else {
            0
        };
        let interaction_policy = crate::quote_policy::capability_policy_json(
            "canFeature",
            feature_policy,
            &urls.id,
            &urls.followers,
            &urls.following,
        );
        Self {
            // The schema.org terms cover `PropertyValue` profile fields and
            // the toot terms the `featured` collection and `Emoji` tags,
            // mirroring Mastodon's own context extensions. The CID and
            // data-integrity contexts define the FEP-521a `Multikey` under
            // `assertionMethod` and the FEP-8b32 `DataIntegrityProof`.
            context: json!([AS_CONTEXT, SECURITY_CONTEXT, CID_CONTEXT, DATA_INTEGRITY_CONTEXT, {
                "schema": "http://schema.org#",
                "PropertyValue": "schema:PropertyValue",
                "VerifiableIdentityStatement": "https://w3id.org/fep/c390/VerifiableIdentityStatement",
                "value": "schema:value",
                "toot": "http://joinmastodon.org/ns#",
                "featured": { "@id": "toot:featured", "@type": "@id" },
                "featuredTags": { "@id": "toot:featuredTags", "@type": "@id" },
                "Hashtag": "as:Hashtag",
                "discoverable": "toot:discoverable",
                "indexable": "toot:indexable",
                "attributionDomains": { "@id": "toot:attributionDomains", "@container": "@set" },
                "memorial": "toot:memorial",
                "suspended": "toot:suspended",
                "showFeatured": "toot:showFeatured",
                "showMedia": "toot:showMedia",
                "showRepliesInMedia": "toot:showRepliesInMedia",
                "Emoji": "toot:Emoji",
                "movedTo": { "@id": "as:movedTo", "@type": "@id" },
                "alsoKnownAs": { "@id": "as:alsoKnownAs", "@type": "@id" },
                "gts": "https://gotosocial.org/ns#",
                "interactionPolicy": { "@id": "gts:interactionPolicy", "@type": "@id" },
                "canFeature": {
                    "@id": "https://w3id.org/fep/7aa9#canFeature",
                    "@type": "@id",
                },
                "automaticApproval": { "@id": "gts:automaticApproval", "@type": "@id" },
                "manualApproval": { "@id": "gts:manualApproval", "@type": "@id" },
                // FEP-844e capability discovery, under Mitra's term — the
                // convention deployed peers already parse.
                "mitra": "http://jsonld.mitra.social#",
                "implements": "mitra:implements",
                // Group actors: Lemmy's community extensions, declared with
                // the terms Lemmy's own context.json uses. Harmless on
                // Person/Service actors (the fields are omitted there).
                "lemmy": "https://join-lemmy.org/ns#",
                "postingRestrictedToMods": "lemmy:postingRestrictedToMods",
                "sensitive": "as:sensitive",
                // The one group fact no deployed vocabulary can express: a
                // community open to its members but not to strangers. Emitted
                // beside Lemmy's boolean, never instead of it.
                "plamenu": PLAMENU_NS,
                "postingPolicy": "plamenu:postingPolicy",
            }]),
            // Bots federate as `Service`, like Mastodon's `bot?` actors;
            // local groups as `Group` (FEP-1b12).
            kind: if params.group.is_some() {
                ActorType::Group
            } else if params.bot {
                ActorType::Service
            } else {
                ActorType::Person
            },
            preferred_username: params.username.to_owned(),
            name: params.display_name.map(str::to_owned),
            summary: params.summary.map(str::to_owned),
            // The actor's `url` is the human web page (`/@name`), not the AP id.
            url: urls.web_url.clone(),
            inbox: urls.inbox,
            outbox: urls.outbox,
            followers: urls.followers,
            following: urls.following,
            featured: urls.featured,
            featured_tags: urls.featured_tags,
            featured_collections: urls.featured_collections,
            published: params.published.clone(),
            manually_approves_followers: params.locked,
            discoverable: params.discoverable,
            indexable: params.indexable,
            attribution_domains: params.attribution_domains.clone(),
            memorial: params.memorial,
            suspended: params.suspended,
            show_featured: params.show_featured,
            show_media: params.show_media,
            show_replies_in_media: params.show_media_replies,
            interaction_policy,
            icon: params.icon.clone(),
            image: params.image.clone(),
            attachment: params.fields.iter().map(|field| json!(field)).collect(),
            tag: params.emoji_tags.clone(),
            assertion_method: verification_methods(
                &urls.id,
                urls.key_id.clone(),
                urls.ed25519_key_id,
                params.public_key_pem,
                params.ed25519_public_multibase,
            ),
            public_key: PublicKey {
                id: urls.key_id,
                owner: urls.id.clone(),
                public_key_pem: params.public_key_pem.to_owned(),
            },
            endpoints: Endpoints {
                shared_inbox: urls.shared_inbox,
            },
            also_known_as: params.also_known_as.clone(),
            moved_to: params.moved_to.clone(),
            generator: Some(capability_generator()),
            attributed_to: params.group.map(|_| urls.moderators),
            affiliations: params.group.map(|_| urls.affiliations),
            posting_restricted_to_mods: params.group.map(|g| g.posting_restricted_to_mods),
            sensitive: params.group.map(|g| g.sensitive),
            posting_policy: params.group.map(|g| g.posting_policy.to_owned()),
            id: urls.id,
        }
    }
}

/// The instance actor: the `Application` actor served at `/actor` that signs
/// this server's outbound fetches. Mastodon restricts its representative's
/// document to exactly these fields at its instance-actor endpoint, so we do
/// too — no followers/following/featured/published.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceActor {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ActorType,
    /// Mastodon uses the bare domain as the instance actor's username.
    pub preferred_username: String,
    pub url: String,
    pub inbox: String,
    pub outbox: String,
    /// Mastodon's instance actor is locked.
    pub manually_approves_followers: bool,
    pub public_key: PublicKey,
    /// FEP-521a verification methods (the Ed25519 Multikey).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertion_method: Vec<Multikey>,
    pub endpoints: Endpoints,
    /// FEP-844e: the server actor lists its capabilities at the top level
    /// (Mitra's convention for `Application` actors).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<Value>,
}

impl InstanceActor {
    #[must_use]
    pub fn new(domain: &str, public_key_pem: &str, ed25519_public_multibase: Option<&str>) -> Self {
        let urls = crate::urls::InstanceActorUrls::new(domain);
        Self {
            context: json!([AS_CONTEXT, SECURITY_CONTEXT, CID_CONTEXT, DATA_INTEGRITY_CONTEXT, {
                "mitra": "http://jsonld.mitra.social#",
                "implements": "mitra:implements",
            }]),
            kind: ActorType::Application,
            preferred_username: domain.to_owned(),
            // No HTML pages on a headless server; the document is the URL.
            url: urls.id.clone(),
            inbox: urls.inbox,
            outbox: urls.outbox,
            manually_approves_followers: true,
            assertion_method: verification_methods(
                &urls.id,
                urls.key_id.clone(),
                urls.ed25519_key_id,
                public_key_pem,
                ed25519_public_multibase,
            ),
            public_key: PublicKey {
                id: urls.key_id,
                owner: urls.id.clone(),
                public_key_pem: public_key_pem.to_owned(),
            },
            endpoints: Endpoints {
                shared_inbox: urls.shared_inbox,
            },
            implements: implemented_capabilities(),
            id: urls.id,
        }
    }
}

/// A remote actor as fetched from another server: deliberately lenient, since
/// non-Mastodon software omits many fields the strict [`Actor`] always has.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Mastodon's actor flags"
)]
pub struct RemoteActor {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub preferred_username: String,
    /// FEP-2c59's explicit `WebFinger` address. Mastodon accepts this when
    /// `preferredUsername` is missing or when the canonical acct lives on a
    /// different domain from the actor URI.
    #[serde(default)]
    pub webfinger: Option<String>,
    pub inbox: String,
    /// The actor's followers collection. Kept raw because Mastodon accepts a
    /// bare IRI, an object with `id`, or the first element of an array.
    #[serde(default)]
    pub followers: Option<Value>,
    /// The actor's following collection, with the same lenient shape as
    /// `followers`.
    #[serde(default)]
    pub following: Option<Value>,
    /// The actor's outbox collection, with the same lenient shape as
    /// `followers`. Its `totalItems` is the origin's authoritative status
    /// count (Mastodon reads it into `statuses_count`).
    #[serde(default)]
    pub outbox: Option<Value>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    /// Origin account-creation time. Older/minimal implementations may omit
    /// it, so ingest treats this as optional rather than inventing a value.
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default, deserialize_with = "remote_endpoints")]
    pub endpoints: Option<RemoteEndpoints>,
    /// The human web URL (`url`); kept raw since it may arrive as a bare
    /// string, a Link object (`{"href": …}`) or an array of either.
    #[serde(default)]
    pub url: Option<Value>,
    /// Avatar / header; kept raw since implementations vary (`url` may be a
    /// string, a Link object or an array).
    #[serde(default)]
    pub icon: Option<Value>,
    #[serde(default)]
    pub image: Option<Value>,
    /// `PropertyValue` profile fields (and whatever else the actor attaches).
    #[serde(default, deserialize_with = "one_or_many_values")]
    pub attachment: Vec<Value>,
    /// `Emoji` tag entries (and profile `Hashtag`s, which — like Mastodon —
    /// are not read from here: featured tags sync via `featuredTags`).
    #[serde(default, deserialize_with = "one_or_many_values")]
    pub tag: Vec<Value>,
    /// The pinned-statuses collection (`toot:featured`); kept raw since it
    /// may arrive as a bare IRI or an embedded collection object.
    #[serde(default)]
    pub featured: Option<Value>,
    /// The featured-hashtags collection (`toot:featuredTags`); a bare IRI or
    /// an embedded collection object. Dereferenced on actor refresh to sync
    /// the profile's featured tags, like Mastodon's featured-tags sync job.
    #[serde(default)]
    pub featured_tags: Option<Value>,
    /// The `featuredCollections` endpoint (FEP-7aa9); a bare IRI or an embedded
    /// collection object. Read on actor refresh to populate `in_collections`.
    #[serde(default)]
    pub featured_collections: Option<Value>,
    /// Locked account; absent on actors that never lock.
    #[serde(default)]
    pub manually_approves_followers: bool,
    /// Discoverability opt-in (`toot:discoverable`); absent on actors that
    /// never opt in, treated as false.
    #[serde(default)]
    pub discoverable: bool,
    /// Public-post search opt-in (`toot:indexable`); absent treated as false.
    #[serde(default)]
    pub indexable: bool,
    /// `GoToSocial`'s unauthenticated-web audience hints. A positive value on
    /// either field means cold history must not be shown to signed-out web
    /// visitors, even though signed federation fetches may cache it.
    #[serde(default)]
    pub hides_to_public_from_unauthed_web: Option<bool>,
    #[serde(default)]
    pub hides_cc_public_from_unauthed_web: Option<bool>,
    /// In-memoriam marker (`toot:memorial`); absent treated as false.
    #[serde(default)]
    pub memorial: bool,
    /// Domains the account authorizes for `fediverse:creator` attribution
    /// (`toot:attributionDomains`); lenient like Mastodon's ingest (a bare
    /// string is accepted alongside the usual array).
    #[serde(default, deserialize_with = "one_or_many_values")]
    pub attribution_domains: Vec<Value>,
    /// Temporary-suspension marker (`toot:suspended`). Mastodon emits it only
    /// while the actor is suspended; absent means not suspended.
    #[serde(default)]
    pub suspended: bool,
    /// Profile-tab settings (`toot:showMedia` / `toot:showRepliesInMedia` /
    /// `toot:showFeatured`). `None` when the actor omits a key — the ingest
    /// then keeps any previously stored value (Mastodon's `if @json.key?`).
    #[serde(default)]
    pub show_media: Option<bool>,
    #[serde(default)]
    pub show_replies_in_media: Option<bool>,
    #[serde(default)]
    pub show_featured: Option<bool>,
    /// FEP-7aa9 / Mastodon 4.6 account feature policy.
    #[serde(default)]
    pub interaction_policy: Option<Value>,
    /// Declared aliases (`alsoKnownAs`); kept raw since it may arrive as a
    /// bare IRI or an array of them.
    #[serde(default)]
    pub also_known_as: Option<Value>,
    /// The actor this one migrated to (`movedTo`); a bare IRI or an object.
    #[serde(default)]
    pub moved_to: Option<Value>,
    #[serde(default)]
    pub public_key: RemotePublicKeys,
    /// FEP-521a verification methods; kept raw since key types vary (RSA
    /// Multikeys, foreign suites). See [`Self::ed25519_public_key`].
    #[serde(default, deserialize_with = "one_or_many_values")]
    pub assertion_method: Vec<Value>,
    /// FEP-844e capability advertisement (`implements`), the canonical
    /// top-level placement. Kept raw; read by [`Self::advertises_rfc9421`].
    #[serde(default)]
    pub implements: Option<Value>,
    /// A `generator` Application object — where Mitra nests its FEP-844e
    /// `implements` on user actors. Kept raw; read by
    /// [`Self::advertises_rfc9421`].
    #[serde(default)]
    pub generator: Option<Value>,
    /// Group actors only: the community-wide NSFW flag, the way Lemmy
    /// publishes it on communities (`as:sensitive`). Absent on every
    /// non-community actor, so `None` means "not stated", not "false".
    #[serde(default)]
    pub sensitive: Option<bool>,
    /// Group actors only: Lemmy's `postingRestrictedToMods` — only the
    /// community's moderators may start threads.
    #[serde(default)]
    pub posting_restricted_to_mods: Option<bool>,
    /// Group actors only: FEP-1b12 `attributedTo`, naming who runs the
    /// community. Kept raw because the two deployed shapes differ — Lemmy
    /// (and we) publish a moderators *collection* IRI, `PeerTube` an inline
    /// list of the owning actors. Read by [`Self::group_moderators`].
    #[serde(default)]
    pub attributed_to: Option<Value>,
    /// Group actors only: the FEP-5219 `affiliations` collection. Mitra
    /// publishes this and no `attributedTo`, so it is the only way to learn
    /// who runs a Mitra community; read as a fallback by
    /// [`Self::group_moderators`].
    #[serde(default)]
    pub affiliations: Option<Value>,
    /// Group actors only: our own tri-state posting policy
    /// (`anyone`/`members`/`mods`). Absent from every peer but another
    /// Plamenu, where it recovers the "members only" state that Lemmy's
    /// boolean flattens away.
    #[serde(default)]
    pub posting_policy: Option<String>,
}

/// Where a Group actor publishes its moderators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupModerators<'a> {
    /// An `attributedTo` collection of bare actor IRIs — Lemmy's shape, and
    /// ours.
    Collection(&'a str),
    /// The moderators listed inline on the actor — `PeerTube`'s shape, where a
    /// channel names its owning `Person` directly.
    Inline(Vec<&'a str>),
    /// A FEP-5219 `affiliations` collection of `Relationship` items, each
    /// naming its `subject` — Mitra's shape (and ours, published alongside
    /// `attributedTo`).
    Affiliations(&'a str),
}

/// The FEP-5219 relationships that make someone a community moderator; the
/// ladder's lower rungs (plain membership, bans) confer nothing.
#[must_use]
pub fn is_moderator_affiliation(relationship: &str) -> bool {
    matches!(relationship, "admin" | "owner" | "moderator")
}

/// Extracts the URL of a lenient image value: a bare string, an object with
/// a string or Link-object `url`, or the first element of an array.
#[must_use]
pub fn image_url(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        Value::Array(items) => items.first().and_then(image_url),
        Value::Object(map) => map
            .get("url")
            .or_else(|| map.get("href"))
            .and_then(image_url),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEndpoints {
    #[serde(default)]
    pub shared_inbox: Option<String>,
}

fn one_or_many_values<'de, D>(deserializer: D) -> Result<Vec<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Array(items)) => Ok(items),
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(item) => Ok(vec![item]),
    }
}

fn remote_endpoints<'de, D>(deserializer: D) -> Result<Option<RemoteEndpoints>, D::Error>
where
    D: Deserializer<'de>,
{
    let endpoints = match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Object(mut map)) => Some(RemoteEndpoints {
            shared_inbox: map
                .remove("sharedInbox")
                .and_then(|value| value.as_str().map(str::to_owned)),
        }),
        _ => None,
    };
    Ok(endpoints)
}

impl RemoteActor {
    /// Maps an FEP-ef61 compatible actor ID back to its canonical `ap://`
    /// form. FEP-ae97 requires a client's main HTTP-signature key ID to stay
    /// canonical even when the actor and its controller are represented by a
    /// gateway-compatible HTTPS ID.
    fn canonical_portable_id(&self) -> Option<String> {
        const GATEWAY_PATH: &str = "/.well-known/apgateway/";
        let (_, did_url) = self.id.split_once(GATEWAY_PATH)?;
        did_url
            .starts_with("did:key:")
            .then(|| format!("ap://{did_url}"))
    }

    /// Inline keys live in the actor document, so their IDs must be scoped
    /// below that actor. Keys hosted elsewhere are represented as references
    /// and fetched with an explicit controller check. This distinction stops
    /// a hostile actor from reserving another actor's well-known key URI and
    /// turning global key-ID uniqueness into a first-claim denial of service.
    fn owns_inline_key_id(&self, key_id: &str) -> bool {
        let is_below = |controller: &str| {
            key_id == controller
                || key_id
                    .strip_prefix(controller)
                    .is_some_and(|suffix| suffix.starts_with('#') || suffix.starts_with('/'))
        };
        is_below(&self.id)
            || self
                .canonical_portable_id()
                .as_deref()
                .is_some_and(is_below)
    }

    /// The canonical `WebFinger` acct declared by FEP-2c59, if present and
    /// syntactically valid. Both `user@domain` and `acct:user@domain` forms
    /// are accepted.
    #[must_use]
    pub fn webfinger_acct(&self) -> Option<Acct> {
        self.webfinger.as_deref()?.parse().ok()
    }

    /// Whether an HTTP-signature `keyId` names this actor's own key: the
    /// declared `publicKey.id`, the actor id itself once the key fragment
    /// is dropped, or a FEP-521a `assertionMethod` entry. The bare-actor-id
    /// form covers signers (pub-relay) whose signatures say `keyId=<actor>`
    /// while the actor document declares `<actor>#main-key`; the keyId still
    /// dereferences to this document. The signature must separately verify
    /// against the named key.
    #[must_use]
    pub fn owns_key(&self, key_id: &str) -> bool {
        self.public_key.entries.iter().any(|entry| match entry {
            RemotePublicKeyEntry::Inline(key) => {
                !key.id.is_empty()
                    && key.owner == self.id
                    && self.owns_inline_key_id(&key.id)
                    && (key.id == key_id || key_id.split('#').next().unwrap_or(key_id) == self.id)
            }
            RemotePublicKeyEntry::Reference(id) => id == key_id,
        }) || (!self.public_key.fallback.id.is_empty()
            && self.public_key.fallback.owner == self.id
            && (self.public_key.fallback.id == key_id
                || key_id.split('#').next().unwrap_or(key_id) == self.id))
            || self.assertion_method.iter().any(|entry| match entry {
                Value::String(id) => id == key_id,
                Value::Object(map) => map.get("id").and_then(Value::as_str) == Some(key_id),
                _ => false,
            })
    }

    /// Extracts up to ten inline/reference FEP-521a methods and ten classic
    /// RSA methods, matching Mastodon's independent per-property bounds.
    /// Entries are deduplicated by exact key URI; a duplicate carrying
    /// different material is a hard error rather than an array-order key
    /// substitution.
    #[allow(
        clippy::too_many_lines,
        reason = "one bounded parser covers inline and referenced methods in both AP key dialects"
    )]
    pub fn verification_methods(
        &self,
    ) -> Result<Vec<RemoteVerificationMethod>, VerificationMethodError> {
        const PER_PROPERTY_LIMIT: usize = 10;
        if self.assertion_method.len() > PER_PROPERTY_LIMIT
            || self.public_key.entries.len() > PER_PROPERTY_LIMIT
        {
            return Err(VerificationMethodError::TooMany);
        }
        let mut methods: Vec<RemoteVerificationMethod> = Vec::with_capacity(PER_PROPERTY_LIMIT * 2);
        let push_unique = |methods: &mut Vec<RemoteVerificationMethod>,
                           method: RemoteVerificationMethod|
         -> Result<(), VerificationMethodError> {
            if let Some(existing) = methods.iter().find(|old| old.key_uri == method.key_uri) {
                // FEP-521a is processed first and therefore owns lifecycle
                // metadata when the classic compatibility mirror uses the
                // same URI and material.
                if existing.controller_uri != method.controller_uri
                    || (existing.algorithm.is_some()
                        && method.algorithm.is_some()
                        && existing.algorithm != method.algorithm)
                    || (existing.public_key.is_some()
                        && method.public_key.is_some()
                        && existing.public_key != method.public_key)
                {
                    return Err(VerificationMethodError::ConflictingDuplicate);
                }
            } else {
                methods.push(method);
            }
            Ok(())
        };
        for entry in &self.assertion_method {
            let method = match entry {
                Value::String(key_uri) => RemoteVerificationMethod {
                    key_uri: key_uri.clone(),
                    controller_uri: self.id.clone(),
                    algorithm: None,
                    public_key: None,
                    source: "multikey",
                    expires_at: None,
                    revoked: false,
                },
                Value::Object(map) => {
                    if map.get("type").and_then(Value::as_str) != Some("Multikey")
                        || map.get("controller").and_then(Value::as_str) != Some(self.id.as_str())
                    {
                        return Err(VerificationMethodError::WrongController);
                    }
                    let key_uri = map
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| {
                            id.starts_with("https://")
                                || id.starts_with("http://")
                                || id.starts_with("ap://")
                        })
                        .ok_or(VerificationMethodError::WrongController)?;
                    if !self.owns_inline_key_id(key_uri) {
                        return Err(VerificationMethodError::WrongController);
                    }
                    let multibase = map
                        .get("publicKeyMultibase")
                        .and_then(Value::as_str)
                        .ok_or(VerificationMethodError::WrongController)?;
                    let decoded = crate::multikey::decode_public(multibase)?;
                    let (algorithm, public_key) = match decoded {
                        crate::multikey::PublicMultikey::Rsa(pem) => ("rsa", pem),
                        crate::multikey::PublicMultikey::Ed25519(_) => {
                            ("ed25519", multibase.to_owned())
                        }
                        crate::multikey::PublicMultikey::MlDsa44(_) => {
                            ("ml-dsa-44", multibase.to_owned())
                        }
                    };
                    RemoteVerificationMethod {
                        key_uri: key_uri.to_owned(),
                        controller_uri: self.id.clone(),
                        algorithm: Some(algorithm.to_owned()),
                        public_key: Some(public_key),
                        source: "multikey",
                        expires_at: match map.get("expires") {
                            None => None,
                            Some(Value::String(raw)) => Some(
                                time::OffsetDateTime::parse(
                                    raw,
                                    &time::format_description::well_known::Rfc3339,
                                )
                                .map_err(|_| VerificationMethodError::InvalidLifecycle)?,
                            ),
                            Some(_) => return Err(VerificationMethodError::InvalidLifecycle),
                        },
                        revoked: map.get("revoked").is_some(),
                    }
                }
                _ => return Err(VerificationMethodError::WrongController),
            };
            push_unique(&mut methods, method)?;
        }
        for entry in &self.public_key.entries {
            let classic = match entry {
                RemotePublicKeyEntry::Inline(key)
                    if !key.id.is_empty()
                        && key.owner == self.id
                        && self.owns_inline_key_id(&key.id)
                        && !key.public_key_pem.is_empty() =>
                {
                    RemoteVerificationMethod {
                        key_uri: key.id.clone(),
                        controller_uri: self.id.clone(),
                        algorithm: Some("rsa".to_owned()),
                        public_key: Some(key.public_key_pem.clone()),
                        source: "classic",
                        expires_at: None,
                        revoked: false,
                    }
                }
                RemotePublicKeyEntry::Reference(key_uri)
                    if key_uri.starts_with("https://") || key_uri.starts_with("http://") =>
                {
                    RemoteVerificationMethod {
                        key_uri: key_uri.clone(),
                        controller_uri: self.id.clone(),
                        algorithm: None,
                        public_key: None,
                        source: "classic-external",
                        expires_at: None,
                        revoked: false,
                    }
                }
                _ => continue,
            };
            push_unique(&mut methods, classic)?;
        }
        if self.public_key.entries.is_empty() {
            let key = &self.public_key.fallback;
            if !key.id.is_empty()
                && key.owner == self.id
                && self.owns_inline_key_id(&key.id)
                && !key.public_key_pem.is_empty()
            {
                push_unique(
                    &mut methods,
                    RemoteVerificationMethod {
                        key_uri: key.id.clone(),
                        controller_uri: self.id.clone(),
                        algorithm: Some("rsa".to_owned()),
                        public_key: Some(key.public_key_pem.clone()),
                        source: "classic",
                        expires_at: None,
                        revoked: false,
                    },
                )?;
            }
        }
        Ok(methods)
    }

    /// The `publicKeyMultibase` of the FEP-521a `assertionMethod` entry with
    /// exactly this id, provided the entry is the actor's own (controller
    /// and hosting document both the actor).
    #[must_use]
    pub fn assertion_method_multikey(&self, key_id: &str) -> Option<&str> {
        if key_id.split('#').next().unwrap_or(key_id) != self.id {
            return None;
        }
        self.assertion_method.iter().find_map(|entry| {
            let map = entry.as_object()?;
            (map.get("type").and_then(Value::as_str) == Some("Multikey")
                && map.get("controller").and_then(Value::as_str) == Some(self.id.as_str())
                && map.get("id").and_then(Value::as_str) == Some(key_id))
            .then(|| map.get("publicKeyMultibase").and_then(Value::as_str))
            .flatten()
        })
    }

    /// Whether this actor advertises RFC 9421 support via FEP-844e
    /// `implements` — the declarative capability signal used to decide
    /// outbound RFC 9421 emission. True when the rfc9421 href appears in a
    /// top-level `implements` or a nested `generator.implements` (Mitra's
    /// placement on user actors). A capability advertisement is trustworthy in
    /// a way a bare delivery `200` is not: upstream Pleroma answers 200 to a
    /// 9421 inbox POST then async-drops it, but never advertises the capability.
    #[must_use]
    pub fn advertises_rfc9421(&self) -> bool {
        const RFC9421_HREF: &str = "https://datatracker.ietf.org/doc/html/rfc9421";
        let lists_rfc9421 = |implements: Option<&Value>| {
            crate::activity::one_or_many(implements)
                .iter()
                .any(|entry| {
                    entry
                        .get("href")
                        .and_then(Value::as_str)
                        .is_some_and(|href| href.starts_with(RFC9421_HREF))
                })
        };
        lists_rfc9421(self.implements.as_ref())
            || lists_rfc9421(self.generator.as_ref().and_then(|g| g.get("implements")))
    }

    /// The actor's Ed25519 public Multikey (FEP-521a): the first
    /// `assertionMethod` entry of type `Multikey` whose `controller` is the
    /// actor itself, whose id lives on the actor (same document, `#fragment`),
    /// and whose key material decodes as Ed25519 — RSA Multikeys and foreign
    /// suites are skipped, not errors.
    #[must_use]
    pub fn ed25519_public_key(&self) -> Option<&str> {
        self.assertion_method.iter().find_map(|entry| {
            let map = entry.as_object()?;
            if map.get("type").and_then(Value::as_str) != Some("Multikey")
                || map.get("controller").and_then(Value::as_str) != Some(self.id.as_str())
            {
                return None;
            }
            let key_id = map.get("id").and_then(Value::as_str)?;
            if key_id.split('#').next().unwrap_or(key_id) != self.id {
                return None;
            }
            let key = map.get("publicKeyMultibase").and_then(Value::as_str)?;
            crate::multikey::decode_ed25519_public(key).ok()?;
            Some(key)
        })
    }

    /// Extracts a collection IRI using Mastodon's lenient
    /// `valid_collection_uri` shape: array first element, object `id`, or
    /// string. URL validation lives in the server layer where the URL parser is
    /// already a dependency.
    fn collection_iri(value: Option<&Value>) -> Option<&str> {
        value.and_then(Self::collection_iri_value)
    }

    fn collection_iri_value(value: &Value) -> Option<&str> {
        match value {
            Value::String(uri) => Some(uri),
            Value::Array(items) => items.first().and_then(Self::collection_iri_value),
            Value::Object(map) => map.get("id").and_then(Value::as_str),
            _ => None,
        }
    }

    #[must_use]
    pub fn followers_url(&self) -> Option<&str> {
        Self::collection_iri(self.followers.as_ref())
    }

    #[must_use]
    pub fn following_url(&self) -> Option<&str> {
        Self::collection_iri(self.following.as_ref())
    }

    #[must_use]
    pub fn outbox_url(&self) -> Option<&str> {
        Self::collection_iri(self.outbox.as_ref())
    }

    /// `Some(true)` when either `GoToSocial` hint restricts anonymous web;
    /// `Some(false)` when at least one hint is explicitly false and neither is
    /// true; `None` when the actor publishes neither extension.
    #[must_use]
    pub fn restricts_unauthenticated_web(&self) -> Option<bool> {
        match (
            self.hides_to_public_from_unauthed_web,
            self.hides_cc_public_from_unauthed_web,
        ) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), _) | (_, Some(false)) => Some(false),
            (None, None) => None,
        }
    }

    /// The shared inbox when published, else the personal inbox.
    #[must_use]
    pub fn shared_inbox_or_empty(&self) -> &str {
        self.endpoints
            .as_ref()
            .and_then(|e| e.shared_inbox.as_deref())
            .unwrap_or("")
    }

    /// The actor's human web URL (`url`), resolved from the lenient field: a
    /// bare string, a Link object, or an array (preferring an explicit
    /// `text/html` link, then the first usable value). `None` when absent.
    #[must_use]
    pub fn web_url(&self) -> Option<&str> {
        let value = self.url.as_ref()?;
        if let Value::Array(items) = value
            && let Some(html) = items.iter().find_map(|item| {
                let obj = item.as_object()?;
                (obj.get("mediaType").and_then(Value::as_str) == Some("text/html"))
                    .then(|| obj.get("href").and_then(Value::as_str))
                    .flatten()
            })
        {
            return Some(html);
        }
        image_url(value)
    }

    /// The actor's declared aliases as a list of IRIs, accepting both the
    /// single-value and array forms (Mastodon's `as_array`).
    #[must_use]
    pub fn also_known_as_uris(&self) -> Vec<String> {
        match &self.also_known_as {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| crate::activity::id_of(item).map(str::to_owned))
                .collect(),
            Some(other) => crate::activity::id_of(other)
                .map(|s| vec![s.to_owned()])
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// The IRI this actor has migrated to (`movedTo`), if any.
    #[must_use]
    pub fn moved_to_uri(&self) -> Option<&str> {
        self.moved_to.as_ref().and_then(crate::activity::id_of)
    }

    /// Whether this actor is an automated account, mirroring Mastodon's
    /// `bot?`: the `Application` and `Service` actor types map to bots, every
    /// other type (`Person`, `Group`, `Organization`, …) does not.
    #[must_use]
    pub fn is_bot(&self) -> bool {
        matches!(self.kind.as_str(), "Application" | "Service")
    }

    /// The actor's declared `type`, restricted to the kinds we recognise
    /// (`Person`/`Service`/`Application`/`Group`/`Organization`) — the same
    /// whitelist the inbox and search apply. An unknown type stores as `None`,
    /// read as `Person`. Backs `accounts.actor_type`.
    #[must_use]
    pub fn actor_type(&self) -> Option<&str> {
        matches!(
            self.kind.as_str(),
            "Person" | "Service" | "Application" | "Group" | "Organization"
        )
        .then_some(self.kind.as_str())
    }

    /// A Group actor's moderators as published, `None` when the actor names
    /// none. Only meaningful on a `Group`; `attributedTo` means something else
    /// entirely on other actor types, so callers must check the type first.
    ///
    /// Both deployed `attributedTo` shapes are accepted, the way Lemmy's own
    /// parser accepts them: a collection IRI (Lemmy, and us) or an inline list
    /// of the owning actors (`PeerTube`). A one-element array is unwrapped
    /// first, since `go-fed` peers serialize single values that way; in a
    /// longer list an entry typed `Group` is the community itself, not a
    /// moderator, and is dropped (Lemmy keeps only the `Person` entries).
    ///
    /// The FEP-5219 `affiliations` collection is the fallback: Mitra publishes
    /// its group admins there and no `attributedTo` at all, so without this a
    /// Mitra community would look unmoderated.
    #[must_use]
    pub fn group_moderators(&self) -> Option<GroupModerators<'_>> {
        self.attributed_to
            .as_ref()
            .and_then(Self::moderators_of)
            .or_else(|| {
                Self::collection_iri(self.affiliations.as_ref()).map(GroupModerators::Affiliations)
            })
    }

    fn moderators_of(value: &Value) -> Option<GroupModerators<'_>> {
        /// The actor types an inline `attributedTo` entry can name; anything
        /// else (a collection, an untyped reference) is a collection IRI.
        const ACTOR_KINDS: [&str; 5] =
            ["Person", "Service", "Application", "Group", "Organization"];
        match value {
            Value::String(uri) => Some(GroupModerators::Collection(uri)),
            Value::Object(map) => {
                let id = map.get("id").and_then(Value::as_str)?;
                let kind = map.get("type").and_then(Value::as_str).unwrap_or_default();
                if ACTOR_KINDS.contains(&kind) {
                    Some(GroupModerators::Inline(vec![id]))
                } else {
                    Some(GroupModerators::Collection(id))
                }
            }
            Value::Array(items) => match items.as_slice() {
                [] => None,
                [single] => Self::moderators_of(single),
                many => {
                    let ids: Vec<&str> = many
                        .iter()
                        .filter(|item| item.get("type").and_then(Value::as_str) != Some("Group"))
                        .filter_map(crate::activity::id_of)
                        .collect();
                    (!ids.is_empty()).then_some(GroupModerators::Inline(ids))
                }
            },
            _ => None,
        }
    }

    /// The IRI of the actor's `featuredCollections` endpoint.
    #[must_use]
    pub fn featured_collections_uri(&self) -> Option<&str> {
        self.featured_collections
            .as_ref()
            .and_then(crate::activity::id_of)
    }

    /// The IRI of the actor's `featuredTags` collection.
    #[must_use]
    pub fn featured_tags_uri(&self) -> Option<&str> {
        self.featured_tags.as_ref().and_then(crate::activity::id_of)
    }

    /// The declared attribution domains as strings, capped at Mastodon's
    /// inbound hard limit (`ATTRIBUTION_DOMAINS_HARD_LIMIT = 256`).
    #[must_use]
    pub fn attribution_domain_strings(&self) -> Vec<String> {
        self.attribution_domains
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .take(256)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Actor {
        Actor::local_person(&sample_params())
    }

    fn sample_params() -> LocalActorParams<'static> {
        LocalActorParams {
            domain: "plamenu.local",
            username: "alice",
            actor_id: None,
            display_name: Some("Alice"),
            summary: None,
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            published: "2026-06-09T00:00:00Z".to_owned(),
            icon: Some(
                Image::new("https://plamenu.local/media/1.png".to_owned(), "image/png")
                    .with_summary("a smiling avatar"),
            ),
            image: None,
            fields: vec![PropertyValue::new(
                "Website".to_owned(),
                "<a href=\"https://example.com\">example.com</a>".to_owned(),
            )],
            emoji_tags: vec![
                crate::emoji::EmojiTag::new(
                    crate::emoji::emoji_url("plamenu.local", 7),
                    "blobcat",
                    "2026-06-12T00:00:00Z".to_owned(),
                    Image::new("https://plamenu.local/media/7.png".to_owned(), "image/png"),
                )
                .into_value(),
            ],
            locked: false,
            discoverable: true,
            bot: false,
            indexable: true,
            memorial: true,
            suspended: false,
            attribution_domains: Vec::new(),
            show_featured: true,
            show_media: false,
            show_media_replies: false,
            also_known_as: Vec::new(),
            moved_to: None,
            ed25519_public_multibase: Some("z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2"),
            group: None,
        }
    }

    fn relay_actor(owner: &str) -> RemoteActor {
        serde_json::from_value(serde_json::json!({
            "id": "https://relay.example/actor",
            "type": "Service",
            "inbox": "https://relay.example/inbox",
            "publicKey": {
                "id": "https://relay.example/actor#main-key",
                "owner": owner,
                "publicKeyPem": "irrelevant",
            },
        }))
        .unwrap()
    }

    #[test]
    fn advertises_rfc9421_on_real_mitra_actor() {
        let raw = r#"{"id": "https://mitra.local/users/erin", "type": "Person", "preferredUsername": "erin", "inbox": "https://mitra.local/users/erin/inbox", "publicKey": {"id": "https://mitra.local/users/erin#main-key", "owner": "https://mitra.local/users/erin", "publicKeyPem": "irrelevant"}, "generator": {"type": "Application", "implements": [{"name": "RFC-9421: HTTP Message Signatures", "href": "https://datatracker.ietf.org/doc/html/rfc9421"}, {"name": "RFC-9421 signatures using the Ed25519 algorithm", "href": "https://datatracker.ietf.org/doc/html/rfc9421#name-eddsa-using-curve-edwards25"}]}}"#;
        let actor: RemoteActor = serde_json::from_str(raw).unwrap();
        assert!(
            actor.generator.is_some(),
            "generator field dropped in deserialize"
        );
        assert!(
            actor.advertises_rfc9421(),
            "real Mitra actor must advertise rfc9421"
        );
    }

    fn actor_with(extra: &serde_json::Value) -> RemoteActor {
        let mut doc = serde_json::json!({
            "id": "https://peer.example/users/x",
            "type": "Person",
            "inbox": "https://peer.example/users/x/inbox",
            "publicKey": {
                "id": "https://peer.example/users/x#main-key",
                "owner": "https://peer.example/users/x",
                "publicKeyPem": "irrelevant",
            },
        });
        doc.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(doc).unwrap()
    }

    #[test]
    fn reads_gotosocial_signed_out_web_visibility_hints() {
        let absent = actor_with(&serde_json::json!({}));
        assert_eq!(absent.restricts_unauthenticated_web(), None);

        let allowed = actor_with(&serde_json::json!({
            "hidesToPublicFromUnauthedWeb": false,
        }));
        assert_eq!(allowed.restricts_unauthenticated_web(), Some(false));

        let restricted = actor_with(&serde_json::json!({
            "hidesToPublicFromUnauthedWeb": false,
            "hidesCcPublicFromUnauthedWeb": true,
        }));
        assert_eq!(restricted.restricts_unauthenticated_web(), Some(true));
    }

    #[test]
    fn advertises_rfc9421_reads_fep_844e_implements() {
        // No advertisement → not detected.
        assert!(!actor_with(&serde_json::json!({})).advertises_rfc9421());

        // Mitra's placement: nested under a `generator` Application object,
        // including the ed25519 sub-href.
        assert!(actor_with(&serde_json::json!({
            "generator": {
                "type": "Application",
                "implements": [
                    {"name": "RFC-9421", "href": "https://datatracker.ietf.org/doc/html/rfc9421"},
                    {"href": "https://datatracker.ietf.org/doc/html/rfc9421#name-eddsa-using-curve-edwards25"},
                ],
            }
        }))
        .advertises_rfc9421());

        // Canonical top-level placement, as a single object (not an array).
        assert!(
            actor_with(&serde_json::json!({
                "implements": {"href": "https://datatracker.ietf.org/doc/html/rfc9421"},
            }))
            .advertises_rfc9421()
        );

        // An unrelated capability must not trip it.
        assert!(
            !actor_with(&serde_json::json!({
                "implements": [{"href": "https://w3id.org/fep/8b32"}],
            }))
            .advertises_rfc9421()
        );
    }

    #[test]
    fn owns_key_accepts_declared_and_bare_actor_keyids() {
        let actor = relay_actor("https://relay.example/actor");
        assert!(actor.owns_key("https://relay.example/actor#main-key"));
        // pub-relay signs with the bare actor id, no key fragment.
        assert!(actor.owns_key("https://relay.example/actor"));
        assert!(!actor.owns_key("https://other.example/actor"));
        assert!(!actor.owns_key("https://other.example/actor#main-key"));
    }

    #[test]
    fn owns_key_rejects_a_key_owned_by_another_actor() {
        let actor = relay_actor("https://other.example/actor");
        assert!(!actor.owns_key("https://relay.example/actor#main-key"));
        assert!(!actor.owns_key("https://relay.example/actor"));
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one assertion per wire field")]
    fn serializes_with_ap_wire_names() {
        let value = serde_json::to_value(sample()).unwrap();
        assert_eq!(value["@context"][0], json!(AS_CONTEXT));
        assert_eq!(value["@context"][1], json!(SECURITY_CONTEXT));
        assert_eq!(
            value["@context"][4]["PropertyValue"],
            "schema:PropertyValue"
        );
        assert_eq!(value["type"], "Person");
        assert_eq!(value["icon"]["type"], "Image");
        assert_eq!(value["icon"]["mediaType"], "image/png");
        assert_eq!(value["icon"]["url"], "https://plamenu.local/media/1.png");
        // Avatar alt text rides the icon as `summary`, like Mastodon.
        assert_eq!(value["icon"]["summary"], "a smiling avatar");
        assert_eq!(value["attachment"][0]["type"], "PropertyValue");
        assert_eq!(value["attachment"][0]["name"], "Website");
        assert_eq!(value["@context"][4]["Emoji"], "toot:Emoji");
        assert_eq!(value["tag"][0]["type"], "Emoji");
        assert_eq!(value["tag"][0]["name"], ":blobcat:");
        // `image` (the header) is unset and must be omitted, not null.
        assert!(!value.as_object().unwrap().contains_key("image"));
        assert_eq!(value["id"], "https://plamenu.local/users/alice");
        // `url` is the human web page, not the AP id.
        assert_eq!(value["url"], "https://plamenu.local/@alice");
        assert_eq!(
            value["featured"],
            "https://plamenu.local/users/alice/collections/featured"
        );
        assert_eq!(
            value["featuredCollections"],
            "https://plamenu.local/users/alice/featured_collections"
        );
        assert_eq!(value["@context"][4]["featured"]["@id"], "toot:featured");
        assert_eq!(
            value["featuredTags"],
            "https://plamenu.local/users/alice/collections/tags"
        );
        assert_eq!(
            value["@context"][4]["featuredTags"]["@id"],
            "toot:featuredTags"
        );
        assert_eq!(value["preferredUsername"], "alice");
        assert_eq!(value["name"], "Alice");
        assert_eq!(value["manuallyApprovesFollowers"], false);
        assert_eq!(value["discoverable"], true);
        assert_eq!(value["@context"][4]["discoverable"], "toot:discoverable");
        assert_eq!(value["indexable"], true);
        assert_eq!(value["@context"][4]["indexable"], "toot:indexable");
        // The profile-tab settings and memorial marker federate under the
        // `toot:` context terms, matching Mastodon's `profile_settings`.
        assert_eq!(value["memorial"], true);
        assert_eq!(value["@context"][4]["memorial"], "toot:memorial");
        // `suspended` is only emitted while true, like Mastodon.
        assert!(!value.as_object().unwrap().contains_key("suspended"));
        assert_eq!(value["@context"][4]["suspended"], "toot:suspended");
        assert_eq!(value["showFeatured"], true);
        assert_eq!(value["showMedia"], false);
        assert_eq!(value["showRepliesInMedia"], false);
        assert_eq!(value["@context"][4]["showFeatured"], "toot:showFeatured");
        assert_eq!(value["@context"][4]["showMedia"], "toot:showMedia");
        assert_eq!(
            value["@context"][4]["showRepliesInMedia"],
            "toot:showRepliesInMedia"
        );
        assert_eq!(
            value["@context"][4]["canFeature"]["@id"],
            "https://w3id.org/fep/7aa9#canFeature"
        );
        assert_eq!(
            value["interactionPolicy"]["canFeature"]["automaticApproval"],
            json!(["https://www.w3.org/ns/activitystreams#Public"])
        );
        assert_eq!(
            value["publicKey"]["id"],
            "https://plamenu.local/users/alice#main-key"
        );
        assert_eq!(
            value["publicKey"]["owner"],
            "https://plamenu.local/users/alice"
        );
        assert!(
            value["publicKey"]["publicKeyPem"]
                .as_str()
                .unwrap()
                .contains("PUBLIC KEY")
        );
        assert_eq!(
            value["endpoints"]["sharedInbox"],
            "https://plamenu.local/inbox"
        );
        // `summary` is None and must be omitted entirely, not serialized as null.
        assert!(!value.as_object().unwrap().contains_key("summary"));
        // No migration: `movedTo`/`alsoKnownAs` are omitted, not null/[].
        assert!(!value.as_object().unwrap().contains_key("movedTo"));
        assert!(!value.as_object().unwrap().contains_key("alsoKnownAs"));
        // FEP-844e: user actors advertise capabilities under `generator`
        // (Mitra's convention), with the term mapped in the context.
        assert_eq!(value["@context"][4]["implements"], "mitra:implements");
        assert_eq!(value["@context"][4]["mitra"], "http://jsonld.mitra.social#");
        assert_eq!(value["generator"]["type"], "Application");
        let capability_hrefs: Vec<&str> = value["generator"]["implements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["href"].as_str().unwrap())
            .collect();
        assert_eq!(
            capability_hrefs,
            [
                "https://w3id.org/fep/8fcf",
                "https://w3id.org/fep/7aa9",
                "https://w3id.org/fep/521a",
                "https://w3id.org/fep/8b32",
                "https://w3id.org/fep/044f",
                "https://w3id.org/fep/e232",
                "https://datatracker.ietf.org/doc/html/rfc9421",
                "https://datatracker.ietf.org/doc/html/rfc9421#name-eddsa-using-curve-edwards25",
            ]
        );
        // FEP-521a: the Ed25519 key as a Multikey under `assertionMethod`,
        // with the CID and data-integrity contexts that define the terms.
        assert_eq!(value["@context"][2], json!(CID_CONTEXT));
        assert_eq!(value["@context"][3], json!(DATA_INTEGRITY_CONTEXT));
        // `sample()`'s `publicKeyPem` is a placeholder that does not parse, so
        // only the Ed25519 method survives here — the graceful degradation
        // `Multikey::rsa` is built for. The RSA mirror an actor with a real
        // key publishes is covered by
        // `rsa_signing_key_is_mirrored_into_assertion_method`.
        assert_eq!(
            value["assertionMethod"],
            json!([{
                "id": "https://plamenu.local/users/alice#ed25519-key",
                "type": "Multikey",
                "controller": "https://plamenu.local/users/alice",
                "publicKeyMultibase": "z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2",
            }])
        );
    }

    /// Mastodon 4.7 resolves a signature's `keyId` against verification
    /// methods it built from `assertionMethod`; builds in the
    /// 2026-06-19…07-06 window (mastodon#39725) let those *replace*
    /// `publicKey`, so an actor that publishes only an Ed25519 method loses
    /// its RSA key there and its signatures come back
    /// `401 Public key not found for key …#main-key`. Both actor documents
    /// must therefore mirror the RSA key under the very id they sign with,
    /// after the Ed25519 method (which stays the FEP-8b32 proof key).
    #[test]
    fn rsa_signing_key_is_mirrored_into_assertion_method() {
        let pair = crate::keys::generate_keypair().unwrap();
        let mirror = crate::multikey::encode_rsa_public(&pair.public_pem).unwrap();

        let mut params = LocalActorParams {
            public_key_pem: &pair.public_pem,
            ..sample_params()
        };
        params.ed25519_public_multibase = Some("z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2");
        let user = serde_json::to_value(Actor::local_person(&params)).unwrap();
        assert_eq!(
            user["assertionMethod"][0]["id"],
            "https://plamenu.local/users/alice#ed25519-key"
        );
        assert_eq!(user["assertionMethod"][1]["type"], "Multikey");
        assert_eq!(user["assertionMethod"][1]["publicKeyMultibase"], mirror);
        assert_eq!(
            user["assertionMethod"][1]["id"], user["publicKey"]["id"],
            "the mirror must carry the very id our signatures name"
        );
        assert_eq!(
            user["assertionMethod"][1]["controller"], user["id"],
            "a verification method is only trusted when the actor controls it"
        );

        // The instance actor signs every outbound fetch, so it needs the same
        // mirror.
        let instance = serde_json::to_value(InstanceActor::new(
            "plamenu.local",
            &pair.public_pem,
            Some("z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2"),
        ))
        .unwrap();
        assert_eq!(
            instance["assertionMethod"][1]["id"],
            "https://plamenu.local/actor#main-key"
        );
        assert_eq!(instance["assertionMethod"][1]["publicKeyMultibase"], mirror);

        // An actor still awaiting the Ed25519 backfill publishes the RSA
        // mirror alone rather than nothing.
        params.ed25519_public_multibase = None;
        let bare = serde_json::to_value(Actor::local_person(&params)).unwrap();
        assert_eq!(bare["assertionMethod"].as_array().unwrap().len(), 1);
        assert_eq!(
            bare["assertionMethod"][0]["id"],
            "https://plamenu.local/users/alice#main-key"
        );
    }

    /// An account still awaiting the Ed25519 backfill publishes no
    /// `assertionMethod` at all — never an empty array or a null.
    #[test]
    fn attribution_domains_emit_only_when_present() {
        let bare = serde_json::to_value(sample()).unwrap();
        assert!(
            !bare.as_object().unwrap().contains_key("attributionDomains"),
            "empty list is omitted, like Mastodon"
        );
        assert_eq!(
            bare["@context"][4]["attributionDomains"]["@id"],
            "toot:attributionDomains"
        );
        let mut actor = sample();
        actor.attribution_domains = vec!["example.com".to_owned()];
        let value = serde_json::to_value(&actor).unwrap();
        assert_eq!(value["attributionDomains"], json!(["example.com"]));
        // Round-trip through the inbound struct, plus the lenient bare-string
        // form some implementations may emit.
        let remote: RemoteActor = serde_json::from_value(value).unwrap();
        assert_eq!(remote.attribution_domain_strings(), ["example.com"]);
        let mut single = serde_json::to_value(sample()).unwrap();
        single["attributionDomains"] = json!("example.org");
        let remote: RemoteActor = serde_json::from_value(single).unwrap();
        assert_eq!(remote.attribution_domain_strings(), ["example.org"]);
    }

    #[test]
    fn suspended_actor_emits_the_marker() {
        let mut actor = sample();
        actor.suspended = true;
        let value = serde_json::to_value(&actor).unwrap();
        assert_eq!(value["suspended"], true);
        // And a round-trip through the inbound struct reads it back.
        let remote: RemoteActor = serde_json::from_value(value).unwrap();
        assert!(remote.suspended);
        let remote: RemoteActor =
            serde_json::from_value(serde_json::to_value(sample()).unwrap()).unwrap();
        assert!(!remote.suspended);
    }

    #[test]
    fn assertion_method_is_omitted_without_a_key() {
        let mut value = serde_json::to_value(sample()).unwrap();
        assert!(value.as_object().unwrap().contains_key("assertionMethod"));
        let actor: Actor = serde_json::from_value(value.take()).unwrap();
        let mut without = actor.clone();
        without.assertion_method = Vec::new();
        let reserialized = serde_json::to_value(without).unwrap();
        assert!(
            !reserialized
                .as_object()
                .unwrap()
                .contains_key("assertionMethod")
        );
    }

    #[test]
    fn bot_actor_serializes_as_service() {
        let actor = Actor::local_person(&LocalActorParams {
            domain: "plamenu.local",
            username: "botto",
            actor_id: None,
            display_name: None,
            summary: None,
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            published: "2026-06-09T00:00:00Z".to_owned(),
            icon: None,
            image: None,
            fields: Vec::new(),
            emoji_tags: Vec::new(),
            locked: false,
            discoverable: false,
            bot: true,
            indexable: false,
            memorial: false,
            suspended: false,
            attribution_domains: Vec::new(),
            show_featured: true,
            show_media: true,
            show_media_replies: true,
            also_known_as: Vec::new(),
            moved_to: None,
            ed25519_public_multibase: None,
            group: None,
        });
        let value = serde_json::to_value(actor).unwrap();
        assert_eq!(value["type"], "Service");
        assert_eq!(value["indexable"], false);
    }

    #[test]
    fn group_actor_serializes_with_community_fields() {
        let actor = Actor::local_person(&LocalActorParams {
            domain: "plamenu.local",
            username: "hiking",
            actor_id: None,
            display_name: Some("Hiking"),
            summary: None,
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            published: "2026-06-09T00:00:00Z".to_owned(),
            icon: None,
            image: None,
            fields: Vec::new(),
            emoji_tags: Vec::new(),
            locked: true,
            discoverable: true,
            bot: false,
            indexable: true,
            memorial: false,
            suspended: false,
            attribution_domains: Vec::new(),
            show_featured: true,
            show_media: true,
            show_media_replies: false,
            also_known_as: Vec::new(),
            moved_to: None,
            ed25519_public_multibase: None,
            group: Some(GroupActorParams {
                posting_restricted_to_mods: true,
                sensitive: false,
                posting_policy: "mods",
            }),
        });
        let value = serde_json::to_value(actor).unwrap();
        assert_eq!(value["type"], "Group");
        // Approval-mode membership rides the standard locked flag.
        assert_eq!(value["manuallyApprovesFollowers"], true);
        // FEP-1b12 moderators (Lemmy's mod list) + FEP-5219 affiliations.
        assert_eq!(
            value["attributedTo"],
            "https://plamenu.local/users/hiking/moderators"
        );
        assert_eq!(
            value["affiliations"],
            "https://plamenu.local/users/hiking/affiliations"
        );
        // Lemmy's community extensions, with their context terms declared.
        assert_eq!(value["postingRestrictedToMods"], true);
        assert_eq!(value["sensitive"], false);
        assert_eq!(
            value["@context"][4]["postingRestrictedToMods"],
            "lemmy:postingRestrictedToMods"
        );
        assert_eq!(value["@context"][4]["sensitive"], "as:sensitive");
        // Our own term carries the state Lemmy's boolean cannot, so a peer
        // running this software reads the policy exactly as configured.
        assert_eq!(value["postingPolicy"], "mods");
        assert_eq!(
            value["@context"][4]["postingPolicy"],
            "plamenu:postingPolicy"
        );
        assert_eq!(value["@context"][4]["plamenu"], PLAMENU_NS);
    }

    /// A members-only group is the case Lemmy's vocabulary flattens: the
    /// boolean must stay false (a Lemmy reader would otherwise refuse posts
    /// from every non-moderator) while our term keeps the real policy.
    #[test]
    fn members_only_group_keeps_the_lemmy_boolean_false() {
        let actor = Actor::local_person(&LocalActorParams {
            domain: "plamenu.local",
            username: "hiking",
            actor_id: None,
            display_name: Some("Hiking"),
            summary: None,
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            published: "2026-06-09T00:00:00Z".to_owned(),
            icon: None,
            image: None,
            fields: Vec::new(),
            emoji_tags: Vec::new(),
            locked: false,
            discoverable: true,
            bot: false,
            indexable: true,
            memorial: false,
            suspended: false,
            attribution_domains: Vec::new(),
            show_featured: true,
            show_media: true,
            show_media_replies: false,
            also_known_as: Vec::new(),
            moved_to: None,
            ed25519_public_multibase: None,
            group: Some(GroupActorParams {
                posting_restricted_to_mods: false,
                sensitive: false,
                posting_policy: "members",
            }),
        });
        let value = serde_json::to_value(actor).unwrap();
        assert_eq!(value["postingRestrictedToMods"], false);
        assert_eq!(value["postingPolicy"], "members");

        // And it round-trips: another Plamenu reads back what we published.
        let parsed: RemoteActor = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.posting_policy.as_deref(), Some("members"));
        assert_eq!(parsed.posting_restricted_to_mods, Some(false));
    }

    #[test]
    fn person_actor_omits_group_fields() {
        let value = serde_json::to_value(sample()).unwrap();
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("attributedTo"));
        assert!(!object.contains_key("affiliations"));
        assert!(!object.contains_key("postingRestrictedToMods"));
        assert!(!object.contains_key("sensitive"));
        assert!(!object.contains_key("postingPolicy"));
    }

    #[test]
    fn serializes_migration_fields_when_set() {
        let actor = Actor::local_person(&LocalActorParams {
            domain: "plamenu.local",
            username: "alice",
            actor_id: None,
            display_name: Some("Alice"),
            summary: None,
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            published: "2026-06-09T00:00:00Z".to_owned(),
            icon: None,
            image: None,
            fields: Vec::new(),
            emoji_tags: Vec::new(),
            locked: false,
            discoverable: false,
            bot: false,
            indexable: false,
            memorial: false,
            suspended: false,
            attribution_domains: Vec::new(),
            show_featured: true,
            show_media: true,
            show_media_replies: true,
            also_known_as: vec!["https://old.example/users/alice".to_owned()],
            moved_to: Some("https://new.example/users/alice".to_owned()),
            ed25519_public_multibase: None,
            group: None,
        });
        let value = serde_json::to_value(actor).unwrap();
        assert_eq!(value["@context"][4]["movedTo"]["@id"], "as:movedTo");
        assert_eq!(value["@context"][4]["alsoKnownAs"]["@id"], "as:alsoKnownAs");
        assert_eq!(value["movedTo"], "https://new.example/users/alice");
        assert_eq!(
            value["alsoKnownAs"],
            json!(["https://old.example/users/alice"])
        );
        assert_eq!(
            value["interactionPolicy"]["canFeature"]["automaticApproval"],
            json!(["https://plamenu.local/users/alice"])
        );
    }

    #[test]
    fn locked_discoverable_actor_can_be_featured_by_followers() {
        let actor = Actor::local_person(&LocalActorParams {
            domain: "plamenu.local",
            username: "alice",
            actor_id: None,
            display_name: Some("Alice"),
            summary: None,
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            published: "2026-06-09T00:00:00Z".to_owned(),
            icon: None,
            image: None,
            fields: Vec::new(),
            emoji_tags: Vec::new(),
            locked: true,
            discoverable: true,
            bot: false,
            indexable: false,
            memorial: false,
            suspended: false,
            attribution_domains: Vec::new(),
            show_featured: true,
            show_media: true,
            show_media_replies: true,
            also_known_as: Vec::new(),
            moved_to: None,
            ed25519_public_multibase: None,
            group: None,
        });
        let value = serde_json::to_value(actor).unwrap();
        assert_eq!(
            value["interactionPolicy"]["canFeature"]["automaticApproval"],
            json!(["https://plamenu.local/users/alice/followers"])
        );
    }

    #[test]
    fn instance_actor_serializes_like_mastodons_representative() {
        let value = serde_json::to_value(InstanceActor::new(
            "plamenu.local",
            "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            Some("z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2"),
        ))
        .unwrap();
        assert_eq!(value["@context"][0], json!(AS_CONTEXT));
        assert_eq!(value["@context"][1], json!(SECURITY_CONTEXT));
        assert_eq!(value["id"], "https://plamenu.local/actor");
        assert_eq!(value["type"], "Application");
        assert_eq!(value["preferredUsername"], "plamenu.local");
        assert_eq!(value["inbox"], "https://plamenu.local/actor/inbox");
        assert_eq!(value["outbox"], "https://plamenu.local/actor/outbox");
        assert_eq!(value["manuallyApprovesFollowers"], true);
        assert_eq!(
            value["publicKey"]["id"],
            "https://plamenu.local/actor#main-key"
        );
        assert_eq!(value["publicKey"]["owner"], "https://plamenu.local/actor");
        assert_eq!(
            value["endpoints"]["sharedInbox"],
            "https://plamenu.local/inbox"
        );
        // None of the user-actor-only fields may appear.
        for absent in ["followers", "following", "featured", "published"] {
            assert!(!value.as_object().unwrap().contains_key(absent), "{absent}");
        }
        // FEP-844e: the server actor lists capabilities at the top level.
        assert_eq!(value["@context"][4]["implements"], "mitra:implements");
        assert_eq!(value["implements"][0]["href"], "https://w3id.org/fep/8fcf");
        assert_eq!(value["implements"][1]["href"], "https://w3id.org/fep/7aa9");
        assert_eq!(value["implements"][4]["href"], "https://w3id.org/fep/044f");
        assert_eq!(value["implements"][5]["href"], "https://w3id.org/fep/e232");
        assert_eq!(
            value["implements"][6]["href"],
            "https://datatracker.ietf.org/doc/html/rfc9421"
        );
        // FEP-521a: the instance actor's Ed25519 Multikey.
        assert_eq!(
            value["assertionMethod"][0]["id"],
            "https://plamenu.local/actor#ed25519-key"
        );
        assert_eq!(
            value["assertionMethod"][0]["controller"],
            "https://plamenu.local/actor"
        );
        // Instance actor documents parse with the lenient remote shape, the
        // way we ingest Mastodon's own `/actor`.
        let as_remote: RemoteActor = serde_json::from_value(value).unwrap();
        assert_eq!(as_remote.kind, "Application");
        assert_eq!(as_remote.preferred_username, "plamenu.local");
    }

    /// FEP-521a ingest: only a `Multikey` controlled by the actor itself,
    /// living on the actor document, with Ed25519 material, is accepted.
    #[test]
    fn remote_actor_ed25519_key_extraction() {
        let actor = |assertion_method: Value| -> RemoteActor {
            serde_json::from_value(json!({
                "id": "https://mitra.example/users/erin",
                "type": "Person",
                "inbox": "https://mitra.example/users/erin/inbox",
                "publicKey": {
                    "id": "https://mitra.example/users/erin#main-key",
                    "owner": "https://mitra.example/users/erin",
                    "publicKeyPem": "PEM",
                },
                "assertionMethod": assertion_method,
            }))
            .unwrap()
        };
        let ed25519 = "z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2";

        // Mitra publishes the RSA Multikey first and the Ed25519 one second —
        // the RSA entry must be skipped, not fail the extraction.
        let mitra_style = actor(json!([
            {
                "id": "https://mitra.example/users/erin#main-key",
                "type": "Multikey",
                "controller": "https://mitra.example/users/erin",
                "publicKeyMultibase": "zDrrBcqCvCJEqBzR7vLpsyStGXfSTvLq6ExpKfDL",
            },
            {
                "id": "https://mitra.example/users/erin#ed25519-key",
                "type": "Multikey",
                "controller": "https://mitra.example/users/erin",
                "publicKeyMultibase": ed25519,
            },
        ]));
        assert_eq!(mitra_style.ed25519_public_key(), Some(ed25519));

        // A single object (not an array) also parses.
        let single = actor(json!({
            "id": "https://mitra.example/users/erin#ed25519-key",
            "type": "Multikey",
            "controller": "https://mitra.example/users/erin",
            "publicKeyMultibase": ed25519,
        }));
        assert_eq!(single.ed25519_public_key(), Some(ed25519));

        // A key controlled by (or hosted on) someone else must be ignored.
        let foreign_controller = actor(json!([{
            "id": "https://mitra.example/users/erin#ed25519-key",
            "type": "Multikey",
            "controller": "https://evil.example/users/mallory",
            "publicKeyMultibase": ed25519,
        }]));
        assert_eq!(foreign_controller.ed25519_public_key(), None);
        let foreign_host = actor(json!([{
            "id": "https://evil.example/users/mallory#ed25519-key",
            "type": "Multikey",
            "controller": "https://mitra.example/users/erin",
            "publicKeyMultibase": ed25519,
        }]));
        assert_eq!(foreign_host.ed25519_public_key(), None);

        // Foreign verification-method types are skipped.
        let ed2018 = actor(json!([{
            "id": "https://mitra.example/users/erin#key",
            "type": "Ed25519VerificationKey2018",
            "controller": "https://mitra.example/users/erin",
            "publicKeyBase58": "irrelevant",
        }]));
        assert_eq!(ed2018.ed25519_public_key(), None);

        // Absent entirely.
        assert_eq!(actor(json!([])).ed25519_public_key(), None);
    }

    #[test]
    fn remote_verification_methods_accept_fep_only_and_exact_mixed_keys() {
        let ed = crate::multikey::encode_ed25519_public(&[7; 32]);
        let ml =
            crate::multikey::encode_ml_dsa_44_public(&[9; crate::multikey::ML_DSA_44_PUBLIC_LEN]);
        let actor: RemoteActor = serde_json::from_value(json!({
            "id": "https://keys.example/actors/alice",
            "type": "Person",
            "inbox": "https://keys.example/actors/alice/inbox",
            "assertionMethod": [
                {
                    "id": "https://keys.example/actors/alice#ed",
                    "type": "Multikey",
                    "controller": "https://keys.example/actors/alice",
                    "publicKeyMultibase": ed,
                    "expires": "2030-01-01T00:00:00Z",
                },
                {
                    "id": "https://keys.example/actors/alice#ml",
                    "type": "Multikey",
                    "controller": "https://keys.example/actors/alice",
                    "publicKeyMultibase": ml,
                },
                "https://keys.example/keys/alice-rotated"
            ]
        }))
        .unwrap();

        let methods = actor.verification_methods().unwrap();
        assert_eq!(methods.len(), 3);
        assert_eq!(methods[0].algorithm.as_deref(), Some("ed25519"));
        assert_eq!(
            methods[0].expires_at.unwrap().year(),
            2030,
            "inline lifecycle metadata reaches normalized storage"
        );
        assert!(!methods[0].revoked);
        assert_eq!(methods[1].algorithm.as_deref(), Some("ml-dsa-44"));
        assert_eq!(methods[2].algorithm, None);
        assert_eq!(methods[2].public_key, None);

        let mut revoked = actor.clone();
        revoked.assertion_method[0]["revoked"] = json!("2029-01-01T00:00:00Z");
        assert!(revoked.verification_methods().unwrap()[0].revoked);
        let mut invalid = actor;
        invalid.assertion_method[0]["expires"] = json!("not-a-time");
        assert_eq!(
            invalid.verification_methods(),
            Err(VerificationMethodError::InvalidLifecycle)
        );
    }

    #[test]
    fn remote_verification_methods_accept_fep_ae97_canonical_client_key() {
        let actor_id = "https://gateway.example/.well-known/apgateway/\
            did:key:z6MkiTBz1ymuepAQ4HEHYSF1H8quG5GLVVQR3djdX3mDooWp/actors/alice";
        let canonical_id =
            "ap://did:key:z6MkiTBz1ymuepAQ4HEHYSF1H8quG5GLVVQR3djdX3mDooWp/actors/alice";
        let actor: RemoteActor = serde_json::from_value(json!({
            "id": actor_id,
            "type": "Person",
            "inbox": format!("{actor_id}/inbox"),
            "assertionMethod": [{
                "id": format!("{canonical_id}#main-key"),
                "type": "Multikey",
                "controller": actor_id,
                "publicKeyMultibase": crate::multikey::encode_ed25519_public(&[7; 32]),
            }],
        }))
        .unwrap();

        let methods = actor.verification_methods().unwrap();
        assert_eq!(methods[0].key_uri, format!("{canonical_id}#main-key"));
        assert_eq!(methods[0].controller_uri, actor_id);
    }

    #[test]
    fn remote_verification_methods_match_mastodon_independent_ten_key_bounds() {
        let actor_id = "https://keys.example/actors/many";
        let fep: Vec<_> = (0..10)
            .map(|n| Value::String(format!("https://keys.example/fep/{n}")))
            .collect();
        let classic: Vec<_> = (0..10)
            .map(|n| Value::String(format!("https://keys.example/classic/{n}")))
            .collect();
        let actor: RemoteActor = serde_json::from_value(json!({
            "id": actor_id,
            "type": "Person",
            "inbox": format!("{actor_id}/inbox"),
            "assertionMethod": fep,
            "publicKey": classic,
        }))
        .unwrap();
        let methods = actor.verification_methods().unwrap();
        assert_eq!(methods.len(), 20);
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.source == "classic-external")
                .count(),
            10
        );

        let too_many_classic: RemoteActor = serde_json::from_value(json!({
            "id": actor_id,
            "type": "Person",
            "inbox": format!("{actor_id}/inbox"),
            "publicKey": (0..11)
                .map(|n| Value::String(format!("https://keys.example/classic/{n}")))
                .collect::<Vec<_>>(),
        }))
        .unwrap();
        assert_eq!(
            too_many_classic.verification_methods(),
            Err(VerificationMethodError::TooMany)
        );
    }

    #[test]
    fn remote_verification_methods_reject_controller_conflict_and_overflow() {
        let ed = crate::multikey::encode_ed25519_public(&[3; 32]);
        let parse = |methods: Value| -> RemoteActor {
            serde_json::from_value(json!({
                "id": "https://keys.example/actors/alice",
                "type": "Person",
                "inbox": "https://keys.example/actors/alice/inbox",
                "assertionMethod": methods,
            }))
            .unwrap()
        };
        let wrong = parse(json!({
            "id": "https://keys.example/actors/alice#ed",
            "type": "Multikey",
            "controller": "https://evil.example/actor",
            "publicKeyMultibase": ed,
        }));
        assert_eq!(
            wrong.verification_methods(),
            Err(VerificationMethodError::WrongController)
        );

        let too_many = parse(Value::Array(
            (0..11)
                .map(|n| Value::String(format!("https://keys.example/keys/{n}")))
                .collect(),
        ));
        assert_eq!(
            too_many.verification_methods(),
            Err(VerificationMethodError::TooMany)
        );

        let conflict = parse(json!([
            {
                "id": "https://keys.example/actors/alice#ed",
                "type": "Multikey",
                "controller": "https://keys.example/actors/alice",
                "publicKeyMultibase": crate::multikey::encode_ed25519_public(&[1; 32]),
            },
            {
                "id": "https://keys.example/actors/alice#ed",
                "type": "Multikey",
                "controller": "https://keys.example/actors/alice",
                "publicKeyMultibase": crate::multikey::encode_ed25519_public(&[2; 32]),
            }
        ]));
        assert_eq!(
            conflict.verification_methods(),
            Err(VerificationMethodError::ConflictingDuplicate)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn remote_actor_parses_minimal_and_full_documents() {
        // Minimal: what a small non-Mastodon server might publish.
        let minimal: RemoteActor = serde_json::from_value(json!({
            "id": "https://tiny.example/u/bob",
            "type": "Person",
            "preferredUsername": "bob",
            "inbox": "https://tiny.example/u/bob/inbox",
            "publicKey": {
                "id": "https://tiny.example/u/bob#main-key",
                "owner": "https://tiny.example/u/bob",
                "publicKeyPem": "PEM",
            },
        }))
        .unwrap();
        assert_eq!(minimal.shared_inbox_or_empty(), "");
        assert_eq!(minimal.followers_url(), None);
        assert_eq!(minimal.following_url(), None);
        assert_eq!(minimal.outbox_url(), None);
        assert!(minimal.also_known_as_uris().is_empty());
        assert_eq!(minimal.moved_to_uri(), None);

        // FEP-2c59: Mastodon accepts an explicit `webfinger` acct as the
        // handle source, even when `preferredUsername` is omitted.
        let webfinger_only: RemoteActor = serde_json::from_value(json!({
            "id": "https://social.example/users/leyka",
            "type": "Person",
            "webfinger": "acct:leyka@example.org",
            "inbox": "https://social.example/users/leyka/inbox",
            "publicKey": {
                "id": "https://social.example/users/leyka#main-key",
                "owner": "https://social.example/users/leyka",
                "publicKeyPem": "PEM",
            },
        }))
        .unwrap();
        assert_eq!(webfinger_only.preferred_username, "");
        assert_eq!(
            webfinger_only.webfinger_acct().unwrap().to_string(),
            "leyka@example.org"
        );

        // `alsoKnownAs` is accepted as both a bare IRI and an array, and
        // `movedTo` as a string or an object — Mastodon publishes the array
        // and string forms.
        let aliased: RemoteActor = serde_json::from_value(json!({
            "id": "https://new.example/users/alice",
            "type": "Person",
            "preferredUsername": "alice",
            "inbox": "https://new.example/users/alice/inbox",
            "alsoKnownAs": ["https://old.example/users/alice"],
            "movedTo": "https://newer.example/users/alice",
            "publicKey": { "id": "k", "owner": "o", "publicKeyPem": "PEM" },
        }))
        .unwrap();
        assert_eq!(
            aliased.also_known_as_uris(),
            vec!["https://old.example/users/alice".to_owned()]
        );
        assert_eq!(
            aliased.moved_to_uri(),
            Some("https://newer.example/users/alice")
        );
        let single_alias: RemoteActor = serde_json::from_value(json!({
            "id": "https://new.example/users/carol",
            "type": "Person",
            "preferredUsername": "carol",
            "inbox": "https://new.example/users/carol/inbox",
            "alsoKnownAs": "https://old.example/users/carol",
            "publicKey": { "id": "k", "owner": "o", "publicKeyPem": "PEM" },
        }))
        .unwrap();
        assert_eq!(
            single_alias.also_known_as_uris(),
            vec!["https://old.example/users/carol".to_owned()]
        );

        // GoToSocial may publish singleton objects for fields Mastodon often
        // serializes as arrays, and may use non-object endpoint extensions.
        let gotosocial: RemoteActor = serde_json::from_value(json!({
            "id": "https://social.example/users/leyka",
            "type": "Person",
            "preferredUsername": "leyka",
            "inbox": "https://social.example/users/leyka/inbox",
            "tag": {
                "type": "Emoji",
                "id": "https://social.example/emoji/1",
                "name": ":blob:",
                "icon": { "type": "Image", "url": "https://social.example/emoji/1.png" },
            },
            "attachment": {
                "type": "PropertyValue",
                "name": "Website",
                "value": "<a href=\"https://example.org\">example.org</a>",
            },
            "alsoKnownAs": "https://old.example/users/leyka",
            "endpoints": "https://social.example/users/leyka/endpoints",
            "interactionPolicy": "https://social.example/users/leyka/interaction_policy",
            "publicKey": { "id": "k", "owner": "o", "publicKeyPem": "PEM" },
        }))
        .unwrap();
        assert_eq!(gotosocial.tag.len(), 1);
        assert_eq!(gotosocial.attachment.len(), 1);
        assert_eq!(gotosocial.shared_inbox_or_empty(), "");
        assert_eq!(
            gotosocial.also_known_as_uris(),
            vec!["https://old.example/users/leyka".to_owned()]
        );

        // Full: our own actor documents parse as remote actors too.
        let own = serde_json::to_value(sample()).unwrap();
        let full: RemoteActor = serde_json::from_value(own).unwrap();
        assert_eq!(full.preferred_username, "alice");
        // The profile-tab settings and memorial marker round-trip.
        assert!(full.memorial);
        assert!(full.indexable);
        assert_eq!(full.show_media, Some(false));
        assert_eq!(full.show_replies_in_media, Some(false));
        assert_eq!(full.show_featured, Some(true));
        // A document omitting the profile-tab keys parses them as `None`
        // (the ingest then keeps any stored value).
        assert_eq!(minimal.show_media, None);
        assert_eq!(minimal.show_featured, None);
        assert!(!minimal.memorial);
        assert_eq!(
            full.followers_url(),
            Some("https://plamenu.local/users/alice/followers")
        );
        assert_eq!(
            full.following_url(),
            Some("https://plamenu.local/users/alice/following")
        );
        assert_eq!(
            full.outbox_url(),
            Some("https://plamenu.local/users/alice/outbox")
        );
        assert_eq!(full.shared_inbox_or_empty(), "https://plamenu.local/inbox");
        assert_eq!(
            full.public_key.public_key_pem.trim_end(),
            sample().public_key.public_key_pem.trim_end()
        );

        let embedded_collections: RemoteActor = serde_json::from_value(json!({
            "id": "https://array.example/users/alice",
            "type": "Person",
            "preferredUsername": "alice",
            "inbox": "https://array.example/users/alice/inbox",
            "followers": [{ "id": "https://array.example/followers" }],
            "following": { "id": "https://array.example/following" },
            "outbox": { "id": "https://array.example/outbox" },
            "publicKey": { "id": "k", "owner": "o", "publicKeyPem": "PEM" },
        }))
        .unwrap();
        assert_eq!(
            embedded_collections.followers_url(),
            Some("https://array.example/followers")
        );
        assert_eq!(
            embedded_collections.following_url(),
            Some("https://array.example/following")
        );
        assert_eq!(
            embedded_collections.outbox_url(),
            Some("https://array.example/outbox")
        );
    }

    #[test]
    fn is_bot_follows_the_actor_type() {
        let actor = |kind: &str| -> RemoteActor {
            serde_json::from_value(json!({
                "id": "https://bots.example/u/x",
                "type": kind,
                "preferredUsername": "x",
                "inbox": "https://bots.example/u/x/inbox",
                "publicKey": { "id": "k", "owner": "o", "publicKeyPem": "PEM" },
            }))
            .unwrap()
        };
        // Mastodon marks `Service` and `Application` actors as bots.
        assert!(actor("Service").is_bot());
        assert!(actor("Application").is_bot());
        // Everything else is a person, not a bot.
        assert!(!actor("Person").is_bot());
        assert!(!actor("Group").is_bot());
        assert!(!actor("Organization").is_bot());
    }

    /// A remote Group document with `extra` merged in.
    fn group_actor(extra: &Value) -> RemoteActor {
        let mut doc = json!({
            "id": "https://peer.example/c/rust",
            "type": "Group",
            "preferredUsername": "rust",
            "inbox": "https://peer.example/c/rust/inbox",
            "publicKey": { "id": "k", "owner": "o", "publicKeyPem": "PEM" },
        });
        doc.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(doc).unwrap()
    }

    #[test]
    fn group_flags_are_read_from_the_community_extensions() {
        // Lemmy's community flags.
        let lemmy = group_actor(&json!({
            "sensitive": true,
            "postingRestrictedToMods": true,
        }));
        assert_eq!(lemmy.sensitive, Some(true));
        assert_eq!(lemmy.posting_restricted_to_mods, Some(true));
        // Absent is "not stated", never false — the ingest keeps them apart.
        let bare = group_actor(&json!({}));
        assert_eq!(bare.sensitive, None);
        assert_eq!(bare.posting_restricted_to_mods, None);
        assert_eq!(bare.posting_policy, None);
        // Our own tri-state rides beside Lemmy's boolean.
        let ours = group_actor(&json!({
            "postingRestrictedToMods": false,
            "postingPolicy": "members",
        }));
        assert_eq!(ours.posting_policy.as_deref(), Some("members"));
    }

    #[test]
    fn group_moderators_reads_every_deployed_shape() {
        // Lemmy (and us): `attributedTo` is the moderators collection.
        assert_eq!(
            group_actor(&json!({ "attributedTo": "https://peer.example/c/rust/moderators" }))
                .group_moderators(),
            Some(GroupModerators::Collection(
                "https://peer.example/c/rust/moderators"
            ))
        );
        // An untyped object reference is still a collection...
        assert_eq!(
            group_actor(&json!({ "attributedTo": { "id": "https://peer.example/mods" } }))
                .group_moderators(),
            Some(GroupModerators::Collection("https://peer.example/mods"))
        );
        // ...while a typed actor is a moderator named inline.
        assert_eq!(
            group_actor(&json!({
                "attributedTo": { "type": "Person", "id": "https://peer.example/u/ada" }
            }))
            .group_moderators(),
            Some(GroupModerators::Inline(vec!["https://peer.example/u/ada"]))
        );
        // PeerTube: a list naming the owning Person beside the channel Group,
        // which is the community itself and not a moderator.
        assert_eq!(
            group_actor(&json!({
                "attributedTo": [
                    { "type": "Person", "id": "https://peer.example/accounts/ada" },
                    { "type": "Group", "id": "https://peer.example/c/rust" },
                ]
            }))
            .group_moderators(),
            Some(GroupModerators::Inline(vec![
                "https://peer.example/accounts/ada"
            ]))
        );
        // go-fed peers wrap a single value in an array; unwrap before deciding,
        // so a lone collection IRI is not mistaken for a moderator.
        assert_eq!(
            group_actor(&json!({ "attributedTo": ["https://peer.example/c/rust/moderators"] }))
                .group_moderators(),
            Some(GroupModerators::Collection(
                "https://peer.example/c/rust/moderators"
            ))
        );
        // Mitra publishes no `attributedTo` at all — only FEP-5219.
        assert_eq!(
            group_actor(
                &json!({ "affiliations": "https://peer.example/ap/actors/1/affiliations" })
            )
            .group_moderators(),
            Some(GroupModerators::Affiliations(
                "https://peer.example/ap/actors/1/affiliations"
            ))
        );
        // `attributedTo` wins when both are published (as we publish both).
        assert_eq!(
            group_actor(&json!({
                "attributedTo": "https://peer.example/c/rust/moderators",
                "affiliations": "https://peer.example/c/rust/affiliations",
            }))
            .group_moderators(),
            Some(GroupModerators::Collection(
                "https://peer.example/c/rust/moderators"
            ))
        );
        // Nothing published, nothing claimed.
        assert_eq!(group_actor(&json!({})).group_moderators(), None);
        assert_eq!(
            group_actor(&json!({ "attributedTo": [] })).group_moderators(),
            None
        );
    }

    #[test]
    fn moderator_affiliations_are_the_upper_rungs() {
        assert!(is_moderator_affiliation("admin"));
        assert!(is_moderator_affiliation("owner"));
        assert!(is_moderator_affiliation("moderator"));
        assert!(!is_moderator_affiliation("member"));
        assert!(!is_moderator_affiliation("none"));
    }

    #[test]
    fn actor_type_whitelists_known_kinds() {
        let actor = |kind: &str| -> RemoteActor {
            serde_json::from_value(json!({
                "id": "https://ex.example/u/x",
                "type": kind,
                "preferredUsername": "x",
                "inbox": "https://ex.example/u/x/inbox",
                "publicKey": { "id": "k", "owner": "o", "publicKeyPem": "PEM" },
            }))
            .unwrap()
        };
        assert_eq!(actor("Group").actor_type(), Some("Group"));
        assert_eq!(actor("Person").actor_type(), Some("Person"));
        assert_eq!(actor("Organization").actor_type(), Some("Organization"));
        // An unrecognised type stores as `None` (read as Person).
        assert_eq!(actor("Spaceship").actor_type(), None);
    }

    #[test]
    fn image_url_handles_the_shapes_in_the_wild() {
        // Mastodon: an Image object with a string url.
        let own = serde_json::to_value(sample()).unwrap();
        let parsed: RemoteActor = serde_json::from_value(own).unwrap();
        assert_eq!(
            parsed.icon.as_ref().and_then(image_url),
            Some("https://plamenu.local/media/1.png")
        );
        assert_eq!(parsed.attachment.len(), 1);

        assert_eq!(
            image_url(&json!("https://x/a.png")),
            Some("https://x/a.png")
        );
        assert_eq!(
            image_url(&json!({"url": {"href": "https://x/a.png"}})),
            Some("https://x/a.png")
        );
        assert_eq!(
            image_url(&json!([{"url": "https://x/a.png"}, {"url": "https://x/b.png"}])),
            Some("https://x/a.png")
        );
        assert_eq!(image_url(&json!({"type": "Image"})), None);
        assert_eq!(image_url(&json!(42)), None);
    }

    #[test]
    fn roundtrips_through_serde() {
        let actor = sample();
        let json = serde_json::to_string(&actor).unwrap();
        let back: Actor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, actor.id);
        assert_eq!(back.kind, ActorType::Person);
    }
}
