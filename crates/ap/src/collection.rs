//! `ActivityPub` collection documents: the followers/following collections
//! remote servers dereference, shaped like Mastodon's
//! `ActivityPub::CollectionSerializer` output.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AS_CONTEXT;

/// The number of items per `OrderedCollectionPage`, matching Mastodon's
/// `FOLLOW_PER_PAGE`.
pub const ITEMS_PER_PAGE: u64 = 12;

/// The top-level `OrderedCollection` document: just the size and a pointer
/// to the first page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderedCollection {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub total_items: u64,
    pub first: String,
}

impl OrderedCollection {
    /// The collection at `collection_url`, whose first page is `?page=1`.
    #[must_use]
    pub fn new(collection_url: &str, total_items: u64) -> Self {
        Self {
            context: json!(AS_CONTEXT),
            id: collection_url.to_owned(),
            kind: "OrderedCollection".to_owned(),
            total_items,
            first: page_url(collection_url, 1),
        }
    }
}

/// One page of an ordered collection, item IRIs inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderedCollectionPage {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub total_items: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    pub part_of: String,
    pub ordered_items: Vec<String>,
}

impl OrderedCollectionPage {
    /// Page `page` (1-based) of the collection at `collection_url`. `next`
    /// and `prev` links are derived from `total_items` and the page number.
    #[must_use]
    pub fn new(collection_url: &str, page: u64, total_items: u64, items: Vec<String>) -> Self {
        let has_next = page.saturating_mul(ITEMS_PER_PAGE) < total_items;
        Self {
            context: json!(AS_CONTEXT),
            id: page_url(collection_url, page),
            kind: "OrderedCollectionPage".to_owned(),
            total_items,
            next: has_next.then(|| page_url(collection_url, page + 1)),
            prev: (page > 1).then(|| page_url(collection_url, page - 1)),
            part_of: collection_url.to_owned(),
            ordered_items: items,
        }
    }
}

fn page_url(collection_url: &str, page: u64) -> String {
    format!("{collection_url}?page={page}")
}

/// The outbox envelope, shaped like Mastodon's outbox endpoint:
/// `first` is the newest page and `last` jumps to the oldest
/// (`?min_id=0&page=true`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxCollection {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub total_items: u64,
    pub first: String,
    pub last: String,
}

impl OutboxCollection {
    #[must_use]
    pub fn new(outbox_url: &str, total_items: u64) -> Self {
        Self {
            context: json!(AS_CONTEXT),
            id: outbox_url.to_owned(),
            kind: "OrderedCollection".to_owned(),
            total_items,
            first: crate::urls::outbox_page_url(outbox_url, None, None),
            last: crate::urls::outbox_page_url(outbox_url, None, Some(0)),
        }
    }
}

/// One page of the outbox: `Create`/`Announce` activities inline, keyset
/// `next`/`prev` links, and — like Mastodon's pages — no `totalItems`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxPage {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    pub part_of: String,
    pub ordered_items: Vec<Value>,
}

impl OutboxPage {
    #[must_use]
    pub fn new(
        context: Value,
        page_id: String,
        outbox_url: &str,
        next: Option<String>,
        prev: Option<String>,
        items: Vec<Value>,
    ) -> Self {
        Self {
            context,
            id: page_id,
            kind: "OrderedCollectionPage".to_owned(),
            next,
            prev,
            part_of: outbox_url.to_owned(),
            ordered_items: items,
        }
    }
}

/// The count-only unordered `Collection` Mastodon serves for a status'
/// likes and shares: no items exposed, just the size.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountCollection {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub total_items: u64,
}

impl CountCollection {
    #[must_use]
    pub fn new(collection_url: &str, total_items: u64) -> Self {
        Self {
            context: json!(AS_CONTEXT),
            id: collection_url.to_owned(),
            kind: "Collection".to_owned(),
            total_items,
        }
    }
}

/// An `OrderedCollection` with the items inline — the shape of Mastodon's
/// featured (pinned statuses) collection, which is never paginated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineOrderedCollection {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub total_items: u64,
    pub ordered_items: Vec<Value>,
}

impl InlineOrderedCollection {
    #[must_use]
    pub fn new(collection_url: &str, items: Vec<Value>) -> Self {
        Self {
            context: json!(AS_CONTEXT),
            id: collection_url.to_owned(),
            kind: "OrderedCollection".to_owned(),
            total_items: items.len() as u64,
            ordered_items: items,
        }
    }
}

/// An unordered `Collection` with its items inline — the shape Mastodon
/// serves for the actor's `featuredTags` collection (`…/collections/tags`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineCollection {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub total_items: u64,
    pub items: Vec<Value>,
}

impl InlineCollection {
    #[must_use]
    pub fn new(collection_url: &str, items: Vec<Value>) -> Self {
        Self {
            context: json!(AS_CONTEXT),
            id: collection_url.to_owned(),
            kind: "Collection".to_owned(),
            total_items: items.len() as u64,
            items,
        }
    }
}

/// One page of a status' replies collection: unordered, forward-only
/// (`next` but never `prev`), items inline — full Notes for local replies,
/// bare IRIs for remote ones, like Mastodon's replies collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepliesPage {
    /// Present when the page is served standalone, absent when inlined
    /// as `first` inside the collection envelope.
    #[serde(rename = "@context", skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    pub part_of: String,
    pub items: Vec<Value>,
}

impl RepliesPage {
    /// A standalone page document (carries its own `@context`).
    #[must_use]
    pub fn standalone(
        context: Value,
        page_url: String,
        replies_url: &str,
        next: Option<String>,
        items: Vec<Value>,
    ) -> Self {
        Self {
            context: Some(context),
            id: page_url,
            kind: "CollectionPage".to_owned(),
            next,
            part_of: replies_url.to_owned(),
            items,
        }
    }
}

/// The bare replies collection envelope: no `totalItems`, the first page
/// inlined, like Mastodon serves it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepliesCollection {
    #[serde(rename = "@context")]
    pub context: Value,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub first: RepliesPage,
}

impl RepliesCollection {
    #[must_use]
    pub fn new(context: Value, replies_url: &str, mut first: RepliesPage) -> Self {
        first.context = None;
        Self {
            context,
            id: replies_url.to_owned(),
            kind: "Collection".to_owned(),
            first,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLECTION: &str = "https://plamenu.local/users/alice/followers";

    #[test]
    fn collection_serializes_with_ap_wire_names() {
        let value = serde_json::to_value(OrderedCollection::new(COLLECTION, 27)).unwrap();
        assert_eq!(
            value,
            json!({
                "@context": AS_CONTEXT,
                "id": COLLECTION,
                "type": "OrderedCollection",
                "totalItems": 27,
                "first": format!("{COLLECTION}?page=1"),
            })
        );
    }

    #[test]
    fn page_serializes_with_ap_wire_names_and_links() {
        let items = vec!["https://remote.example/users/bob".to_owned()];
        let value =
            serde_json::to_value(OrderedCollectionPage::new(COLLECTION, 2, 27, items)).unwrap();
        assert_eq!(
            value,
            json!({
                "@context": AS_CONTEXT,
                "id": format!("{COLLECTION}?page=2"),
                "type": "OrderedCollectionPage",
                "totalItems": 27,
                "next": format!("{COLLECTION}?page=3"),
                "prev": format!("{COLLECTION}?page=1"),
                "partOf": COLLECTION,
                "orderedItems": ["https://remote.example/users/bob"],
            })
        );
    }

    #[test]
    fn first_page_has_no_prev_and_last_page_has_no_next() {
        let first = OrderedCollectionPage::new(COLLECTION, 1, ITEMS_PER_PAGE + 1, Vec::new());
        assert_eq!(
            first.next.as_deref(),
            Some(format!("{COLLECTION}?page=2").as_str())
        );
        assert_eq!(first.prev, None);

        let last = OrderedCollectionPage::new(COLLECTION, 2, ITEMS_PER_PAGE + 1, Vec::new());
        assert_eq!(last.next, None);
        // An exactly-full single page is also last.
        let exact = OrderedCollectionPage::new(COLLECTION, 1, ITEMS_PER_PAGE, Vec::new());
        assert_eq!(exact.next, None);
        // `null` next/prev must be omitted, not serialized.
        let value = serde_json::to_value(exact).unwrap();
        let keys = value.as_object().unwrap();
        assert!(!keys.contains_key("next"));
        assert!(!keys.contains_key("prev"));
    }

    #[test]
    fn roundtrips_through_serde() {
        let page = OrderedCollectionPage::new(COLLECTION, 1, 5, vec!["https://x/u/a".to_owned()]);
        let back: OrderedCollectionPage =
            serde_json::from_str(&serde_json::to_string(&page).unwrap()).unwrap();
        assert_eq!(back.id, page.id);
        assert_eq!(back.ordered_items, page.ordered_items);
    }

    #[test]
    fn outbox_collection_serializes_like_mastodon() {
        let outbox = "https://plamenu.local/users/alice/outbox";
        let value = serde_json::to_value(OutboxCollection::new(outbox, 7)).unwrap();
        assert_eq!(
            value,
            json!({
                "@context": AS_CONTEXT,
                "id": outbox,
                "type": "OrderedCollection",
                "totalItems": 7,
                "first": format!("{outbox}?page=true"),
                "last": format!("{outbox}?min_id=0&page=true"),
            })
        );
    }

    #[test]
    fn outbox_page_serializes_without_total_items() {
        let outbox = "https://plamenu.local/users/alice/outbox";
        let page = OutboxPage::new(
            json!(AS_CONTEXT),
            format!("{outbox}?page=true"),
            outbox,
            Some(format!("{outbox}?max_id=5&page=true")),
            Some(format!("{outbox}?min_id=9&page=true")),
            vec![json!({"type": "Create"})],
        );
        let value = serde_json::to_value(page).unwrap();
        assert_eq!(
            value,
            json!({
                "@context": AS_CONTEXT,
                "id": format!("{outbox}?page=true"),
                "type": "OrderedCollectionPage",
                "next": format!("{outbox}?max_id=5&page=true"),
                "prev": format!("{outbox}?min_id=9&page=true"),
                "partOf": outbox,
                "orderedItems": [{"type": "Create"}],
            })
        );

        // Exhausted pages omit the links entirely, like Mastodon.
        let bare = OutboxPage::new(
            json!(AS_CONTEXT),
            format!("{outbox}?page=true"),
            outbox,
            None,
            None,
            Vec::new(),
        );
        let value = serde_json::to_value(bare).unwrap();
        let keys = value.as_object().unwrap();
        assert!(!keys.contains_key("next"));
        assert!(!keys.contains_key("prev"));
        assert!(!keys.contains_key("totalItems"));
    }

    const STATUS: &str = "https://plamenu.local/users/alice/statuses/42";

    #[test]
    fn count_collection_serializes_like_mastodon_likes() {
        let likes = format!("{STATUS}/likes");
        let value = serde_json::to_value(CountCollection::new(&likes, 3)).unwrap();
        assert_eq!(
            value,
            json!({
                "@context": AS_CONTEXT,
                "id": likes,
                "type": "Collection",
                "totalItems": 3,
            })
        );
    }

    #[test]
    fn inline_ordered_collection_serializes_like_mastodon_featured() {
        let featured = "https://plamenu.local/users/alice/collections/featured";
        let value =
            serde_json::to_value(InlineOrderedCollection::new(featured, Vec::new())).unwrap();
        assert_eq!(
            value,
            json!({
                "@context": AS_CONTEXT,
                "id": featured,
                "type": "OrderedCollection",
                "totalItems": 0,
                "orderedItems": [],
            })
        );
    }

    #[test]
    fn replies_collection_inlines_the_first_page_without_context() {
        let replies = format!("{STATUS}/replies");
        let page = RepliesPage::standalone(
            json!(AS_CONTEXT),
            format!("{replies}?page=true"),
            &replies,
            Some(format!("{replies}?only_other_accounts=true&page=true")),
            vec![json!("https://remote.example/users/bob/statuses/7")],
        );

        // Standalone, the page carries its own @context.
        let standalone = serde_json::to_value(&page).unwrap();
        assert_eq!(
            standalone,
            json!({
                "@context": AS_CONTEXT,
                "id": format!("{replies}?page=true"),
                "type": "CollectionPage",
                "next": format!("{replies}?only_other_accounts=true&page=true"),
                "partOf": replies,
                "items": ["https://remote.example/users/bob/statuses/7"],
            })
        );

        // Inlined as `first`, it must not.
        let envelope =
            serde_json::to_value(RepliesCollection::new(json!(AS_CONTEXT), &replies, page))
                .unwrap();
        assert_eq!(envelope["id"], replies.as_str());
        assert_eq!(envelope["type"], "Collection");
        assert!(!envelope.as_object().unwrap().contains_key("totalItems"));
        let first = envelope["first"].as_object().unwrap();
        assert!(!first.contains_key("@context"));
        assert_eq!(first["partOf"], replies.as_str());
    }

    #[test]
    fn replies_page_omits_next_when_exhausted() {
        let replies = format!("{STATUS}/replies");
        let page = RepliesPage::standalone(
            json!(AS_CONTEXT),
            format!("{replies}?only_other_accounts=true&page=true"),
            &replies,
            None,
            Vec::new(),
        );
        let value = serde_json::to_value(page).unwrap();
        assert!(!value.as_object().unwrap().contains_key("next"));
        assert_eq!(value["items"], json!([]));
    }
}
