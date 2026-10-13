//! Structured metadata for native and federated Audio publications.
use crate::DbError;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgExecutor};
use std::collections::HashMap;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
#[serde(default)]
pub struct Metadata {
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub release_year: Option<i32>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub musicbrainz_id: Option<String>,
}

#[derive(FromRow)]
struct Row {
    status_id: i64,
    #[sqlx(flatten)]
    metadata: Metadata,
}

pub async fn store<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
    metadata: &Metadata,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO status_music(status_id,artist,album,genre,release_year,track_number,disc_number,musicbrainz_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(status_id) DO UPDATE SET artist=EXCLUDED.artist,album=EXCLUDED.album,genre=EXCLUDED.genre,release_year=EXCLUDED.release_year,track_number=EXCLUDED.track_number,disc_number=EXCLUDED.disc_number,musicbrainz_id=EXCLUDED.musicbrainz_id")
        .bind(status_id).bind(&metadata.artist).bind(&metadata.album).bind(&metadata.genre)
        .bind(metadata.release_year).bind(metadata.track_number).bind(metadata.disc_number).bind(&metadata.musicbrainz_id)
        .execute(executor).await?;
    Ok(())
}

pub async fn for_statuses<'e, E: PgExecutor<'e>>(
    executor: E,
    ids: &[i64],
) -> Result<HashMap<i64, Metadata>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Row> = sqlx::query_as("SELECT * FROM status_music WHERE status_id=ANY($1)")
        .bind(ids)
        .fetch_all(executor)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.status_id, row.metadata))
        .collect())
}

/// Account purge retains storage debt until the cleanup worker acknowledges
/// deletion. Call inside the account lifecycle transaction before FK cascades.
pub async fn retire_account_assets(
    conn: &mut sqlx::PgConnection,
    owner: i64,
) -> Result<(), DbError> {
    lock_account_owner(conn, owner).await?;
    // The account row and music-owner fence are already held. A fresh
    // statement now proves there is no Music state to retire; writers cannot
    // acquire this owner fence (or an account FK lock) until we commit.
    if !account_has_state(conn, owner).await? {
        return Ok(());
    }
    for query in [
        "DELETE FROM music_queues WHERE account_id=$1",
        "DELETE FROM music_listen_sessions WHERE account_id=$1",
        "DELETE FROM music_listening_preferences WHERE account_id=$1",
        "DELETE FROM music_favourites WHERE account_id=$1",
        "DELETE FROM music_saved_resources WHERE account_id=$1",
        "DELETE FROM music_saved_remote WHERE account_id=$1",
        "DELETE FROM music_share_links WHERE owner_id=$1",
        "DELETE FROM music_podcast_subscriptions WHERE account_id=$1",
        "DELETE FROM music_podcast_plays WHERE account_id=$1",
        "DELETE FROM music_follows WHERE owner_id=$1 OR follower_id=$1",
        "DELETE FROM music_asset_grants WHERE account_id=$1",
        "DELETE FROM music_album_grants WHERE account_id=$1",
        "DELETE FROM music_collection_grants WHERE account_id=$1",
    ] {
        sqlx::query(query).bind(owner).execute(&mut *conn).await?;
    }
    sqlx::query("DELETE FROM music_remote_track_proofs WHERE controller_id=$1")
        .bind(owner)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp(),tombstoned_at=clock_timestamp(),changed_at=clock_timestamp(),name='',summary='',details=ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details,audio_url=NULL,next_page_uri=NULL WHERE controller_id=$1").bind(owner).execute(&mut *conn).await?;
    sqlx::query("DELETE FROM music_radios WHERE owner_id=$1")
        .bind(owner)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM music_radio_grants WHERE account_id=$1")
        .bind(owner)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM music_artworks WHERE owner_id=$1")
        .bind(owner)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM music_collections WHERE owner_id=$1")
        .bind(owner)
        .execute(&mut *conn)
        .await?;
    let keys:Vec<(String,String,i64,i64)>=sqlx::query_as("DELETE FROM music_assets WHERE owner_id=$1 RETURNING original_file,stream_file,CASE WHEN indexed_original THEN 0 ELSE original_bytes END,stream_bytes").bind(owner).fetch_all(&mut *conn).await?;
    let mut files = Vec::new();
    let mut sizes = Vec::new();
    for (original, stream, original_bytes, stream_bytes) in keys {
        for (key, bytes) in [(original, original_bytes), (stream, stream_bytes)] {
            if bytes > 0 {
                files.push(key);
                sizes.push(bytes);
            }
        }
    }
    crate::media_cleanup::enqueue_many(&mut *conn, &files).await?;
    sqlx::query("INSERT INTO music_storage_debt(file_name,owner_id,size_bytes) SELECT key,$1,size FROM unnest($2::text[],$3::bigint[]) AS input(key,size)").bind(owner).bind(&files).bind(&sizes).execute(&mut *conn).await?;
    // Composite ownership FKs guarantee foreign assets cannot depend on
    // these metadata rows. Empty albums and artists also contain user data.
    for query in [
        "DELETE FROM music_albums WHERE owner_id=$1",
        "DELETE FROM music_artists WHERE owner_id=$1",
    ] {
        sqlx::query(query).bind(owner).execute(&mut *conn).await?;
    }
    Ok(())
}

async fn account_has_state(conn: &mut sqlx::PgConnection, owner: i64) -> Result<bool, DbError> {
    let has_music: bool = sqlx::query_scalar(
        r"
        SELECT EXISTS (
            SELECT 1 FROM music_queues WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_listen_sessions WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_listening_preferences WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_favourites WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_saved_resources WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_saved_remote WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_share_links WHERE owner_id=$1
            UNION ALL
            SELECT 1 FROM music_podcast_subscriptions WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_podcast_plays WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_follows WHERE owner_id=$1 OR follower_id=$1
            UNION ALL
            SELECT 1 FROM music_asset_grants WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_album_grants WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_collection_grants WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_remote_track_proofs WHERE controller_id=$1
            UNION ALL
            SELECT 1 FROM music_radios WHERE owner_id=$1
            UNION ALL
            SELECT 1 FROM music_radio_grants WHERE account_id=$1
            UNION ALL
            SELECT 1 FROM music_artworks WHERE owner_id=$1
            UNION ALL
            SELECT 1 FROM music_collections WHERE owner_id=$1
            UNION ALL
            SELECT 1 FROM music_assets WHERE owner_id=$1
            UNION ALL
            SELECT 1 FROM music_albums WHERE owner_id=$1
            UNION ALL
            SELECT 1 FROM music_artists WHERE owner_id=$1
            UNION ALL
            SELECT 1 FROM music_remote_resources WHERE controller_id=$1
        )
        ",
    )
    .bind(owner)
    .fetch_one(&mut *conn)
    .await?;
    Ok(has_music)
}

/// Capture managed bytes before a hard DELETE on this same transaction.
/// Account FKs cascade metadata even when there are no Music files; tombstone
/// purges still use the full explicit retirement path above.
pub(crate) async fn keys_before_account_delete(
    conn: &mut sqlx::PgConnection,
    owner: i64,
) -> Result<Vec<String>, DbError> {
    lock_account_owner(conn, owner).await?;
    keys_after_account_lock(conn, owner).await
}

/// Resolve and lock a remote actor before taking a fresh file-key snapshot.
/// The caller hard-deletes the returned account on this same transaction.
pub(crate) async fn keys_before_account_delete_by_uri(
    conn: &mut sqlx::PgConnection,
    uri: &str,
) -> Result<Option<(i64, Vec<String>)>, DbError> {
    let owner: Option<i64> = sqlx::query_scalar(
        r"WITH owner AS MATERIALIZED (
            SELECT id FROM accounts WHERE uri=$1 FOR UPDATE
        ), fenced AS MATERIALIZED (
            SELECT id,pg_advisory_xact_lock(hashtextextended('music-owner:' || id::text,0))
            FROM owner
        ) SELECT id FROM fenced",
    )
    .bind(uri)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(owner) = owner else { return Ok(None) };
    Ok(Some((owner, keys_after_account_lock(conn, owner).await?)))
}

async fn keys_after_account_lock(
    conn: &mut sqlx::PgConnection,
    owner: i64,
) -> Result<Vec<String>, DbError> {
    let keys = crate::media_cleanup::collect_account_keys(conn, owner).await?;
    // Music files occupy their own reserved namespace. A conservative match
    // also keeps explicit retirement for any Music reference in an attachment.
    if keys.iter().any(|key| key.starts_with("music")) {
        retire_account_assets(conn, owner).await?;
    }
    Ok(keys)
}

async fn lock_account_owner(conn: &mut sqlx::PgConnection, owner: i64) -> Result<(), DbError> {
    // Materialization makes the account row lock precede the owner fence,
    // matching mutation lock order while acquiring both in one round trip.
    sqlx::query(
        r"WITH owner AS MATERIALIZED (
            SELECT id FROM accounts WHERE id=$1 FOR UPDATE
        ) SELECT pg_advisory_xact_lock(hashtextextended('music-owner:' || id::text,0))
          FROM owner",
    )
    .bind(owner)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
