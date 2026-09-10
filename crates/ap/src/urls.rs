//! Canonical URL layout for local `ActivityPub` resources.
//!
//! Every place that needs "the URL of X" goes through here so the scheme
//! lives in exactly one spot. The layout intentionally mirrors Mastodon's
//! (`/users/:name`, `/users/:name/inbox`, shared `/inbox`) so that remote
//! software with hardcoded assumptions stays happy.

/// URLs for a local user actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalUserUrls {
    pub id: String,
    /// The human web URL (`/@name`), Mastodon's `short_account_url`. This is
    /// the actor's `url`, distinct from the `ActivityPub` `id`.
    pub web_url: String,
    pub inbox: String,
    pub outbox: String,
    pub followers: String,
    pub following: String,
    pub followers_synchronization: String,
    /// The pinned-statuses collection (`toot:featured`).
    pub featured: String,
    /// The featured-hashtags collection (`toot:featuredTags`), Mastodon's
    /// `…/collections/tags`.
    pub featured_tags: String,
    /// The account's `featuredCollections` endpoint (FEP-7aa9), advertised by
    /// the actor as `featuredCollections`.
    pub featured_collections: String,
    /// A Group actor's moderators collection (FEP-1b12 `attributedTo`,
    /// Lemmy's layout); only ever served for `actor_type = 'Group'` accounts.
    pub moderators: String,
    /// A Group actor's FEP-5219 `affiliations` collection.
    pub affiliations: String,
    pub shared_inbox: String,
    pub key_id: String,
    /// The FEP-521a Ed25519 key's id (`#ed25519-key`, Mitra's convention).
    pub ed25519_key_id: String,
}

impl LocalUserUrls {
    /// Legacy username-based actor layout. Kept for actors that published this
    /// URI before immutable local IDs were introduced and for wire fixtures
    /// representing that generation.
    #[must_use]
    pub fn new(domain: &str, username: &str) -> Self {
        let id = format!("https://{domain}/users/{username}");
        Self::from_actor_id(domain, username, id)
    }

    /// Builds every actor-scoped URL from the persisted canonical actor ID.
    /// `username` is used only for the human profile URL; changing it cannot
    /// move the `ActivityPub` actor, collections, inbox, outbox, or key IDs.
    #[must_use]
    pub fn from_actor_id(domain: &str, username: &str, id: String) -> Self {
        Self {
            web_url: format!("https://{domain}/@{username}"),
            inbox: format!("{id}/inbox"),
            outbox: format!("{id}/outbox"),
            followers: format!("{id}/followers"),
            following: format!("{id}/following"),
            followers_synchronization: format!("{id}/followers_synchronization"),
            featured: format!("{id}/collections/featured"),
            featured_tags: format!("{id}/collections/tags"),
            featured_collections: format!("{id}/featured_collections"),
            moderators: format!("{id}/moderators"),
            affiliations: format!("{id}/affiliations"),
            shared_inbox: format!("https://{domain}/inbox"),
            key_id: format!("{id}#main-key"),
            ed25519_key_id: format!("{id}#ed25519-key"),
            id,
        }
    }

    /// URLs for a newly-created handle-independent local actor.
    #[must_use]
    pub fn numeric(domain: &str, username: &str, account_id: i64) -> Self {
        Self::from_actor_id(
            domain,
            username,
            format!("https://{domain}/ap/accounts/{account_id}"),
        )
    }

    /// Uses the persisted local actor URI when present, retaining the legacy
    /// derivation only for pre-backfill/test-fixture rows.
    #[must_use]
    pub fn for_account(domain: &str, username: &str, actor_uri: Option<&str>) -> Self {
        actor_uri.map_or_else(
            || Self::new(domain, username),
            |uri| Self::from_actor_id(domain, username, uri.to_owned()),
        )
    }
}

/// URLs for the instance actor, mirroring Mastodon's layout (`/actor`,
/// `/actor/inbox`, `/actor/outbox`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceActorUrls {
    pub id: String,
    pub inbox: String,
    pub outbox: String,
    pub shared_inbox: String,
    pub key_id: String,
    /// The FEP-521a Ed25519 key's id (`#ed25519-key`, Mitra's convention).
    pub ed25519_key_id: String,
}

impl InstanceActorUrls {
    #[must_use]
    pub fn new(domain: &str) -> Self {
        let id = format!("https://{domain}/actor");
        Self {
            inbox: format!("{id}/inbox"),
            outbox: format!("{id}/outbox"),
            shared_inbox: format!("https://{domain}/inbox"),
            key_id: format!("{id}#main-key"),
            ed25519_key_id: format!("{id}#ed25519-key"),
            id,
        }
    }
}

/// URLs for the collections hanging off a local status, mirroring
/// Mastodon's layout (`/users/:name/statuses/:id/{replies,likes,shares}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStatusUrls {
    pub id: String,
    /// The human web URL (`/@name/{id}`), Mastodon's `short_account_status_url`.
    /// This is the status' `url`, distinct from the `ActivityPub` `id`.
    pub web_url: String,
    pub replies: String,
    pub likes: String,
    pub shares: String,
}

impl LocalStatusUrls {
    #[must_use]
    pub fn new(domain: &str, username: &str, status_id: i64) -> Self {
        Self::from_actor_id(
            domain,
            username,
            &format!("https://{domain}/users/{username}"),
            status_id,
        )
    }

    /// Builds the federated status ID under its author's persisted actor ID;
    /// the human URL intentionally remains handle-based.
    #[must_use]
    pub fn from_actor_id(domain: &str, username: &str, actor_id: &str, status_id: i64) -> Self {
        let id = format!("{actor_id}/statuses/{status_id}");
        Self {
            web_url: format!("https://{domain}/@{username}/{status_id}"),
            replies: format!("{id}/replies"),
            likes: format!("{id}/likes"),
            shares: format!("{id}/shares"),
            id,
        }
    }
}

/// The id our outgoing `Flag` (report) activities carry. It only has to be a
/// unique IRI on our host — the target's server keeps it only when its host
/// matches the sending instance actor's — and is not separately dereferenceable.
#[must_use]
pub fn report_uri(domain: &str, report_id: i64) -> String {
    format!("https://{domain}/reports/{report_id}")
}

/// The AP id of a local account collection (FEP-7aa9), distinct from the
/// numeric-vs-named `…/collections/featured` pinned collection by living under
/// a numeric `…/collections/{id}` of its own.
#[must_use]
pub fn collection_uri(domain: &str, username: &str, collection_id: i64) -> String {
    format!("https://{domain}/users/{username}/collections/{collection_id}")
}

#[must_use]
pub fn collection_uri_for_actor(actor_id: &str, collection_id: i64) -> String {
    format!("{actor_id}/collections/{collection_id}")
}

/// The human web URL of a local account collection (`/@name/collections/{id}`).
#[must_use]
pub fn collection_web_url(domain: &str, username: &str, collection_id: i64) -> String {
    format!("https://{domain}/@{username}/collections/{collection_id}")
}

/// A local account's Atom syndication feed. Hangs off the web profile
/// URL with an `.atom` suffix (`/@name.atom`), the shape RSS/Atom readers and
/// the profile page's `rel=alternate` autodiscovery link both point at.
#[must_use]
pub fn account_atom_url(domain: &str, username: &str) -> String {
    format!("https://{domain}/@{username}.atom")
}

/// The AP id of a locally-owned conversation's FEP-f228 *collection of posts*
/// (its `context`). Top-level, keyed on the conversation snowflake — a
/// conversation may be owned by a Group, so it is not actor-scoped.
#[must_use]
pub fn context_uri(domain: &str, conversation_id: i64) -> String {
    format!("https://{domain}/contexts/{conversation_id}")
}

/// The AP id of a locally-owned conversation's FEP-171b *collection of
/// activities* (its `contextHistory`).
#[must_use]
pub fn context_history_uri(domain: &str, conversation_id: i64) -> String {
    format!("https://{domain}/contexts/{conversation_id}/history")
}

/// Reverse of [`context_uri`]: extracts the conversation id from a local
/// context URL (`/contexts/{id}`), rejecting the `/history` sub-collection.
#[must_use]
pub fn parse_local_context_url(domain: &str, url: &str) -> Option<i64> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    path.strip_prefix("/contexts/")?.parse().ok()
}

/// A keyset page URL within a context collection (posts or activities),
/// forward-only by `min_id`, matching the replies/outbox scheme.
#[must_use]
pub fn context_page_url(context_url: &str, min_id: Option<i64>) -> String {
    let min_id = min_id.map(|id| format!("min_id={id}&")).unwrap_or_default();
    format!("{context_url}?{min_id}page=true")
}

/// The URL of a `FeatureAuthorization` stamp we issue (mirrors
/// [`crate::activity::quote_authorization_uri`]). `item_id` is the
/// `collection_items` row id.
#[must_use]
pub fn feature_authorization_uri(domain: &str, username: &str, item_id: i64) -> String {
    format!("https://{domain}/users/{username}/feature_authorizations/{item_id}")
}

#[must_use]
pub fn feature_authorization_uri_for_actor(actor_id: &str, item_id: i64) -> String {
    format!("{actor_id}/feature_authorizations/{item_id}")
}

/// The id of a feature-request activity we mint when featuring a remote
/// account (Mastodon embeds a UUID; Plamenu keys it on the item id so the
/// `Accept`/`Reject` resolves deterministically).
#[must_use]
pub fn feature_request_uri(domain: &str, username: &str, item_id: i64) -> String {
    format!("https://{domain}/users/{username}/feature_requests/{item_id}")
}

#[must_use]
pub fn feature_request_uri_for_actor(actor_id: &str, item_id: i64) -> String {
    format!("{actor_id}/feature_requests/{item_id}")
}

/// Reverse of [`collection_uri`]: extracts `(username, collection_id)` from a
/// local account-collection URL.
#[must_use]
pub fn parse_local_collection_url<'a>(domain: &str, url: &'a str) -> Option<(&'a str, i64)> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let rest = path.strip_prefix("/users/")?;
    let (username, id_part) = rest.split_once("/collections/")?;
    let collection_id: i64 = id_part.parse().ok()?;
    (!username.is_empty() && !username.contains(['/', '?', '#']))
        .then_some((username, collection_id))
}

/// Reverse of [`collection_web_url`]: extracts `(username, collection_id)`
/// from a local account-collection **web** URL (`/@name/collections/{id}`).
#[must_use]
pub fn parse_local_collection_web_url<'a>(domain: &str, url: &'a str) -> Option<(&'a str, i64)> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let rest = path.strip_prefix("/@")?;
    let (username, id_part) = rest.split_once("/collections/")?;
    let collection_id: i64 = id_part.parse().ok()?;
    (!username.is_empty() && !username.contains(['/', '?', '#']))
        .then_some((username, collection_id))
}

/// Reverse of [`feature_authorization_uri`]: extracts `(username, item_id)`.
#[must_use]
pub fn parse_local_feature_authorization_url<'a>(
    domain: &str,
    url: &'a str,
) -> Option<(&'a str, i64)> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let rest = path.strip_prefix("/users/")?;
    let (username, id_part) = rest.split_once("/feature_authorizations/")?;
    let item_id: i64 = id_part.parse().ok()?;
    (!username.is_empty() && !username.contains(['/', '?', '#'])).then_some((username, item_id))
}

/// A page URL within a replies collection. Parameters appear in Rails'
/// `Hash#to_query` alphabetical order (`min_id`, `only_other_accounts`,
/// `page`), so our links are byte-identical to Mastodon's.
#[must_use]
pub fn replies_page_url(
    replies_url: &str,
    min_id: Option<i64>,
    only_other_accounts: Option<bool>,
) -> String {
    let min_id = min_id.map(|id| format!("min_id={id}&")).unwrap_or_default();
    let only_other = only_other_accounts
        .map(|other| format!("only_other_accounts={other}&"))
        .unwrap_or_default();
    format!("{replies_url}?{min_id}{only_other}page=true")
}

/// A page URL within an outbox. Parameters appear in Rails' `Hash#to_query`
/// alphabetical order (`max_id`, `min_id`, `page`), so our links are
/// byte-identical to Mastodon's.
#[must_use]
pub fn outbox_page_url(outbox_url: &str, max_id: Option<i64>, min_id: Option<i64>) -> String {
    let max_id = max_id.map(|id| format!("max_id={id}&")).unwrap_or_default();
    let min_id = min_id.map(|id| format!("min_id={id}&")).unwrap_or_default();
    format!("{outbox_url}?{max_id}{min_id}page=true")
}

/// Reverse of [`LocalUserUrls::new`]'s `id`: extracts the username when `url`
/// is exactly this instance's actor URL form (`https://{domain}/users/{name}`).
#[must_use]
pub fn parse_local_user_url<'a>(domain: &str, url: &'a str) -> Option<&'a str> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let username = path.strip_prefix("/users/")?;
    (!username.is_empty() && !username.contains(['/', '?', '#'])).then_some(username)
}

/// Extracts the immutable account id from this instance's numeric local actor
/// URL (`https://{domain}/ap/accounts/{id}`). Sub-resources and lookalike hosts
/// are rejected.
#[must_use]
pub fn parse_local_numeric_actor_url(domain: &str, url: &str) -> Option<i64> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let account_id = path.strip_prefix("/ap/accounts/")?;
    (!account_id.is_empty() && !account_id.contains(['/', '?', '#']))
        .then(|| account_id.parse().ok())
        .flatten()
}

/// Extracts `(account_id, collection_id)` from a collection under an immutable
/// actor URI.
#[must_use]
pub fn parse_local_numeric_collection_url(domain: &str, url: &str) -> Option<(i64, i64)> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let rest = path.strip_prefix("/ap/accounts/")?;
    let (account_id, collection_id) = rest.split_once("/collections/")?;
    Some((account_id.parse().ok()?, collection_id.parse().ok()?))
}

/// Extracts `(account_id, item_id)` from a feature authorization under an
/// immutable actor URI.
#[must_use]
pub fn parse_local_numeric_feature_authorization_url(
    domain: &str,
    url: &str,
) -> Option<(i64, i64)> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let rest = path.strip_prefix("/ap/accounts/")?;
    let (account_id, item_id) = rest.split_once("/feature_authorizations/")?;
    Some((account_id.parse().ok()?, item_id.parse().ok()?))
}

/// Extracts `(account_id, status_id)` from a status under an immutable actor
/// URI. Subresources and query/fragment lookalikes fail the integer parse.
#[must_use]
pub fn parse_local_numeric_status_url(domain: &str, url: &str) -> Option<(i64, i64)> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let rest = path.strip_prefix("/ap/accounts/")?;
    let (account_id, status_id) = rest.split_once("/statuses/")?;
    Some((account_id.parse().ok()?, status_id.parse().ok()?))
}

/// Extracts `(username, status_id)` from a local status URL
/// (`https://{domain}/users/{name}/statuses/{id}`).
#[must_use]
pub fn parse_local_status_url<'a>(domain: &str, url: &'a str) -> Option<(&'a str, i64)> {
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let rest = path.strip_prefix("/users/")?;
    let (username, status_part) = rest.split_once("/statuses/")?;
    let status_id: i64 = status_part.parse().ok()?;
    (!username.is_empty() && !username.contains(['/', '?', '#'])).then_some((username, status_id))
}

#[cfg(test)]
mod tests {
    use super::{
        LocalStatusUrls, LocalUserUrls, account_atom_url, collection_uri, context_history_uri,
        context_uri, feature_authorization_uri, outbox_page_url, parse_local_collection_url,
        parse_local_context_url, parse_local_feature_authorization_url,
        parse_local_numeric_actor_url, parse_local_numeric_collection_url,
        parse_local_numeric_feature_authorization_url, parse_local_numeric_status_url,
        parse_local_status_url, parse_local_user_url, replies_page_url,
    };

    #[test]
    fn collection_urls_round_trip() {
        let domain = "plamenu.local";
        let uri = collection_uri(domain, "alice", 7);
        assert_eq!(uri, "https://plamenu.local/users/alice/collections/7");
        assert_eq!(parse_local_collection_url(domain, &uri), Some(("alice", 7)));
        // The named pinned collection is not a numeric account collection.
        assert_eq!(
            parse_local_collection_url(
                domain,
                "https://plamenu.local/users/alice/collections/featured"
            ),
            None
        );
        let stamp = feature_authorization_uri(domain, "alice", 9);
        assert_eq!(
            stamp,
            "https://plamenu.local/users/alice/feature_authorizations/9"
        );
        assert_eq!(
            parse_local_feature_authorization_url(domain, &stamp),
            Some(("alice", 9))
        );
        // The Atom feed hangs off the `/@name` web URL.
        assert_eq!(
            account_atom_url(domain, "alice"),
            "https://plamenu.local/@alice.atom"
        );
    }

    #[test]
    fn user_urls_follow_mastodon_layout() {
        let urls = LocalUserUrls::new("plamenu.local", "alice");
        assert_eq!(urls.id, "https://plamenu.local/users/alice");
        assert_eq!(urls.web_url, "https://plamenu.local/@alice");
        assert_eq!(urls.inbox, "https://plamenu.local/users/alice/inbox");
        assert_eq!(urls.outbox, "https://plamenu.local/users/alice/outbox");
        assert_eq!(
            urls.followers,
            "https://plamenu.local/users/alice/followers"
        );
        assert_eq!(
            urls.following,
            "https://plamenu.local/users/alice/following"
        );
        assert_eq!(
            urls.featured,
            "https://plamenu.local/users/alice/collections/featured"
        );
        assert_eq!(
            urls.featured_collections,
            "https://plamenu.local/users/alice/featured_collections"
        );
        assert_eq!(urls.shared_inbox, "https://plamenu.local/inbox");
        assert_eq!(urls.key_id, "https://plamenu.local/users/alice#main-key");
    }

    #[test]
    fn numeric_actor_urls_keep_human_handle_separate() {
        let urls = LocalUserUrls::numeric("plamenu.local", "alice", 42);
        assert_eq!(urls.id, "https://plamenu.local/ap/accounts/42");
        assert_eq!(urls.web_url, "https://plamenu.local/@alice");
        assert_eq!(urls.inbox, "https://plamenu.local/ap/accounts/42/inbox");
        assert_eq!(urls.outbox, "https://plamenu.local/ap/accounts/42/outbox");
        assert_eq!(urls.key_id, "https://plamenu.local/ap/accounts/42#main-key");
        let renamed = LocalUserUrls::numeric("plamenu.local", "renamed", 42);
        assert_eq!(renamed.id, urls.id);
        assert_eq!(renamed.key_id, urls.key_id);
        assert_ne!(renamed.web_url, urls.web_url);

        let status = LocalStatusUrls::from_actor_id("plamenu.local", "renamed", &renamed.id, 99);
        assert_eq!(
            status.id,
            "https://plamenu.local/ap/accounts/42/statuses/99"
        );
        assert_eq!(status.web_url, "https://plamenu.local/@renamed/99");
    }

    #[test]
    fn instance_actor_urls_follow_mastodon_layout() {
        let urls = super::InstanceActorUrls::new("plamenu.local");
        assert_eq!(urls.id, "https://plamenu.local/actor");
        assert_eq!(urls.inbox, "https://plamenu.local/actor/inbox");
        assert_eq!(urls.outbox, "https://plamenu.local/actor/outbox");
        assert_eq!(urls.shared_inbox, "https://plamenu.local/inbox");
        assert_eq!(urls.key_id, "https://plamenu.local/actor#main-key");
    }

    #[test]
    fn report_uri_lives_on_our_host() {
        assert_eq!(
            super::report_uri("plamenu.local", 42),
            "https://plamenu.local/reports/42"
        );
    }

    #[test]
    fn status_urls_follow_mastodon_layout() {
        let urls = LocalStatusUrls::new("plamenu.local", "alice", 42);
        assert_eq!(urls.id, "https://plamenu.local/users/alice/statuses/42");
        assert_eq!(urls.web_url, "https://plamenu.local/@alice/42");
        assert_eq!(urls.replies, format!("{}/replies", urls.id));
        assert_eq!(urls.likes, format!("{}/likes", urls.id));
        assert_eq!(urls.shares, format!("{}/shares", urls.id));
    }

    #[test]
    fn replies_page_urls_order_params_like_rails() {
        let replies = "https://plamenu.local/users/alice/statuses/42/replies";
        assert_eq!(
            replies_page_url(replies, None, None),
            format!("{replies}?page=true")
        );
        assert_eq!(
            replies_page_url(replies, None, Some(true)),
            format!("{replies}?only_other_accounts=true&page=true")
        );
        assert_eq!(
            replies_page_url(replies, Some(7), Some(false)),
            format!("{replies}?min_id=7&only_other_accounts=false&page=true")
        );
        assert_eq!(
            replies_page_url(replies, Some(7), None),
            format!("{replies}?min_id=7&page=true")
        );
    }

    #[test]
    fn outbox_page_urls_order_params_like_rails() {
        let outbox = "https://plamenu.local/users/alice/outbox";
        assert_eq!(
            outbox_page_url(outbox, None, None),
            format!("{outbox}?page=true")
        );
        assert_eq!(
            outbox_page_url(outbox, Some(42), None),
            format!("{outbox}?max_id=42&page=true")
        );
        assert_eq!(
            outbox_page_url(outbox, None, Some(0)),
            format!("{outbox}?min_id=0&page=true")
        );
        assert_eq!(
            outbox_page_url(outbox, Some(42), Some(7)),
            format!("{outbox}?max_id=42&min_id=7&page=true")
        );
    }

    #[test]
    fn parse_local_status_url_extracts_owner_and_id() {
        assert_eq!(
            parse_local_status_url(
                "plamenu.test",
                "https://plamenu.test/users/alice/statuses/42"
            ),
            Some(("alice", 42))
        );
        for bad in [
            "https://plamenu.test/users/alice/statuses/not-a-number",
            "https://plamenu.test/users/alice",
            "https://other.example/users/alice/statuses/42",
            "https://plamenu.test/users//statuses/42",
        ] {
            assert_eq!(parse_local_status_url("plamenu.test", bad), None, "{bad}");
        }
    }

    #[test]
    fn numeric_subresource_parsers_are_exact() {
        assert_eq!(
            parse_local_numeric_status_url(
                "plamenu.local",
                "https://plamenu.local/ap/accounts/42/statuses/9"
            ),
            Some((42, 9))
        );
        assert_eq!(
            parse_local_numeric_collection_url(
                "plamenu.local",
                "https://plamenu.local/ap/accounts/42/collections/7"
            ),
            Some((42, 7))
        );
        assert_eq!(
            parse_local_numeric_feature_authorization_url(
                "plamenu.local",
                "https://plamenu.local/ap/accounts/42/feature_authorizations/8"
            ),
            Some((42, 8))
        );
        for bad in [
            "https://evil.local/ap/accounts/42/statuses/9",
            "https://plamenu.local/ap/accounts/no/statuses/9",
            "https://plamenu.local/ap/accounts/42/statuses/9/replies",
        ] {
            assert_eq!(parse_local_numeric_status_url("plamenu.local", bad), None);
        }
    }

    #[test]
    fn parse_local_user_url_roundtrips_and_rejects_lookalikes() {
        let domain = "plamenu.local";
        let urls = LocalUserUrls::new(domain, "alice");
        assert_eq!(parse_local_user_url(domain, &urls.id), Some("alice"));

        for bad in [
            "https://plamenu.local/users/",
            "https://plamenu.local/users/alice/inbox",
            "https://plamenu.local/users/alice#main-key",
            "https://other.example/users/alice",
            "https://plamenu.local.evil.example/users/alice",
            "http://plamenu.local/users/alice",
            "https://plamenu.local/@alice",
        ] {
            assert_eq!(parse_local_user_url(domain, bad), None, "{bad}");
        }
    }

    #[test]
    fn numeric_actor_url_roundtrips_and_rejects_subresources() {
        assert_eq!(
            parse_local_numeric_actor_url("plamenu.local", "https://plamenu.local/ap/accounts/42"),
            Some(42)
        );
        for bad in [
            "https://plamenu.local/ap/accounts/",
            "https://plamenu.local/ap/accounts/nope",
            "https://plamenu.local/ap/accounts/42/inbox",
            "https://plamenu.local/ap/accounts/42#main-key",
            "https://other.example/ap/accounts/42",
            "http://plamenu.local/ap/accounts/42",
        ] {
            assert_eq!(parse_local_numeric_actor_url("plamenu.local", bad), None);
        }
    }

    #[test]
    fn context_urls_round_trip() {
        let domain = "plamenu.local";
        assert_eq!(context_uri(domain, 7), "https://plamenu.local/contexts/7");
        assert_eq!(
            context_history_uri(domain, 7),
            "https://plamenu.local/contexts/7/history"
        );
        assert_eq!(
            parse_local_context_url(domain, &context_uri(domain, 7)),
            Some(7)
        );
        for bad in [
            // the /history sub-collection is not a posts collection
            "https://plamenu.local/contexts/7/history",
            "https://plamenu.local/contexts/not-a-number",
            "https://other.example/contexts/7",
            "http://plamenu.local/contexts/7",
            "https://plamenu.local/contexts/",
        ] {
            assert_eq!(parse_local_context_url(domain, bad), None, "{bad}");
        }
    }
}
