//! Per-kind native delivery preferences. Missing rows preserve existing inbox
//! behavior; email channels require explicit opt-in.
use crate::DbError;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
pub const TYPES: &[&str] = &[
    "mention",
    "status",
    "reblog",
    "follow",
    "follow_request",
    "favourite",
    "music.favourite",
    "music.share",
    "music.download",
    "webmention",
    "poll",
    "update",
    "severed_relationships",
    "moderation_warning",
    "annual_report",
    "admin.sign_up",
    "admin.report",
    "quote",
    "quoted_update",
    "live",
    "added_to_collection",
    "collection_update",
    // Plamenu also emits Pleroma's reaction kind (P1); listing it makes the
    // type filters and the push-alert whitelist accept it, like Pleroma does.
    "pleroma:emoji_reaction",
    // Event participation (E-track). Mastodon has no event concept, so these are
    // Plamenu extensions; listing them is what makes `types[]` filtering and the
    // push-alert whitelist accept them at all.
    "event.participation",
    "event.accepted",
    "event.rejected",
    "event.changed",
    "event.invite",
];
#[derive(Clone, Debug, Deserialize, Serialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct Preference {
    pub kind: String,
    pub in_app: bool,
    pub email: bool,
    pub email_digest: bool,
}
pub async fn preferences(pool: &PgPool, account: i64) -> Result<Vec<Preference>, DbError> {
    sqlx::query_as("SELECT k.kind,COALESCE(p.in_app,true) AS in_app,COALESCE(p.email,false) AS email,COALESCE(p.email_digest,false) AS email_digest FROM unnest($2::text[]) AS k(kind) LEFT JOIN notification_delivery_preferences p ON p.account_id=$1 AND p.kind=k.kind")
       .bind(account).bind(TYPES).fetch_all(pool).await.map_err(DbError::from)
}
/// Validated by the HTTP/native caller before any row is changed.
pub async fn update(
    pool: &PgPool,
    account: i64,
    preferences: &[Preference],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    for p in preferences {
        sqlx::query("INSERT INTO notification_delivery_preferences(account_id,kind,in_app,email,email_digest) VALUES($1,$2,$3,$4,$5) ON CONFLICT(account_id,kind) DO UPDATE SET in_app=EXCLUDED.in_app,email_since=CASE WHEN NOT notification_delivery_preferences.email AND EXCLUDED.email THEN now() ELSE notification_delivery_preferences.email_since END,email=EXCLUDED.email,email_digest=EXCLUDED.email_digest,updated_at=now()")
        .bind(account).bind(&p.kind).bind(p.in_app).bind(p.email).bind(p.email_digest).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
/// Coalesced reasons choose the first enabled reason through the existing SQL
/// selector, so disabling mentions never hides an enabled quote/status reason.
pub async fn in_app_kinds(
    pool: &PgPool,
    account: i64,
    allowed: Option<&[String]>,
) -> Result<Option<Vec<String>>, DbError> {
    let disabled: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM notification_delivery_preferences WHERE account_id=$1 AND NOT in_app",
    )
    .bind(account)
    .fetch_all(pool)
    .await?;
    if disabled.is_empty() {
        return Ok(allowed.map(<[String]>::to_vec));
    }
    Ok(Some(
        allowed
            .map_or_else(
                || TYPES.iter().map(|s| (*s).to_owned()).collect(),
                <[String]>::to_vec,
            )
            .into_iter()
            .filter(|s| !disabled.contains(s))
            .collect(),
    ))
}

pub async fn email_kinds(
    pool: &PgPool,
    account: i64,
    allowed: Option<&[String]>,
) -> Result<Vec<String>, DbError> {
    let kinds:Vec<String>=sqlx::query_scalar("SELECT kind FROM notification_delivery_preferences WHERE account_id=$1 AND (email OR email_digest)").bind(account).fetch_all(pool).await?;
    Ok(kinds
        .into_iter()
        .filter(|s| allowed.is_none_or(|a| a.contains(s)))
        .collect())
}
