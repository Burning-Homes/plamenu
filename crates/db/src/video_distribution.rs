//! Explicit distribution offers; never additional content authority.
use crate::{DbError, PgExecutor};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Policy {
    pub media_id: i64,
    pub download_enabled: bool,
    pub embed_enabled: bool,
    pub mirror_allowed: bool,
}
impl Policy {
    #[must_use]
    pub const fn default_for(media_id: i64) -> Self {
        Self {
            media_id,
            download_enabled: true,
            embed_enabled: true,
            mirror_allowed: false,
        }
    }
}
pub async fn for_media<'e, E: PgExecutor<'e>>(
    executor: E,
    ids: &[i64],
) -> Result<HashMap<i64, Policy>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(
        sqlx::query_as::<_, Policy>("SELECT * FROM video_distribution WHERE media_id=ANY($1)")
            .bind(ids)
            .fetch_all(executor)
            .await?
            .into_iter()
            .map(|p| (p.media_id, p))
            .collect(),
    )
}
pub async fn upsert<'e, E: PgExecutor<'e>>(executor: E, policy: Policy) -> Result<(), DbError> {
    sqlx::query("INSERT INTO video_distribution(media_id,download_enabled,embed_enabled,mirror_allowed) VALUES($1,$2,$3,$4) ON CONFLICT(media_id) DO UPDATE SET download_enabled=EXCLUDED.download_enabled,embed_enabled=EXCLUDED.embed_enabled,mirror_allowed=EXCLUDED.mirror_allowed")
        .bind(policy.media_id).bind(policy.download_enabled).bind(policy.embed_enabled).bind(policy.mirror_allowed).execute(executor).await?;
    Ok(())
}
pub async fn embed_allowed<'e, E: PgExecutor<'e>>(
    executor: E,
    status: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT NOT EXISTS(
            SELECT 1 FROM video_publications v
            JOIN media_attachments m ON m.id=v.media_id
            JOIN accounts a ON a.id=m.account_id
            JOIN accounts c ON c.id=v.channel_id
            JOIN video_channels vc ON vc.account_id=v.channel_id
            JOIN accounts o ON o.id=vc.owner_account_id
            LEFT JOIN video_distribution p ON p.media_id=v.media_id
            WHERE v.status_id=$1 AND (NOT COALESCE(p.embed_enabled,true)
                OR a.suspended_at IS NOT NULL OR c.suspended_at IS NOT NULL
                OR o.suspended_at IS NOT NULL
                OR m.status_id IS DISTINCT FROM v.status_id))",
    )
    .bind(status)
    .fetch_one(executor)
    .await?)
}

#[derive(sqlx::FromRow)]
pub struct Offer {
    pub media_id: i64,
    pub status_id: i64,
    pub download_enabled: bool,
    pub embed_enabled: bool,
}
pub async fn offers<'e, E: PgExecutor<'e>>(
    executor: E,
    ids: &[i64],
) -> Result<HashMap<i64, Offer>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(sqlx::query_as::<_,Offer>("SELECT v.media_id,v.status_id,COALESCE(p.download_enabled,true) AS download_enabled,COALESCE(p.embed_enabled,true) AS embed_enabled FROM video_publications v LEFT JOIN video_distribution p ON p.media_id=v.media_id JOIN statuses s ON s.id=v.status_id WHERE v.media_id=ANY($1) AND s.deleted_at IS NULL AND s.visibility IN ('public','unlisted')")
        .bind(ids).fetch_all(executor).await?.into_iter().map(|p|(p.media_id,p)).collect())
}
