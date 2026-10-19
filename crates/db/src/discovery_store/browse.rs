//! Read-only, stable catalogue positions and viewer-specific exclusions.
use super::{Entry, PgPool};
use crate::DbError;

pub type ResolvedAvatar = (i64, String, Option<String>, Option<String>);

/// Reuse identities explicitly imported through ordinary resolution. Reading
/// discovery cards must not import accounts or acquire remote metadata.
pub async fn resolved_avatars(
    pool: &PgPool,
    entries: &[i64],
    actor_uris: &[String],
    viewer: i64,
) -> Result<Vec<ResolvedAvatar>, DbError> {
    Ok(sqlx::query_as::<_, ResolvedAvatar>(
        "SELECT id,uri,avatar_file_name,avatar_remote_url FROM accounts
         WHERE (uri=ANY($2) OR uri IN (
             SELECT actor_uri FROM discovery_entries WHERE id=ANY($1)
             UNION SELECT channel_uri FROM discovery_video_details WHERE entry_id=ANY($1)
         )) AND suspended_at IS NULL AND silenced_at IS NULL
         AND NOT account_hidden($3,id)",
    )
    .bind(entries)
    .bind(actor_uris)
    .bind(viewer)
    .fetch_all(pool)
    .await?)
}

#[derive(Debug)]
pub struct BrowsePosition {
    pub id: i64,
    pub canonical_uri: String,
    pub actor_uri: Option<String>,
    pub origin: String,
    pub ordinal: i64,
    pub source_order: i64,
    pub creator_ordinal: i64,
}

pub async fn browse_watermark(pool: &PgPool) -> Result<i64, DbError> {
    Ok(
        sqlx::query_scalar!(r#"SELECT COALESCE(max(id),0) AS "id!" FROM discovery_entries"#)
            .fetch_one(pool)
            .await?,
    )
}

/// Rank all retained identities before filtering so a refusal, dismissal or
/// new arrival cannot move another identity across the cursor. New arrivals
/// belong to a new browsing journey. Callers must still check live eligibility.
pub async fn browse_positions(
    pool: &PgPool,
    viewer: i64,
    watermark: i64,
    ordinal: i64,
    after: i64,
    source: Option<i64>,
    rotation: (i64, i64),
) -> Result<Vec<BrowsePosition>, DbError> {
    catalogue_positions(
        pool,
        viewer,
        watermark,
        ordinal,
        after,
        BrowseFilter {
            source,
            kind: "profile",
            term: "",
            ..BrowseFilter::default()
        },
        (rotation.0, rotation.1, 0),
    )
    .await
}

#[derive(Default)]
pub struct BrowseFilter<'a> {
    pub source: Option<i64>,
    pub kind: &'a str,
    pub term: &'a str,
    pub topic: &'a str,
    pub actor: &'a str,
}

pub async fn catalogue_positions(
    pool: &PgPool,
    viewer: i64,
    watermark: i64,
    ordinal: i64,
    after: i64,
    filter: BrowseFilter<'_>,
    rotation: (i64, i64, i64),
) -> Result<Vec<BrowsePosition>, DbError> {
    Ok(sqlx::query_as!(
        BrowsePosition,
        r#"
        WITH ranked AS (
            SELECT e.*,s.origin,
                e.browse_ordinal AS ordinal,
                CASE WHEN $6::bigint=0 THEN 0 ELSE hashtextextended(s.origin,$6) END AS source_order
            FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
            WHERE (e.kind=$8 OR ($8='all' AND e.kind IN ('profile','post','community','video'))) AND e.id<=$2 ORDER BY e.id DESC LIMIT 2000
        )
        SELECT e.id,e.canonical_uri,e.actor_uri,e.origin,e.ordinal AS "ordinal!",e.source_order AS "source_order!",
            CASE WHEN $8 IN ('post','video') THEN e.creator_ordinal ELSE 0 END AS "creator_ordinal!"
        FROM ranked e LEFT JOIN accounts a ON a.uri=e.actor_uri
        WHERE (CASE WHEN $8 IN ('post','video') THEN e.creator_ordinal ELSE 0 END,e.ordinal,e.source_order,e.id)>($12,$3,$7,$4) AND ($5::bigint IS NULL OR e.source_id=$5)
            AND e.state='active' AND e.valid_until>clock_timestamp()
            AND ($9='' OR to_tsvector('simple',coalesce(e.title,'') || ' ' || coalesce(e.summary,'')) @@ plainto_tsquery('simple',$9))
            AND ($10='' OR EXISTS(SELECT 1 FROM discovery_topics t WHERE t.entry_id=e.id AND t.topic=$10))
            AND ($11='' OR e.actor_uri=$11)
            AND (a.id IS NULL OR (a.id<>$1 AND NOT account_hidden($1,a.id)
                AND NOT EXISTS(SELECT 1 FROM follow_recommendation_mutes m
                    WHERE m.account_id=$1 AND m.target_account_id=a.id)))
            AND NOT EXISTS(SELECT 1 FROM discovery_dismissals d
                WHERE d.account_id=$1 AND d.entry_id=e.id)
        ORDER BY CASE WHEN $8 IN ('post','video') THEN e.creator_ordinal ELSE 0 END,e.ordinal,e.source_order,e.id LIMIT 200
        "#,
        viewer,
        watermark,
        ordinal,
        after,
        filter.source, rotation.0, rotation.1,filter.kind,filter.term,filter.topic,filter.actor,rotation.2
    )
    .fetch_all(pool)
    .await?)
}

pub async fn entry_by_id(pool: &PgPool, id: i64) -> Result<Option<Entry>, DbError> {
    Ok(
        sqlx::query_as!(Entry, "SELECT * FROM discovery_entries WHERE id=$1", id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Match by canonical URI on each request: an account may have been imported
/// after this catalogue entry was verified, leaving its cached `account_id` empty.
pub async fn viewer_hides(pool: &PgPool, viewer: i64, id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(
        SELECT 1 FROM discovery_dismissals WHERE account_id=$1 AND entry_id=$2
        UNION ALL
        SELECT 1 FROM discovery_entries e JOIN accounts a ON a.uri=e.actor_uri
        WHERE e.id=$2 AND (a.id=$1 OR account_hidden($1,a.id) OR EXISTS(
            SELECT 1 FROM follow_recommendation_mutes m
            WHERE m.account_id=$1 AND m.target_account_id=a.id))
        ) AS "hidden!""#,
        viewer,
        id
    )
    .fetch_one(pool)
    .await?)
}

pub async fn dismiss(pool: &PgPool, viewer: i64, id: i64) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    super::lock_settings(&mut tx).await?;
    sqlx::query!(
        "INSERT INTO discovery_dismissals(account_id,entry_id)
        SELECT $1,id FROM discovery_entries WHERE id=$2 AND kind IN ('profile','post','community','video')
        ON CONFLICT DO NOTHING",
        viewer,
        id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Bounded source continuation for member browsing.
pub async fn browse_sources(pool: &PgPool, after: i64) -> Result<Vec<super::Source>, DbError> {
    Ok(sqlx::query_as!(
        super::Source,
        "SELECT * FROM discovery_sources WHERE id>$1 ORDER BY id LIMIT 200",
        after
    )
    .fetch_all(pool)
    .await?)
}
