//! Explicit topic edges and private saved views; bounded by entries and members.
use super::{PgPool, Tx, invalid};
use crate::DbError;

pub const TOPICS_PER_ENTRY: usize = 16;
pub const LENSES_PER_ACCOUNT: i64 = 20;

/// Match the existing case-insensitive hashtag convention, without accepting
/// free text, URLs, controls or arbitrary labels as topic identities.
#[must_use]
pub fn normalize_topic(value: &str) -> Option<String> {
    let value = value.strip_prefix('#').unwrap_or(value).to_lowercase();
    (!value.is_empty()
        && value.len() <= 256
        && value.chars().count() <= 64
        && value.chars().all(|c| c.is_alphanumeric() || c == '_'))
    .then_some(value)
}

pub(super) async fn replace_topics(
    tx: &mut Tx<'_>,
    id: i64,
    topics: &[String],
) -> Result<(), DbError> {
    if topics.len() > TOPICS_PER_ENTRY
        || topics
            .iter()
            .any(|t| normalize_topic(t).as_ref() != Some(t))
    {
        return Err(invalid("invalid discovery topics"));
    }
    sqlx::query!("DELETE FROM discovery_topics WHERE entry_id=$1", id)
        .execute(&mut **tx)
        .await?;
    sqlx::query!("INSERT INTO discovery_topics(entry_id,topic) SELECT $1,unnest($2::text[]) ON CONFLICT DO NOTHING", id, topics)
        .execute(&mut **tx).await?;
    Ok(())
}

pub async fn entry_topics(pool: &PgPool, id: i64) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT topic FROM discovery_topics WHERE entry_id=$1 ORDER BY topic LIMIT 16",
        id
    )
    .fetch_all(pool)
    .await?)
}

pub async fn saved_topics(pool: &PgPool, viewer: i64) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT topic FROM discovery_lenses WHERE account_id=$1 ORDER BY topic LIMIT 20",
        viewer
    )
    .fetch_all(pool)
    .await?)
}

/// The account lock serializes concurrent saves without holding the global
/// discovery lock. Removal/reset remain possible while discovery is disabled.
pub async fn save_topic(pool: &PgPool, viewer: i64, topic: &str) -> Result<bool, DbError> {
    let topic = normalize_topic(topic).ok_or_else(|| invalid("invalid discovery topic"))?;
    let mut tx = pool.begin().await?;
    sqlx::query!("SELECT id FROM accounts WHERE id=$1 FOR UPDATE", viewer)
        .fetch_one(&mut *tx)
        .await?;
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM discovery_lenses WHERE account_id=$1",
        viewer
    )
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(0);
    let existing = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM discovery_lenses WHERE account_id=$1 AND topic=$2)",
        viewer,
        topic
    )
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(false);
    if !existing && count >= LENSES_PER_ACCOUNT {
        return Ok(false);
    }
    sqlx::query!(
        "INSERT INTO discovery_lenses(account_id,topic) VALUES($1,$2) ON CONFLICT DO NOTHING",
        viewer,
        topic
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// `None` removes all saved topics. Dismissals and ordinary blocks are separate
/// explicit choices and are not silently undone by resetting saved views.
pub async fn remove_topics(pool: &PgPool, viewer: i64, topic: Option<&str>) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SELECT id FROM accounts WHERE id=$1 FOR UPDATE", viewer)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM discovery_lenses WHERE account_id=$1 AND ($2::text IS NULL OR topic=$2)",
        viewer,
        topic
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Only locally controlled public collections have authoritative current
/// collection metadata here. Remote collection/stamp refresh is a later adapter.
pub async fn topic_collections(
    pool: &PgPool,
    viewer: i64,
    topic: &str,
    actor: &str,
) -> Result<Vec<i64>, DbError> {
    collection_candidates(pool, viewer, topic, actor, None).await
}

pub async fn collection_available(pool: &PgPool, viewer: i64, id: i64) -> Result<bool, DbError> {
    Ok(!collection_candidates(pool, viewer, "", "", Some(id))
        .await?
        .is_empty())
}

async fn collection_candidates(
    pool: &PgPool,
    viewer: i64,
    topic: &str,
    actor: &str,
    id: Option<i64>,
) -> Result<Vec<i64>, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT c.id FROM collections c JOIN accounts a ON a.id=c.account_id
        LEFT JOIN tags t ON t.id=c.tag_id
        WHERE ($4::bigint IS NULL OR c.id=$4) AND c.local AND c.discoverable AND a.discoverable AND a.suspended_at IS NULL
        AND NOT account_hidden($1,a.id)
        AND NOT EXISTS(SELECT 1 FROM follow_recommendation_mutes m WHERE m.account_id=$1 AND m.target_account_id=a.id)
        AND ($2='' OR lower(t.name)=$2)
        AND EXISTS(SELECT 1 FROM collection_items i JOIN accounts member ON member.id=i.account_id
            WHERE i.collection_id=c.id AND i.state='accepted' AND ($3='' OR member.uri=$3)
            AND (member.domain IS NULL OR i.approval_uri IS NOT NULL))
        ORDER BY c.id DESC LIMIT 100"#,viewer,topic,actor,id).fetch_all(pool).await?)
}

/// An authenticated actor Update invalidates earlier tag claims immediately,
/// including an in-flight first fetch. Positive tags await canonical refresh.
pub async fn forget_actor_topics(pool: &PgPool, uri: &str) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    super::lock_settings(&mut tx).await?;
    super::videos::invalidate_video_actor_locked(&mut tx, uri, false, "actor_updated").await?;
    sqlx::query!("UPDATE discovery_entries SET revision=revision+1 WHERE canonical_uri=$1 AND kind='profile'",uri).execute(&mut *tx).await?;
    sqlx::query!("DELETE FROM discovery_jobs WHERE target_uri=$1 AND NOT withdrawn AND operation IN ('refresh_entry','revalidate_entry')",uri).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
