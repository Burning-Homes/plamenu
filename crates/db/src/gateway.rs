//! Persistence for the FEP-ae97 client-side `ActivityPub` gateway.

use serde_json::Value;
use sqlx::{FromRow, PgExecutor, PgPool};

use crate::{DbError, id};

#[derive(Debug, Clone, FromRow)]
pub struct GatewayActor {
    pub account_id: i64,
    pub actor_uri: String,
    pub inbox_uri: String,
    pub outbox_uri: String,
    pub username: String,
    pub actor: Value,
    pub client_rsa_key_id: String,
    pub client_rsa_public_key: String,
    pub gateway_rsa_public_multikey: String,
}

const ACTOR_COLUMNS: &str = "account_id, actor_uri, inbox_uri, outbox_uri, username, actor, \
client_rsa_key_id, client_rsa_public_key, gateway_rsa_public_multikey";

async fn find_one(
    pool: &PgPool,
    predicate: &'static str,
    value: &str,
) -> Result<Option<GatewayActor>, DbError> {
    let sql = format!("SELECT {ACTOR_COLUMNS} FROM gateway_actors WHERE {predicate} = $1");
    Ok(sqlx::query_as::<_, GatewayActor>(sqlx::AssertSqlSafe(sql))
        .bind(value)
        .fetch_optional(pool)
        .await?)
}

pub async fn find_by_actor_uri(
    pool: &PgPool,
    actor_uri: &str,
) -> Result<Option<GatewayActor>, DbError> {
    find_one(pool, "actor_uri", actor_uri).await
}

pub async fn find_by_inbox_uri(
    pool: &PgPool,
    inbox_uri: &str,
) -> Result<Option<GatewayActor>, DbError> {
    find_one(pool, "inbox_uri", inbox_uri).await
}

pub async fn find_by_outbox_uri(
    pool: &PgPool,
    outbox_uri: &str,
) -> Result<Option<GatewayActor>, DbError> {
    find_one(pool, "outbox_uri", outbox_uri).await
}

pub async fn find_by_client_key_id(
    pool: &PgPool,
    key_id: &str,
) -> Result<Option<GatewayActor>, DbError> {
    find_one(pool, "client_rsa_key_id", key_id).await
}

pub async fn find_by_account_id(
    pool: &PgPool,
    account_id: i64,
) -> Result<Option<GatewayActor>, DbError> {
    let sql = format!("SELECT {ACTOR_COLUMNS} FROM gateway_actors WHERE account_id = $1");
    Ok(sqlx::query_as::<_, GatewayActor>(sqlx::AssertSqlSafe(sql))
        .bind(account_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn find_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<GatewayActor>, DbError> {
    let sql =
        format!("SELECT {ACTOR_COLUMNS} FROM gateway_actors WHERE lower(username) = lower($1)");
    Ok(sqlx::query_as::<_, GatewayActor>(sqlx::AssertSqlSafe(sql))
        .bind(username)
        .fetch_optional(pool)
        .await?)
}

pub struct NewGatewayActor<'a> {
    pub account_id: i64,
    pub actor_uri: &'a str,
    pub inbox_uri: &'a str,
    pub outbox_uri: &'a str,
    pub username: &'a str,
    pub actor: &'a Value,
    pub client_rsa_key_id: &'a str,
    pub client_rsa_public_key: &'a str,
    pub gateway_rsa_public_multikey: &'a str,
}

/// Inserts a gateway actor after its account row has been locked. `false`
/// means a concurrent registration won and the caller should return its keys.
pub async fn insert<'e, E: PgExecutor<'e>>(
    executor: E,
    new: NewGatewayActor<'_>,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        r"
        INSERT INTO gateway_actors
            (account_id, actor_uri, inbox_uri, outbox_uri, username, actor,
             client_rsa_key_id, client_rsa_public_key, gateway_rsa_public_multikey)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (account_id) DO NOTHING
        ",
    )
    .bind(new.account_id)
    .bind(new.actor_uri)
    .bind(new.inbox_uri)
    .bind(new.outbox_uri)
    .bind(new.username)
    .bind(new.actor)
    .bind(new.client_rsa_key_id)
    .bind(new.client_rsa_public_key)
    .bind(new.gateway_rsa_public_multikey)
    .execute(executor)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(database) if database.is_unique_violation() => DbError::UsernameTaken,
        _ => DbError::Sqlx(error),
    })?;
    Ok(result.rows_affected() == 1)
}

pub async fn mark_account_portable<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
) -> Result<(), DbError> {
    sqlx::query("UPDATE accounts SET portable = true, is_internal = false WHERE id = $1")
        .bind(account_id)
        .execute(executor)
        .await
        .map_err(|error| match &error {
            sqlx::Error::Database(database) if database.is_unique_violation() => {
                DbError::UsernameTaken
            }
            _ => DbError::Sqlx(error),
        })?;
    Ok(())
}

pub async fn update_actor<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    actor: &Value,
) -> Result<(), DbError> {
    sqlx::query("UPDATE gateway_actors SET actor = $2, updated_at = now() WHERE account_id = $1")
        .bind(account_id)
        .bind(actor)
        .execute(executor)
        .await?;
    Ok(())
}

/// Adds a collection item. `false` is an idempotent redelivery to this exact
/// collection; one activity may legitimately be delivered to several
/// portable actors' inboxes.
pub async fn insert_collection_item<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    collection: &str,
    object_uri: &str,
    object: &Value,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        r"
        INSERT INTO gateway_collection_items
            (id, account_id, collection, object_uri, object)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (account_id, collection, object_uri) DO NOTHING
        ",
    )
    .bind(id::next())
    .bind(account_id)
    .bind(collection)
    .bind(object_uri)
    .bind(object)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn collection_items(
    pool: &PgPool,
    account_id: i64,
    collection: &str,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<Value>, DbError> {
    let cursor: Option<i64> = match after {
        Some(uri) => {
            sqlx::query_scalar(
                "SELECT id FROM gateway_collection_items \
                 WHERE account_id = $1 AND collection = $2 AND object_uri = $3",
            )
            .bind(account_id)
            .bind(collection)
            .bind(uri)
            .fetch_optional(pool)
            .await?
        }
        None => None,
    };
    let rows: Vec<(Value,)> = if let Some(cursor) = cursor {
        // Return the oldest next page, in collection (newest-first) order, so
        // repeated polling cannot skip a burst larger than one page.
        sqlx::query_as(
            r"
            SELECT object FROM (
                SELECT id, object
                FROM gateway_collection_items
                WHERE account_id = $1 AND collection = $2 AND id > $3
                ORDER BY id ASC
                LIMIT $4
            ) page
            ORDER BY id DESC
            ",
        )
        .bind(account_id)
        .bind(collection)
        .bind(cursor)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(
            r"
            SELECT object
            FROM gateway_collection_items
            WHERE account_id = $1 AND collection = $2
            ORDER BY id DESC
            LIMIT $3
            ",
        )
        .bind(account_id)
        .bind(collection)
        .bind(limit)
        .fetch_all(pool)
        .await?
    };
    Ok(rows.into_iter().map(|(value,)| value).collect())
}

/// Resolves an item already accepted into one actor's inbox or outbox. This
/// lets later activities (notably Minimitra's `Accept` with a Follow URI as
/// its object) refer to the original activity without trusting another
/// actor's collection or a network refetch.
pub async fn find_collection_item<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    collection: &str,
    object_uri: &str,
) -> Result<Option<Value>, DbError> {
    Ok(sqlx::query_scalar(
        r"
        SELECT object
        FROM gateway_collection_items
        WHERE account_id = $1 AND collection = $2 AND object_uri = $3
        ",
    )
    .bind(account_id)
    .bind(collection)
    .bind(object_uri)
    .fetch_optional(executor)
    .await?)
}

/// One collection owner that has already seen this URI. Outbox push handlers
/// call this while holding the URI's advisory lock to enforce the FEP-ae97
/// "ID has not been used before" rule before ordinary social projection.
pub async fn collection_item_owner<'e, E: PgExecutor<'e>>(
    executor: E,
    object_uri: &str,
) -> Result<Option<i64>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT account_id FROM gateway_collection_items WHERE object_uri = $1 LIMIT 1",
    )
    .bind(object_uri)
    .fetch_optional(executor)
    .await?)
}

/// The portable account that already owns a fetchable gateway object URI.
pub async fn object_owner<'e, E: PgExecutor<'e>>(
    executor: E,
    object_uri: &str,
) -> Result<Option<i64>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT account_id FROM gateway_objects WHERE object_uri = $1")
            .bind(object_uri)
            .fetch_optional(executor)
            .await?,
    )
}

pub async fn put_object<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    object_uri: &str,
    object: &Value,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        r"
        INSERT INTO gateway_objects (object_uri, account_id, object)
        VALUES ($1, $2, $3)
        ON CONFLICT (object_uri) DO NOTHING
        ",
    )
    .bind(object_uri)
    .bind(account_id)
    .bind(object)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn replace_object<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    object_uri: &str,
    object: &Value,
) -> Result<(), DbError> {
    sqlx::query(
        r"
        INSERT INTO gateway_objects (object_uri, account_id, object)
        VALUES ($1, $2, $3)
        ON CONFLICT (object_uri) DO UPDATE
        SET object = EXCLUDED.object, updated_at = now()
        WHERE gateway_objects.account_id = EXCLUDED.account_id
        ",
    )
    .bind(object_uri)
    .bind(account_id)
    .bind(object)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn find_object(pool: &PgPool, object_uri: &str) -> Result<Option<Value>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT object FROM gateway_objects WHERE object_uri = $1")
            .bind(object_uri)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn upsert_follower<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    actor_uri: &str,
    inbox_url: &str,
    accepted: bool,
) -> Result<(), DbError> {
    sqlx::query(
        r"
        INSERT INTO gateway_followers (account_id, actor_uri, inbox_url, accepted)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, actor_uri) DO UPDATE
        SET inbox_url = EXCLUDED.inbox_url,
            accepted = gateway_followers.accepted OR EXCLUDED.accepted,
            updated_at = now()
        ",
    )
    .bind(account_id)
    .bind(actor_uri)
    .bind(inbox_url)
    .bind(accepted)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn set_follower_accepted<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    actor_uri: &str,
    accepted: bool,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE gateway_followers SET accepted = $3, updated_at = now() \
         WHERE account_id = $1 AND actor_uri = $2",
    )
    .bind(account_id)
    .bind(actor_uri)
    .bind(accepted)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn remove_follower<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    actor_uri: &str,
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM gateway_followers WHERE account_id = $1 AND actor_uri = $2")
        .bind(account_id)
        .bind(actor_uri)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn follower_inboxes(pool: &PgPool, account_id: i64) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT gf.inbox_url FROM gateway_followers gf \
         LEFT JOIN accounts a ON a.uri = gf.actor_uri \
         WHERE gf.account_id = $1 AND gf.accepted \
           AND (a.id IS NULL OR (a.domain IS NOT NULL AND a.suspended_at IS NULL)) \
         ORDER BY gf.actor_uri",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, Clone, FromRow)]
pub struct GatewayMedia {
    pub account_id: i64,
    pub digest: Vec<u8>,
    pub file_name: String,
    pub content_type: String,
}

pub async fn insert_media(
    pool: &PgPool,
    account_id: i64,
    digest: &[u8],
    file_name: &str,
    content_type: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        r"
        INSERT INTO gateway_media (account_id, digest, file_name, content_type)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, digest) DO NOTHING
        ",
    )
    .bind(account_id)
    .bind(digest)
    .bind(file_name)
    .bind(content_type)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn find_media(pool: &PgPool, digest: &[u8]) -> Result<Option<GatewayMedia>, DbError> {
    Ok(sqlx::query_as(
        "SELECT account_id, digest, file_name, content_type \
         FROM gateway_media WHERE digest = $1 ORDER BY account_id LIMIT 1",
    )
    .bind(digest)
    .fetch_optional(pool)
    .await?)
}

/// Deletes one owner's reference and returns `(file_name, references_left)`.
pub async fn delete_media(
    pool: &PgPool,
    account_id: i64,
    digest: &[u8],
) -> Result<Option<(String, i64)>, DbError> {
    let mut tx = pool.begin().await?;
    let file_name: Option<String> = sqlx::query_scalar(
        "DELETE FROM gateway_media WHERE account_id = $1 AND digest = $2 RETURNING file_name",
    )
    .bind(account_id)
    .bind(digest)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(file_name) = file_name else {
        return Ok(None);
    };
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway_media WHERE digest = $1")
        .bind(digest)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some((file_name, remaining)))
}
