//! Account collections (Mastodon 4.6 / FEP-7aa9 "Featured Collections"): a
//! curated, shareable set of up to 25 accounts owned by one account, optionally
//! tied to a hashtag. Each membership (`collection_items`) carries a consent
//! state machine — a remote member stays `pending` until they grant an approval
//! stamp, mirroring the FEP-044f quote handshake (see [`crate::quote`]).

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

/// Mastodon's `Collection::MAX_ITEMS` (pending or accepted memberships).
pub const MAX_ITEMS: i64 = 25;
/// Local `name` length limit (characters).
pub const NAME_LENGTH_LIMIT: usize = 40;
/// Local `description` length limit (characters).
pub const DESCRIPTION_LENGTH_LIMIT: usize = 100;
/// Remote `name` hard limit — Mastodon's `NAME_LENGTH_HARD_LIMIT`.
pub const NAME_LENGTH_HARD_LIMIT: usize = 256;
/// Remote sanitized-HTML `description` hard limit.
pub const DESCRIPTION_LENGTH_HARD_LIMIT: usize = 2048;
/// Per-account collection cap. Mastodon reads it off `UserRole.collection_limit`
/// (default 10); Plamenu is headless and role-free, so the default is the cap.
pub const PER_ACCOUNT_LIMIT: i64 = 10;

/// The membership states `collection_items.state` admits (Mastodon's integer
/// enum, spelled out for the REST `state` field).
pub const ITEM_STATES: [&str; 4] = ["pending", "accepted", "rejected", "revoked"];

/// A collection row, the source for the REST `Collection` entity and the AP
/// `FeaturedCollection` object.
#[derive(Debug, Clone)]
pub struct Collection {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    /// Plain text for local collections, sanitized HTML for remote ones
    /// (mutually exclusive by `local`).
    pub description: String,
    pub language: Option<String>,
    pub sensitive: bool,
    pub discoverable: bool,
    pub local: bool,
    pub tag_id: Option<i64>,
    /// AP id of a remote collection; `None` for local ones (derived from the
    /// URL layout).
    pub uri: Option<String>,
    pub url: Option<String>,
    /// The `totalItems` a remote collection advertised.
    pub original_number_of_items: Option<i32>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

const COLLECTION_COLS: &str = "id, account_id, name, description, language, sensitive, \
     discoverable, local, tag_id, uri, url, original_number_of_items, created_at, updated_at";
const _: &str = COLLECTION_COLS;

/// A collection membership.
#[derive(Debug, Clone)]
pub struct CollectionItem {
    pub id: i64,
    pub collection_id: i64,
    /// `None` while the featured remote actor is unresolved (`object_uri`
    /// holds its URI then).
    pub account_id: Option<i64>,
    /// `pending` | `accepted` | `rejected` | `revoked`.
    pub state: String,
    pub position: i32,
    /// The `FeaturedItem`'s AP id (remote items).
    pub uri: Option<String>,
    /// The featured actor's URI, recorded when `account_id` is unresolved.
    pub object_uri: Option<String>,
    /// The feature-request activity we minted and sent (local collection
    /// featuring a remote account).
    pub activity_uri: Option<String>,
    /// The authorization stamp granted by the featured account.
    pub approval_uri: Option<String>,
    pub approval_last_verified_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

const ITEM_COLS: &str = "id, collection_id, account_id, state, position, uri, object_uri, \
     activity_uri, approval_uri, approval_last_verified_at, created_at, updated_at";
const _: &str = ITEM_COLS;

/// Attributes for a new collection. Local collections leave `uri`/`url`/
/// `original_number_of_items` `None` (their URLs derive from the layout);
/// remote ones carry the fetched values.
#[derive(Debug)]
pub struct NewCollection<'a> {
    pub account_id: i64,
    pub name: &'a str,
    pub description: &'a str,
    pub language: Option<&'a str>,
    pub sensitive: bool,
    pub discoverable: bool,
    pub local: bool,
    pub tag_id: Option<i64>,
    pub uri: Option<&'a str>,
    pub url: Option<&'a str>,
    pub original_number_of_items: Option<i32>,
}

/// Inserts a collection row.
pub async fn create<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    new: NewCollection<'_>,
) -> Result<Collection, DbError> {
    let collection = sqlx::query_as!(
        Collection,
        r#"
        INSERT INTO collections
            (id, account_id, name, description, language, sensitive, discoverable,
             local, tag_id, uri, url, original_number_of_items)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        RETURNING id, account_id, name, description, language, sensitive,
                  discoverable, local, tag_id, uri, url, original_number_of_items,
                  created_at, updated_at
        "#,
        id::next(),
        new.account_id,
        new.name,
        new.description,
        new.language,
        new.sensitive,
        new.discoverable,
        new.local,
        new.tag_id,
        new.uri,
        new.url,
        new.original_number_of_items,
    )
    .fetch_one(pool)
    .await?;
    Ok(collection)
}

/// A collection by id (unscoped — the REST `show`/`update`/`destroy` look the
/// row up first, then authorize).
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn find<'e, E: PgExecutor<'e>>(
    executor: E,
    collection_id: i64,
) -> Result<Option<Collection>, DbError> {
    let collection = sqlx::query_as!(
        Collection,
        r#"
        SELECT id, account_id, name, description, language, sensitive,
               discoverable, local, tag_id, uri, url, original_number_of_items,
               created_at, updated_at
        FROM collections WHERE id = $1
        "#,
        collection_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(collection)
}

/// [`find`] over a set of ids in one query — the notification renderer
/// resolves every distinct referenced collection on a page with one round
/// trip. Unknown ids are simply absent.
pub async fn find_by_ids(
    pool: &PgPool,
    collection_ids: &[i64],
) -> Result<Vec<Collection>, DbError> {
    if collection_ids.is_empty() {
        return Ok(Vec::new());
    }
    let collections = sqlx::query_as!(
        Collection,
        r#"
        SELECT id, account_id, name, description, language, sensitive,
               discoverable, local, tag_id, uri, url, original_number_of_items,
               created_at, updated_at
        FROM collections WHERE id = ANY($1)
        "#,
        collection_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(collections)
}

/// A remote collection by its human `url` — the link-scanning match for a
/// local status referencing an already-known remote collection.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn find_by_url<'e, E: PgExecutor<'e>>(
    executor: E,
    url: &str,
) -> Result<Option<Collection>, DbError> {
    let collection = sqlx::query_as!(
        Collection,
        r#"
        SELECT id, account_id, name, description, language, sensitive,
               discoverable, local, tag_id, uri, url, original_number_of_items,
               created_at, updated_at
        FROM collections WHERE url = $1
        "#,
        url,
    )
    .fetch_optional(executor)
    .await?;
    Ok(collection)
}

/// A remote collection by its AP id — the find-or-create key for inbound
/// `FeaturedCollection` ingest.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn find_by_uri<'e, E: PgExecutor<'e>>(
    executor: E,
    uri: &str,
) -> Result<Option<Collection>, DbError> {
    let collection = sqlx::query_as!(
        Collection,
        r#"
        SELECT id, account_id, name, description, language, sensitive,
               discoverable, local, tag_id, uri, url, original_number_of_items,
               created_at, updated_at
        FROM collections WHERE uri = $1
        "#,
        uri,
    )
    .fetch_optional(executor)
    .await?;
    Ok(collection)
}

/// A page of an account's collections, newest first (Mastodon orders by
/// `created_at DESC`, which a snowflake id mirrors). `only_discoverable`
/// hides non-discoverable collections — the view a non-owner gets.
pub async fn owned_by(
    pool: &PgPool,
    account_id: i64,
    only_discoverable: bool,
    offset: i64,
    limit: i64,
) -> Result<Vec<Collection>, DbError> {
    let collections = sqlx::query_as!(
        Collection,
        r#"
        SELECT id, account_id, name, description, language, sensitive,
               discoverable, local, tag_id, uri, url, original_number_of_items,
               created_at, updated_at
        FROM collections
        WHERE account_id = $1 AND (NOT $2 OR discoverable)
        ORDER BY id DESC
        LIMIT $3 OFFSET $4
        "#,
        account_id,
        only_discoverable,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(collections)
}

/// How many collections `account_id` owns — the per-account limit check and
/// the `records_continue?` pagination total.
pub async fn count_owned(pool: &PgPool, account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM collections WHERE account_id = $1"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Mutable collection attributes (the handler merges absent params first).
#[derive(Debug)]
pub struct ChangeCollection<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub language: Option<&'a str>,
    pub sensitive: bool,
    pub discoverable: bool,
    pub tag_id: Option<i64>,
}

/// Replaces a collection's attributes, bumping `updated_at`.
pub async fn update<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    collection_id: i64,
    change: ChangeCollection<'_>,
) -> Result<Collection, DbError> {
    let collection = sqlx::query_as!(
        Collection,
        r#"
        UPDATE collections
        SET name = $2, description = $3, language = $4, sensitive = $5,
            discoverable = $6, tag_id = $7, updated_at = now()
        WHERE id = $1
        RETURNING id, account_id, name, description, language, sensitive,
                  discoverable, local, tag_id, uri, url, original_number_of_items,
                  created_at, updated_at
        "#,
        collection_id,
        change.name,
        change.description,
        change.language,
        change.sensitive,
        change.discoverable,
        change.tag_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(collection)
}

/// Replaces a remote collection's attributes on re-fetch, keyed by `uri`.
/// Returns the refreshed row.
pub async fn update_remote(
    pool: &PgPool,
    collection_id: i64,
    new: &NewCollection<'_>,
) -> Result<Collection, DbError> {
    let collection = sqlx::query_as!(
        Collection,
        r#"
        UPDATE collections
        SET name = $2, description = $3, language = $4, sensitive = $5,
            discoverable = $6, tag_id = $7, url = $8,
            original_number_of_items = $9, updated_at = now()
        WHERE id = $1
        RETURNING id, account_id, name, description, language, sensitive,
                  discoverable, local, tag_id, uri, url, original_number_of_items,
                  created_at, updated_at
        "#,
        collection_id,
        new.name,
        new.description,
        new.language,
        new.sensitive,
        new.discoverable,
        new.tag_id,
        new.url,
        new.original_number_of_items,
    )
    .fetch_one(pool)
    .await?;
    Ok(collection)
}

/// Deletes a collection owned by `account_id` (items cascade); returns whether
/// a row matched.
pub async fn delete<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    collection_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM collections WHERE id = $1 AND account_id = $2",
        collection_id,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Deletes a remote collection by its AP id (inbound `Remove`/`Delete`).
pub async fn delete_by_uri(pool: &PgPool, uri: &str) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM collections WHERE uri = $1", uri)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// The collections `account_id` is featured in (a pending or accepted member),
/// newest first — the `GET /accounts/{id}/in_collections` listing. The policy
/// restricts this to the account itself, so no block filtering is needed.
pub async fn containing(
    pool: &PgPool,
    account_id: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Collection>, DbError> {
    let collections = sqlx::query_as!(
        Collection,
        r#"
        SELECT c.id, c.account_id, c.name, c.description, c.language, c.sensitive,
               c.discoverable, c.local, c.tag_id, c.uri, c.url,
               c.original_number_of_items, c.created_at, c.updated_at
        FROM collections c
        JOIN collection_items i ON i.collection_id = c.id
        WHERE i.account_id = $1 AND i.state IN ('pending', 'accepted')
        ORDER BY c.id DESC
        LIMIT $2 OFFSET $3
        "#,
        account_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(collections)
}

/// How many collections `account_id` is a pending/accepted member of.
pub async fn count_containing(pool: &PgPool, account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM collection_items
        WHERE account_id = $1 AND state IN ('pending', 'accepted')
        "#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// The items of a collection visible to `viewer`, in display order, with
/// Mastodon's collection-item visibility rules. The owner sees pending and accepted items; everyone
/// else only accepted ones. Items whose featured account is hidden from the
/// viewer (a block either way, or a mute) are dropped (`not_blocked_by`).
/// `viewer` `None` is the anonymous reader (accepted items, no block filter).
pub async fn items_for<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    collection_id: i64,
    owner_id: i64,
    viewer: Option<i64>,
) -> Result<Vec<CollectionItem>, DbError> {
    let is_owner = viewer == Some(owner_id);
    let items = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT id, collection_id, account_id, state, position, uri, object_uri,
               activity_uri, approval_uri, approval_last_verified_at,
               created_at, updated_at
        FROM collection_items
        WHERE collection_id = $1
          AND (state = 'accepted' OR ($2 AND state = 'pending'))
          AND ($3::bigint IS NULL OR account_id IS NULL
               OR NOT account_hidden($3, account_id))
        ORDER BY position ASC, id ASC
        "#,
        collection_id,
        is_owner,
        viewer,
    )
    .fetch_all(pool)
    .await?;
    Ok(items)
}

/// [`items_for`] over many collections in one query, grouped by collection id
/// (collections with no visible items are absent). The owner test folds into
/// the join — each collection's pending items are visible only to *its* owner
/// — so a page render fetches every referenced collection's member list in one
/// round trip instead of one per collection.
pub async fn items_for_many(
    pool: &PgPool,
    collection_ids: &[i64],
    viewer: Option<i64>,
) -> Result<HashMap<i64, Vec<CollectionItem>>, DbError> {
    let items = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT ci.id, ci.collection_id, ci.account_id, ci.state, ci.position,
               ci.uri, ci.object_uri, ci.activity_uri, ci.approval_uri,
               ci.approval_last_verified_at, ci.created_at, ci.updated_at
        FROM collection_items ci
        JOIN collections c ON c.id = ci.collection_id
        WHERE ci.collection_id = ANY($1)
          AND (ci.state = 'accepted' OR (c.account_id = $2 AND ci.state = 'pending'))
          AND ($2::bigint IS NULL OR ci.account_id IS NULL
               OR NOT account_hidden($2, ci.account_id))
        ORDER BY ci.collection_id ASC, ci.position ASC, ci.id ASC
        "#,
        collection_ids,
        viewer,
    )
    .fetch_all(pool)
    .await?;
    let mut grouped: HashMap<i64, Vec<CollectionItem>> = HashMap::new();
    for item in items {
        grouped.entry(item.collection_id).or_default().push(item);
    }
    Ok(grouped)
}

/// [`items_for_many`] across a set of signed-in viewers in one query, keyed by
/// `(viewer, collection id)` — each viewer's visible member list, with the
/// same owner-sees-pending and `account_hidden` rules applied per viewer.
pub async fn items_for_many_viewers(
    pool: &PgPool,
    collection_ids: &[i64],
    viewer_ids: &[i64],
) -> Result<HashMap<(i64, i64), Vec<CollectionItem>>, DbError> {
    if collection_ids.is_empty() || viewer_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT v.viewer AS "viewer!",
               ci.id, ci.collection_id, ci.account_id, ci.state, ci.position,
               ci.uri, ci.object_uri, ci.activity_uri, ci.approval_uri,
               ci.approval_last_verified_at, ci.created_at, ci.updated_at
        FROM unnest($2::bigint[]) AS v(viewer)
        JOIN collection_items ci ON ci.collection_id = ANY($1)
        JOIN collections c ON c.id = ci.collection_id
        WHERE (ci.state = 'accepted' OR (c.account_id = v.viewer AND ci.state = 'pending'))
          AND (ci.account_id IS NULL OR NOT account_hidden(v.viewer, ci.account_id))
        ORDER BY v.viewer ASC, ci.collection_id ASC, ci.position ASC, ci.id ASC
        "#,
        collection_ids,
        viewer_ids,
    )
    .fetch_all(pool)
    .await?;
    let mut grouped: HashMap<(i64, i64), Vec<CollectionItem>> = HashMap::new();
    for row in rows {
        grouped
            .entry((row.viewer, row.collection_id))
            .or_default()
            .push(CollectionItem {
                id: row.id,
                collection_id: row.collection_id,
                account_id: row.account_id,
                state: row.state,
                position: row.position,
                uri: row.uri,
                object_uri: row.object_uri,
                activity_uri: row.activity_uri,
                approval_uri: row.approval_uri,
                approval_last_verified_at: row.approval_last_verified_at,
                created_at: row.created_at,
                updated_at: row.updated_at,
            });
    }
    Ok(grouped)
}

/// The accepted items of each collection **as its own owner sees them**
/// (members hidden from that owner by a block or mute are dropped), grouped
/// by collection id — what the `FeaturedCollection` AP builders serialize,
/// in one query however many collections a page references. Equivalent to
/// [`items_for`] with `viewer = Some(owner)` filtered to `accepted`.
pub async fn accepted_items_for_owners<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    collection_ids: &[i64],
) -> Result<HashMap<i64, Vec<CollectionItem>>, DbError> {
    if collection_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let items = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT ci.id, ci.collection_id, ci.account_id, ci.state, ci.position,
               ci.uri, ci.object_uri, ci.activity_uri, ci.approval_uri,
               ci.approval_last_verified_at, ci.created_at, ci.updated_at
        FROM collection_items ci
        JOIN collections c ON c.id = ci.collection_id
        WHERE ci.collection_id = ANY($1)
          AND ci.state = 'accepted'
          AND (ci.account_id IS NULL OR NOT account_hidden(c.account_id, ci.account_id))
        ORDER BY ci.collection_id ASC, ci.position ASC, ci.id ASC
        "#,
        collection_ids,
    )
    .fetch_all(pool)
    .await?;
    let mut grouped: HashMap<i64, Vec<CollectionItem>> = HashMap::new();
    for item in items {
        grouped.entry(item.collection_id).or_default().push(item);
    }
    Ok(grouped)
}

/// How many items a collection holds toward [`MAX_ITEMS`] — pending and
/// accepted memberships, the same scope as `items_do_not_exceed_limit`.
pub async fn count_active_items(pool: &PgPool, collection_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM collection_items
        WHERE collection_id = $1 AND state IN ('pending', 'accepted')
        "#,
        collection_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// [`count_active_items`] over a set of collections in one grouped query,
/// keyed by collection id. Collections with no active items are absent
/// (count as zero).
pub async fn count_active_items_many(
    pool: &PgPool,
    collection_ids: &[i64],
) -> Result<HashMap<i64, i64>, DbError> {
    if collection_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT collection_id, COUNT(*) AS "count!"
        FROM collection_items
        WHERE collection_id = ANY($1) AND state IN ('pending', 'accepted')
        GROUP BY collection_id
        "#,
        collection_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.collection_id, row.count))
        .collect())
}

/// Attributes for a new membership. `item_id` is pre-generated by the caller so
/// the authorization-stamp URL can embed it (mirrors [`crate::quote::NewQuote`]).
#[derive(Debug)]
pub struct NewCollectionItem<'a> {
    pub item_id: i64,
    pub collection_id: i64,
    pub account_id: Option<i64>,
    pub state: &'a str,
    pub uri: Option<&'a str>,
    pub object_uri: Option<&'a str>,
    pub activity_uri: Option<&'a str>,
    pub approval_uri: Option<&'a str>,
}

/// Adds a membership, auto-assigning `position = max(position) + 1`. Returns
/// `None` when the account is already a member (the partial unique index), like
/// Mastodon's `find_or_create_by` short-circuit.
pub async fn add_item<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    new: NewCollectionItem<'_>,
) -> Result<Option<CollectionItem>, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        INSERT INTO collection_items
            (id, collection_id, account_id, state, position, uri, object_uri,
             activity_uri, approval_uri)
        VALUES ($1, $2, $3, $4,
                COALESCE((SELECT max(position) + 1 FROM collection_items
                          WHERE collection_id = $2), 1),
                $5, $6, $7, $8)
        ON CONFLICT (collection_id, account_id) WHERE account_id IS NOT NULL
        DO NOTHING
        RETURNING id, collection_id, account_id, state, position, uri, object_uri,
                  activity_uri, approval_uri, approval_last_verified_at,
                  created_at, updated_at
        "#,
        new.item_id,
        new.collection_id,
        new.account_id,
        new.state,
        new.uri,
        new.object_uri,
        new.activity_uri,
        new.approval_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(item)
}

/// An item by id, scoped to its collection (Mastodon's
/// `collection.collection_items.find`).
pub async fn find_item(
    pool: &PgPool,
    collection_id: i64,
    item_id: i64,
) -> Result<Option<CollectionItem>, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT id, collection_id, account_id, state, position, uri, object_uri,
               activity_uri, approval_uri, approval_last_verified_at,
               created_at, updated_at
        FROM collection_items WHERE id = $1 AND collection_id = $2
        "#,
        item_id,
        collection_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(item)
}

/// A membership by its row id alone (the key for the stamp endpoint and
/// inbound `Accept`/`Reject`/`Delete` matching).
pub async fn find_item_by_id(
    pool: &PgPool,
    item_id: i64,
) -> Result<Option<CollectionItem>, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT id, collection_id, account_id, state, position, uri, object_uri,
               activity_uri, approval_uri, approval_last_verified_at,
               created_at, updated_at
        FROM collection_items WHERE id = $1
        "#,
        item_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(item)
}

/// The membership of `account_id` in a collection, if any.
pub async fn find_item_by_account(
    pool: &PgPool,
    collection_id: i64,
    account_id: i64,
) -> Result<Option<CollectionItem>, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT id, collection_id, account_id, state, position, uri, object_uri,
               activity_uri, approval_uri, approval_last_verified_at,
               created_at, updated_at
        FROM collection_items WHERE collection_id = $1 AND account_id = $2
        "#,
        collection_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(item)
}

/// A remote `FeaturedItem` by its AP id, scoped to its collection — the
/// find-or-create key for inbound collection reconciliation.
pub async fn find_item_by_uri(
    pool: &PgPool,
    collection_id: i64,
    uri: &str,
) -> Result<Option<CollectionItem>, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT id, collection_id, account_id, state, position, uri, object_uri,
               activity_uri, approval_uri, approval_last_verified_at,
               created_at, updated_at
        FROM collection_items WHERE collection_id = $1 AND uri = $2
        "#,
        collection_id,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(item)
}

/// A membership by its `approval_uri` and featured account — the key for an
/// inbound `Delete(FeatureAuthorization)` (a remote member revoking consent to
/// a local collection).
pub async fn find_item_by_approval(
    pool: &PgPool,
    account_id: i64,
    approval_uri: &str,
) -> Result<Option<CollectionItem>, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT id, collection_id, account_id, state, position, uri, object_uri,
               activity_uri, approval_uri, approval_last_verified_at,
               created_at, updated_at
        FROM collection_items WHERE account_id = $1 AND approval_uri = $2
        "#,
        account_id,
        approval_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(item)
}

/// Re-links a remote item's wire fields on re-ingest (the `FeaturedItem` id,
/// the featured object URI, the granted stamp and the resolved account).
pub async fn relink_remote_item(
    pool: &PgPool,
    item_id: i64,
    account_id: Option<i64>,
    uri: &str,
    object_uri: &str,
    approval_uri: Option<&str>,
) -> Result<CollectionItem, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        UPDATE collection_items
        SET account_id = COALESCE($2, account_id), uri = $3, object_uri = $4,
            approval_uri = COALESCE($5, approval_uri), state = 'accepted',
            updated_at = now()
        WHERE id = $1
        RETURNING id, collection_id, account_id, state, position, uri, object_uri,
                  activity_uri, approval_uri, approval_last_verified_at,
                  created_at, updated_at
        "#,
        item_id,
        account_id,
        uri,
        object_uri,
        approval_uri,
    )
    .fetch_one(pool)
    .await?;
    Ok(item)
}

/// Deletes a remote collection's items whose `uri` is not in `keep` — the
/// reconciliation step of inbound `FeaturedCollection` ingest. Items with no
/// `uri` yet (a local member just pre-approved, not yet echoed back) are kept.
pub async fn delete_items_not_in(
    pool: &PgPool,
    collection_id: i64,
    keep: &[String],
) -> Result<u64, DbError> {
    let result = sqlx::query!(
        "DELETE FROM collection_items
         WHERE collection_id = $1 AND uri IS NOT NULL AND NOT (uri = ANY($2))",
        collection_id,
        keep,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// An item by the feature-request activity URI we minted — the key for
/// matching a remote member's `Accept`/`Reject` (mirrors
/// [`crate::quote::set_state_by_activity_uri`]).
pub async fn find_item_by_activity_uri(
    pool: &PgPool,
    activity_uri: &str,
) -> Result<Option<CollectionItem>, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT id, collection_id, account_id, state, position, uri, object_uri,
               activity_uri, approval_uri, approval_last_verified_at,
               created_at, updated_at
        FROM collection_items WHERE activity_uri = $1
        "#,
        activity_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(item)
}

/// Sets a membership's state, optionally stamping the granted `approval_uri`
/// (kept when `None`). Returns the updated row.
pub async fn set_item_state<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    item_id: i64,
    state: &str,
    approval_uri: Option<&str>,
) -> Result<CollectionItem, DbError> {
    let item = sqlx::query_as!(
        CollectionItem,
        r#"
        UPDATE collection_items
        SET state = $2, approval_uri = COALESCE($3, approval_uri), updated_at = now()
        WHERE id = $1
        RETURNING id, collection_id, account_id, state, position, uri, object_uri,
                  activity_uri, approval_uri, approval_last_verified_at,
                  created_at, updated_at
        "#,
        item_id,
        state,
        approval_uri,
    )
    .fetch_one(pool)
    .await?;
    Ok(item)
}

/// Deletes a membership by id; returns whether a row matched.
pub async fn delete_item<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    item_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM collection_items WHERE id = $1", item_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::block;

    async fn local(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    fn new_local(account_id: i64, name: &str) -> NewCollection<'_> {
        NewCollection {
            account_id,
            name,
            description: "",
            language: None,
            sensitive: false,
            discoverable: true,
            local: true,
            tag_id: None,
            uri: None,
            url: None,
            original_number_of_items: None,
        }
    }

    fn member(
        collection_id: i64,
        account_id: i64,
        state: &'static str,
    ) -> NewCollectionItem<'static> {
        NewCollectionItem {
            item_id: id::next(),
            collection_id,
            account_id: Some(account_id),
            state,
            uri: None,
            object_uri: None,
            activity_uri: None,
            approval_uri: None,
        }
    }

    #[sqlx::test]
    async fn crud_and_ownership(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let c = create(&pool, new_local(alice, "Mutuals")).await.unwrap();
        assert!(c.local);
        assert_eq!(count_owned(&pool, alice).await.unwrap(), 1);
        assert_eq!(find(&pool, c.id).await.unwrap().unwrap().name, "Mutuals");

        // A non-owner sees only discoverable collections.
        let hidden = create(
            &pool,
            NewCollection {
                discoverable: false,
                ..new_local(alice, "Secret")
            },
        )
        .await
        .unwrap();
        assert_eq!(owned_by(&pool, alice, false, 0, 40).await.unwrap().len(), 2);
        let public = owned_by(&pool, alice, true, 0, 40).await.unwrap();
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].id, c.id);

        let updated = update(
            &pool,
            hidden.id,
            ChangeCollection {
                name: "Now public",
                description: "hi",
                language: Some("en"),
                sensitive: true,
                discoverable: true,
                tag_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.name, "Now public");
        assert!(updated.discoverable);
        assert_eq!(owned_by(&pool, alice, true, 0, 40).await.unwrap().len(), 2);

        // delete is owner-scoped.
        assert!(!delete(&pool, bob, c.id).await.unwrap());
        assert!(delete(&pool, alice, c.id).await.unwrap());
        assert_eq!(count_owned(&pool, alice).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn items_state_and_block_filtering(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let c = create(&pool, new_local(alice, "Friends")).await.unwrap();

        let accepted = add_item(&pool, member(c.id, bob, "accepted"))
            .await
            .unwrap()
            .unwrap();
        let pending = add_item(&pool, member(c.id, carol, "pending"))
            .await
            .unwrap()
            .unwrap();
        add_item(&pool, member(c.id, dave, "accepted"))
            .await
            .unwrap()
            .unwrap();
        // Positions auto-increment.
        assert_eq!(accepted.position, 1);
        assert_eq!(pending.position, 2);

        // Re-adding the same account is a no-op.
        assert!(
            add_item(&pool, member(c.id, bob, "accepted"))
                .await
                .unwrap()
                .is_none()
        );

        // The owner sees pending + accepted; a stranger only accepted.
        let owner_view = items_for(&pool, c.id, alice, Some(alice)).await.unwrap();
        assert_eq!(owner_view.len(), 3);
        let stranger_view = items_for(&pool, c.id, alice, Some(bob)).await.unwrap();
        assert_eq!(stranger_view.len(), 2);
        assert!(stranger_view.iter().all(|i| i.state == "accepted"));
        let anon_view = items_for(&pool, c.id, alice, None).await.unwrap();
        assert_eq!(anon_view.len(), 2);

        // A viewer who blocks dave does not see him.
        block::create(&pool, carol, dave, None).await.unwrap();
        let carol_view = items_for(&pool, c.id, alice, Some(carol)).await.unwrap();
        assert_eq!(carol_view.len(), 1, "dave is blocked, only bob remains");
        assert_eq!(carol_view[0].account_id, Some(bob));

        assert_eq!(count_active_items(&pool, c.id).await.unwrap(), 3);

        // The grouped batch form must apply the same owner/state/block rules
        // per collection as the singular query — including a second collection
        // whose pending items are visible to *its* owner only.
        let c2 = create(&pool, new_local(bob, "Bobs")).await.unwrap();
        add_item(&pool, member(c2.id, carol, "pending"))
            .await
            .unwrap()
            .unwrap();
        let ids = [c.id, c2.id];
        for viewer in [Some(alice), Some(bob), Some(carol), None] {
            let grouped = items_for_many(&pool, &ids, viewer).await.unwrap();
            for collection in [&c, &c2] {
                let singular = items_for(&pool, collection.id, collection.account_id, viewer)
                    .await
                    .unwrap();
                let batch = grouped.get(&collection.id).cloned().unwrap_or_default();
                assert_eq!(
                    batch.iter().map(|i| i.id).collect::<Vec<_>>(),
                    singular.iter().map(|i| i.id).collect::<Vec<_>>(),
                    "items_for_many diverged from items_for (collection {}, viewer {viewer:?})",
                    collection.id,
                );
            }
        }
    }

    #[sqlx::test]
    async fn item_state_machine_and_in_collections(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let c = create(&pool, new_local(alice, "Friends")).await.unwrap();

        let item = add_item(
            &pool,
            NewCollectionItem {
                activity_uri: Some("https://plamenu.test/users/alice/feature_requests/x"),
                ..member(c.id, bob, "pending")
            },
        )
        .await
        .unwrap()
        .unwrap();

        // bob is featured (pending counts toward in_collections).
        assert_eq!(containing(&pool, bob, 0, 40).await.unwrap().len(), 1);
        assert_eq!(count_containing(&pool, bob).await.unwrap(), 1);

        // Resolve the feature request by its activity uri.
        let found =
            find_item_by_activity_uri(&pool, "https://plamenu.test/users/alice/feature_requests/x")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(found.id, item.id);

        let accepted = set_item_state(&pool, item.id, "accepted", Some("https://x/stamp"))
            .await
            .unwrap();
        assert_eq!(accepted.state, "accepted");
        assert_eq!(accepted.approval_uri.as_deref(), Some("https://x/stamp"));

        // A revoked membership drops out of in_collections.
        set_item_state(&pool, item.id, "revoked", None)
            .await
            .unwrap();
        assert!(containing(&pool, bob, 0, 40).await.unwrap().is_empty());

        assert_eq!(
            find_item_by_account(&pool, c.id, bob)
                .await
                .unwrap()
                .unwrap()
                .id,
            item.id
        );
        assert!(delete_item(&pool, item.id).await.unwrap());
        assert!(!delete_item(&pool, item.id).await.unwrap());
    }

    #[sqlx::test]
    async fn remote_collection_by_uri(pool: PgPool) {
        let owner = local(&pool, "owner").await;
        let uri = "https://remote.test/collections/9";
        let c = create(
            &pool,
            NewCollection {
                local: false,
                discoverable: true,
                uri: Some(uri),
                url: Some("https://remote.test/@x/collections/9"),
                original_number_of_items: Some(3),
                ..new_local(owner, "Remote set")
            },
        )
        .await
        .unwrap();
        assert!(!c.local);
        assert_eq!(find_by_uri(&pool, uri).await.unwrap().unwrap().id, c.id);

        let refreshed = update_remote(
            &pool,
            c.id,
            &NewCollection {
                local: false,
                uri: Some(uri),
                original_number_of_items: Some(5),
                ..new_local(owner, "Renamed remote")
            },
        )
        .await
        .unwrap();
        assert_eq!(refreshed.name, "Renamed remote");
        assert_eq!(refreshed.original_number_of_items, Some(5));

        assert!(delete_by_uri(&pool, uri).await.unwrap());
        assert!(find_by_uri(&pool, uri).await.unwrap().is_none());
    }
}
