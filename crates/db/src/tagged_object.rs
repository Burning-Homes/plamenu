//! Collections referenced by a status (Mastodon's `tagged_objects`): text
//! links to collections in local posts and inbound Notes' `FeaturedCollection`
//! tag entries. Feeds the Status entity's `tagged_collections` and the
//! outbound Note's `tag` array.

use std::collections::HashMap;

use sqlx::PgExecutor;

use crate::collection::Collection;
use crate::{DbError, id};

/// One status→collection reference.
#[derive(Debug, Clone)]
pub struct TaggedObject {
    pub id: i64,
    pub status_id: i64,
    pub collection_id: i64,
    /// The URI the reference arrived under (the tag `id` / the link as typed).
    pub uri: String,
}

/// Records a reference, keeping an existing row (a status references each
/// collection at most once).
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn add<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
    collection_id: i64,
    uri: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO tagged_objects (id, status_id, collection_id, uri)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (status_id, collection_id) DO NOTHING",
        id::next(),
        status_id,
        collection_id,
        uri,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// The references a status currently holds.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn for_status<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<Vec<TaggedObject>, DbError> {
    let rows = sqlx::query_as!(
        TaggedObject,
        "SELECT id, status_id, collection_id, uri FROM tagged_objects WHERE status_id = $1",
        status_id,
    )
    .fetch_all(executor)
    .await?;
    Ok(rows)
}

/// Drops references a re-scan no longer found (Mastodon's
/// `ProcessLinksService` removing objects "no longer contained in the text").
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn remove<'e, E: PgExecutor<'e>>(executor: E, ids: &[i64]) -> Result<(), DbError> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query!("DELETE FROM tagged_objects WHERE id = ANY($1)", ids)
        .execute(executor)
        .await?;
    Ok(())
}

/// The referenced collections for a batch of statuses, keyed by status id in
/// insertion order — the Status entity's `tagged_collections`.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn collections_for_statuses<'e, E: PgExecutor<'e>>(
    executor: E,
    status_ids: &[i64],
) -> Result<HashMap<i64, Vec<Collection>>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT t.status_id,
               c.id, c.account_id, c.name, c.description, c.language,
               c.sensitive, c.discoverable, c.local, c.tag_id, c.uri, c.url,
               c.original_number_of_items, c.created_at, c.updated_at
        FROM tagged_objects t
        JOIN collections c ON c.id = t.collection_id
        WHERE t.status_id = ANY($1)
        ORDER BY t.id
        "#,
        status_ids,
    )
    .fetch_all(executor)
    .await?;
    let mut map: HashMap<i64, Vec<Collection>> = HashMap::new();
    for row in rows {
        map.entry(row.status_id).or_default().push(Collection {
            id: row.id,
            account_id: row.account_id,
            name: row.name,
            description: row.description,
            language: row.language,
            sensitive: row.sensitive,
            discoverable: row.discoverable,
            local: row.local,
            tag_id: row.tag_id,
            uri: row.uri,
            url: row.url,
            original_number_of_items: row.original_number_of_items,
            created_at: row.created_at,
            updated_at: row.updated_at,
        });
    }
    Ok(map)
}
