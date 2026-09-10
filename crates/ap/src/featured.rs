//! FEP-7aa9 "Featured Collections" wire vocabulary: the `FeaturedCollection`
//! object, its `FeaturedItem`s, the `FeatureRequest`/`FeatureAuthorization`
//! consent handshake (a sibling of the FEP-044f quote flow in
//! [`crate::activity`]), and the `Add`/`Remove`/`Update`/`Delete` activities
//! that distribute changes. Shapes mirror what Mastodon puts on the wire
//! for featured collections.

use serde_json::{Value, json};

use crate::activity::PUBLIC;
use crate::{AS_CONTEXT, urls};

/// The JSON-LD context an actor / `FeaturedCollection` ships so the FEP-7aa9
/// and hashtag terms resolve (Mastodon's `discoverable`, `featured_collections`,
/// `hashtag`, `sensitive` extensions, folded into one `@context`).
#[must_use]
pub fn featured_context() -> Value {
    json!([AS_CONTEXT, {
        "toot": "http://joinmastodon.org/ns#",
        "discoverable": "toot:discoverable",
        "sensitive": "as:sensitive",
        "Hashtag": "as:Hashtag",
        "FeaturedCollection": "https://w3id.org/fep/7aa9#FeaturedCollection",
        "FeaturedItem": "https://w3id.org/fep/7aa9#FeaturedItem",
        "FeatureRequest": "https://w3id.org/fep/7aa9#FeatureRequest",
        "FeatureAuthorization": "https://w3id.org/fep/7aa9#FeatureAuthorization",
        "topic": { "@id": "https://w3id.org/fep/7aa9#topic", "@type": "@id" },
        "featuredObject": { "@id": "https://w3id.org/fep/7aa9#featuredObject", "@type": "@id" },
        "featureAuthorization": { "@id": "https://w3id.org/fep/7aa9#featureAuthorization", "@type": "@id" },
    }])
}

/// The context for a `FeatureRequest` (Mastodon's `feature_requests` extension).
#[must_use]
pub fn feature_request_context() -> Value {
    json!([AS_CONTEXT, {
        "FeatureRequest": "https://w3id.org/fep/7aa9#FeatureRequest",
    }])
}

/// The context for a `FeatureAuthorization` stamp (Mastodon's
/// `feature_authorizations` extension, gts-namespaced).
#[must_use]
pub fn feature_authorization_context() -> Value {
    json!([AS_CONTEXT, {
        "gts": "https://gotosocial.org/ns#",
        "FeatureAuthorization": "https://w3id.org/fep/7aa9#FeatureAuthorization",
        "interactingObject": { "@id": "gts:interactingObject", "@type": "@id" },
        "interactionTarget": { "@id": "gts:interactionTarget", "@type": "@id" },
    }])
}

/// A `FeaturedItem` (one membership) — `featured_object` is the featured
/// actor's URI, `feature_authorization` the stamp URL (local owner's stamp for
/// a local member, the member's granted stamp for a remote one).
#[derive(Debug)]
pub struct FeaturedItemParams<'a> {
    pub id: &'a str,
    pub featured_object: &'a str,
    pub feature_authorization: Option<&'a str>,
    pub published: &'a str,
}

#[must_use]
pub fn featured_item(params: &FeaturedItemParams<'_>) -> Value {
    let mut item = json!({
        "id": params.id,
        "type": "FeaturedItem",
        "featuredObject": params.featured_object,
        "published": params.published,
    });
    if let Some(authorization) = params.feature_authorization {
        item["featureAuthorization"] = json!(authorization);
    }
    item
}

/// The data a `FeaturedCollection` object carries on the wire.
#[derive(Debug)]
pub struct FeaturedCollectionParams<'a> {
    pub id: &'a str,
    pub attributed_to: &'a str,
    pub url: &'a str,
    pub name: &'a str,
    /// The plain/HTML description; emitted as `summaryMap` when `language` is
    /// set, otherwise as `summary`.
    pub summary: &'a str,
    pub language: Option<&'a str>,
    pub sensitive: bool,
    pub discoverable: bool,
    pub published: &'a str,
    pub updated: &'a str,
    /// Count of *accepted* members (Mastodon's `total_items`).
    pub total_items: u64,
    /// The `topic` Hashtag tag object, if the collection is tied to a tag.
    pub topic: Option<Value>,
    /// The accepted members as `FeaturedItem`s.
    pub items: Vec<Value>,
}

/// The bare `FeaturedCollection` object (no `@context`) for embedding inside an
/// `Add`/`Update` activity, or as a paged endpoint item.
#[must_use]
pub fn featured_collection_object(params: &FeaturedCollectionParams<'_>) -> Value {
    let mut object = json!({
        "id": params.id,
        "type": "FeaturedCollection",
        "totalItems": params.total_items,
        "name": params.name,
        "attributedTo": params.attributed_to,
        "url": params.url,
        "sensitive": params.sensitive,
        "discoverable": params.discoverable,
        "published": params.published,
        "updated": params.updated,
        "orderedItems": params.items,
    });
    match params.language {
        Some(language) => object["summaryMap"] = json!({ language: params.summary }),
        None => object["summary"] = json!(params.summary),
    }
    if let Some(topic) = &params.topic {
        object["topic"] = topic.clone();
    }
    object
}

/// The standalone `FeaturedCollection` document (with `@context`), served at
/// the collection's own URL.
#[must_use]
pub fn featured_collection(params: &FeaturedCollectionParams<'_>) -> Value {
    let mut object = featured_collection_object(params);
    object["@context"] = featured_context();
    object
}

/// `Add(FeaturedCollection)` — the owner announces a whole new collection to
/// their followers (`target` = their `featuredCollections` endpoint).
#[must_use]
pub fn add_featured_collection(
    actor: &str,
    featured_collections_url: &str,
    object: &Value,
) -> Value {
    json!({
        "@context": featured_context(),
        "type": "Add",
        "actor": actor,
        "target": featured_collections_url,
        "object": object,
    })
}

/// `Remove(FeaturedCollection)` — the owner retires a collection.
#[must_use]
pub fn remove_featured_collection(
    actor: &str,
    featured_collections_url: &str,
    collection_uri: &str,
) -> Value {
    json!({
        "@context": featured_context(),
        "type": "Remove",
        "actor": actor,
        "target": featured_collections_url,
        "object": collection_uri,
    })
}

/// `Update(FeaturedCollection)` — a metadata or membership change, addressed
/// to the public collection (`to: [Public]`), like Mastodon's serializer.
#[must_use]
pub fn update_featured_collection(
    actor: &str,
    collection_uri: &str,
    object: &Value,
    epoch: i64,
) -> Value {
    json!({
        "@context": featured_context(),
        "id": format!("{collection_uri}#updates/{epoch}"),
        "type": "Update",
        "actor": actor,
        "to": [PUBLIC],
        "object": object,
    })
}

/// `Add(FeaturedItem)` — a single membership added to an existing collection
/// (`target` = the collection's URI).
#[must_use]
pub fn add_featured_item(actor: &str, collection_uri: &str, object: &Value) -> Value {
    json!({
        "@context": featured_context(),
        "type": "Add",
        "actor": actor,
        "target": collection_uri,
        "object": object,
    })
}

/// `Remove(FeaturedItem)` — a single membership removed (`object` = item URI).
#[must_use]
pub fn remove_featured_item(actor: &str, collection_uri: &str, item_uri: &str) -> Value {
    json!({
        "@context": featured_context(),
        "type": "Remove",
        "actor": actor,
        "target": collection_uri,
        "object": item_uri,
    })
}

/// An outbound `FeatureRequest`: asks `featured_account_uri` for consent to be
/// listed in `collection_uri`. Like Mastodon's serializer it carries no
/// `actor` — the receiver derives it from the signature and the collection's
/// `attributedTo`.
#[must_use]
pub fn feature_request(
    activity_uri: &str,
    featured_account_uri: &str,
    collection_uri: &str,
) -> Value {
    json!({
        "@context": feature_request_context(),
        "id": activity_uri,
        "type": "FeatureRequest",
        "object": featured_account_uri,
        "instrument": collection_uri,
    })
}

/// The `FeatureAuthorization` stamp served at
/// [`urls::feature_authorization_uri`]; the consent proof a featured account
/// grants, fetched by others to verify a feature.
#[must_use]
pub fn feature_authorization(
    stamp_uri: &str,
    interaction_target_account_uri: &str,
    interacting_object_collection_uri: &str,
) -> Value {
    json!({
        "@context": feature_authorization_context(),
        "id": stamp_uri,
        "type": "FeatureAuthorization",
        "interactingObject": interacting_object_collection_uri,
        "interactionTarget": interaction_target_account_uri,
    })
}

/// Parameters for answering an inbound `FeatureRequest` (a remote owner adding
/// our local account to their collection).
#[derive(Debug)]
pub struct FeatureResponseParams<'a> {
    pub domain: &'a str,
    /// Our local (featured) account's username.
    pub username: &'a str,
    /// Persisted canonical actor ID (`None` for legacy fixtures).
    pub actor_id: Option<&'a str>,
    /// The `collection_items` row id (stamp/activity id segment).
    pub item_id: i64,
    /// The original `FeatureRequest`'s id, echoed as `object`.
    pub request_activity_uri: &'a str,
    /// The remote collection owner's actor URI (the `to` audience).
    pub owner_uri: &'a str,
}

/// `Accept(FeatureRequest)` with our freshly-minted stamp as `result`.
#[must_use]
pub fn accept_feature_request(params: &FeatureResponseParams<'_>) -> Value {
    let actor =
        urls::LocalUserUrls::for_account(params.domain, params.username, params.actor_id).id;
    let result = urls::feature_authorization_uri_for_actor(&actor, params.item_id);
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{actor}#accepts/feature_requests/{}", params.item_id),
        "type": "Accept",
        "actor": actor,
        "to": params.owner_uri,
        "object": params.request_activity_uri,
        "result": result,
    })
}

/// `Reject(FeatureRequest)`.
#[must_use]
pub fn reject_feature_request(params: &FeatureResponseParams<'_>) -> Value {
    let actor =
        urls::LocalUserUrls::for_account(params.domain, params.username, params.actor_id).id;
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{actor}#rejects/feature_requests/{}", params.item_id),
        "type": "Reject",
        "actor": actor,
        "to": params.owner_uri,
        "object": params.request_activity_uri,
    })
}

/// `Delete(FeatureAuthorization)` — a featured account revoking the consent it
/// granted, addressed to the collection owner (`to: [Public]` like Mastodon).
#[must_use]
pub fn delete_feature_authorization(actor: &str, stamp: &Value) -> Value {
    let stamp_id = stamp["id"].as_str().unwrap_or_default();
    json!({
        "@context": feature_authorization_context(),
        "id": format!("{stamp_id}#delete"),
        "type": "Delete",
        "actor": actor,
        "to": [PUBLIC],
        "object": stamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLECTION: &str = "https://plamenu.test/users/alice/collections/7";
    const OWNER: &str = "https://plamenu.test/users/alice";

    #[test]
    fn featured_collection_uses_summary_or_summary_map() {
        let base = FeaturedCollectionParams {
            id: COLLECTION,
            attributed_to: OWNER,
            url: "https://plamenu.test/@alice/collections/7",
            name: "Mutuals",
            summary: "my pals",
            language: None,
            sensitive: false,
            discoverable: true,
            published: "2026-06-28T00:00:00Z",
            updated: "2026-06-28T00:00:00Z",
            total_items: 1,
            topic: None,
            items: vec![],
        };
        let plain = featured_collection(&base);
        assert_eq!(plain["type"], "FeaturedCollection");
        assert_eq!(plain["summary"], "my pals");
        assert!(plain.get("summaryMap").is_none());
        assert_eq!(plain["totalItems"], 1);
        assert_eq!(plain["attributedTo"], OWNER);

        let localized = featured_collection(&FeaturedCollectionParams {
            language: Some("en"),
            ..base
        });
        assert_eq!(localized["summaryMap"], json!({ "en": "my pals" }));
        assert!(localized.get("summary").is_none());
    }

    #[test]
    fn featured_item_omits_authorization_when_absent() {
        let without = featured_item(&FeaturedItemParams {
            id: "https://plamenu.test/users/alice/collections/7/items/9",
            featured_object: "https://remote.test/users/bob",
            feature_authorization: None,
            published: "2026-06-28T00:00:00Z",
        });
        assert!(without.get("featureAuthorization").is_none());
        let with = featured_item(&FeaturedItemParams {
            feature_authorization: Some("https://remote.test/users/bob/stamp"),
            ..FeaturedItemParams {
                id: "x",
                featured_object: "y",
                feature_authorization: None,
                published: "z",
            }
        });
        assert_eq!(
            with["featureAuthorization"],
            "https://remote.test/users/bob/stamp"
        );
    }

    #[test]
    fn feature_request_has_no_actor_and_carries_instrument() {
        let request = feature_request(
            "https://plamenu.test/users/alice/feature_requests/9",
            "https://remote.test/users/bob",
            COLLECTION,
        );
        assert_eq!(request["type"], "FeatureRequest");
        assert_eq!(request["object"], "https://remote.test/users/bob");
        assert_eq!(request["instrument"], COLLECTION);
        assert!(
            request.get("actor").is_none(),
            "Mastodon's FeatureRequest has no actor"
        );
    }

    #[test]
    fn accept_feature_request_carries_the_stamp_as_result() {
        let accept = accept_feature_request(&FeatureResponseParams {
            domain: "plamenu.test",
            username: "bob",
            actor_id: None,
            item_id: 9,
            request_activity_uri: "https://remote.test/users/alice/feature_requests/1",
            owner_uri: "https://remote.test/users/alice",
        });
        assert_eq!(accept["type"], "Accept");
        assert_eq!(accept["actor"], "https://plamenu.test/users/bob");
        assert_eq!(
            accept["object"],
            "https://remote.test/users/alice/feature_requests/1"
        );
        assert_eq!(
            accept["result"],
            "https://plamenu.test/users/bob/feature_authorizations/9"
        );
    }

    #[test]
    fn add_and_remove_collection_target_the_endpoint() {
        let object = featured_collection_object(&FeaturedCollectionParams {
            id: COLLECTION,
            attributed_to: OWNER,
            url: "https://plamenu.test/@alice/collections/7",
            name: "Mutuals",
            summary: "",
            language: None,
            sensitive: false,
            discoverable: true,
            published: "2026-06-28T00:00:00Z",
            updated: "2026-06-28T00:00:00Z",
            total_items: 0,
            topic: None,
            items: vec![],
        });
        let endpoint = "https://plamenu.test/users/alice/featured_collections";
        let add = add_featured_collection(OWNER, endpoint, &object);
        assert_eq!(add["type"], "Add");
        assert_eq!(add["target"], endpoint);
        assert_eq!(add["object"]["id"], COLLECTION);
        assert!(
            add["object"].get("@context").is_none(),
            "embedded object drops its context"
        );

        let remove = remove_featured_collection(OWNER, endpoint, COLLECTION);
        assert_eq!(remove["type"], "Remove");
        assert_eq!(remove["object"], COLLECTION);
    }
}
