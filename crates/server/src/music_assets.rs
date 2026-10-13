//! Durable, independently authorized audio. Internal keys are never bearer URLs.
use crate::{AppState, error::ApiError};
use plamenu_db::{DbError, id, media_cleanup, music::Metadata};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;

// Authorization is included before paging in every query. Only literals may
// enter this macro; user data is bound, never interpolated into SQL.
macro_rules! asset_query {
    ($condition:literal, $tail:literal) => { concat!("SELECT t.*,r.name AS artist,COALESCE(a.name,'') AS album,a.artist_id AS album_artist_id,art.id AS artwork_id,art.content_type AS artwork_content_type,art.width AS artwork_width,art.height AS artwork_height FROM music_assets t JOIN music_artists r ON r.id=t.artist_id LEFT JOIN music_albums a ON a.id=t.album_id LEFT JOIN music_artists ar ON ar.id=a.artist_id LEFT JOIN LATERAL (SELECT i.id,i.content_type,i.width,i.height FROM music_artworks i WHERE ((i.target_kind='assets' AND i.target_id=t.id) OR (i.target_kind='albums' AND i.target_id=a.id) OR (i.target_kind='artists' AND i.target_id=r.id)) AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=i.file_name) ORDER BY CASE i.target_kind WHEN 'assets' THEN 0 WHEN 'albums' THEN 1 ELSE 2 END,CASE WHEN (i.target_kind='albums' AND i.slot='cover') OR (i.target_kind<>'albums' AND i.slot='image') THEN 0 ELSE 1 END LIMIT 1) art ON true", $condition, "AND EXISTS (SELECT 1 FROM accounts o WHERE o.id=t.owner_id AND o.suspended_at IS NULL AND o.deleted_at IS NULL AND NOT EXISTS (SELECT 1 FROM users u WHERE u.account_id=o.id AND (NOT u.approved OR u.confirmed_at IS NULL))) AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name IN(t.original_file,t.stream_file)) AND (t.owner_id=$2 OR t.visibility='public' OR (t.visibility='local' AND EXISTS(SELECT 1 FROM accounts v WHERE v.id=$2 AND v.domain IS NULL)) OR EXISTS(SELECT 1 FROM music_asset_grants g WHERE g.asset_id=t.id AND g.account_id=$2) OR (a.owner_id=t.owner_id AND a.share_tracks AND (a.visibility='public' OR (a.visibility='local' AND EXISTS(SELECT 1 FROM accounts album_viewer WHERE album_viewer.id=$2 AND album_viewer.domain IS NULL)) OR EXISTS(SELECT 1 FROM music_album_grants album_grant WHERE album_grant.album_id=a.id AND album_grant.account_id=$2))) OR EXISTS(SELECT 1 FROM music_collection_entries shared_entry JOIN music_collections shared_parent ON shared_parent.id=shared_entry.collection_id WHERE shared_entry.asset_id=t.id AND shared_parent.owner_id=t.owner_id AND shared_parent.share_tracks AND (shared_parent.visibility='public' OR (shared_parent.visibility='local' AND EXISTS(SELECT 1 FROM accounts shared_viewer WHERE shared_viewer.id=$2 AND shared_viewer.domain IS NULL)) OR EXISTS(SELECT 1 FROM music_collection_grants shared_grant WHERE shared_grant.collection_id=shared_parent.id AND shared_grant.account_id=$2)))) AND NOT EXISTS (SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=t.owner_id) OR (b.account_id=t.owner_id AND b.target_account_id=$2))", $tail) };
}

pub(crate) use asset_query;

#[derive(Debug, Clone, FromRow)]
pub struct Asset {
    pub id: i64,
    pub owner_id: i64,
    pub title: String,
    pub description: String,
    pub description_type: String,
    pub tags: Vec<String>,
    pub extra_artists: Vec<String>,
    pub artist_id: i64,
    pub album_id: Option<i64>,
    pub album_artist_id: Option<i64>,
    pub artwork_id: Option<i64>,
    pub artwork_content_type: Option<String>,
    pub artwork_width: Option<i32>,
    pub artwork_height: Option<i32>,
    pub artist: String,
    pub album: String,
    pub release_year: Option<i32>,
    pub genre: String,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub musicbrainz_id: Option<String>,
    pub visibility: String,
    pub original_file: String,
    pub indexed_original: bool,
    pub indexed_tags_revision: Option<i64>,
    pub stream_file: String,
    pub original_bytes: i64,
    pub original_format: String,
    pub stream_bytes: i64,
    pub audio_hash: String,
    pub duration: f64,
    pub revision: i64,
}
impl Asset {
    #[must_use]
    pub fn entity(&self) -> Value {
        json!({"id":self.id.to_string(),"owner_id":self.owner_id.to_string(),"title":self.title,"image":self.artwork_id.map(|id|json!({"id":id.to_string(),"url":format!("/music/artwork/{id}"),"content_type":self.artwork_content_type,"width":self.artwork_width,"height":self.artwork_height})),"description":self.description,"description_type":self.description_type,"tags":self.tags,"extra_artists":self.extra_artists,"artist_id":self.artist_id.to_string(),"album_id":self.album_id.map(|id|id.to_string()),"music":self.metadata(),"visibility":self.visibility,"duration":self.duration,"revision":self.revision.to_string(),"stream_url":format!("/music/assets/{}/stream",self.id),"download_url":format!("/music/assets/{}/download",self.id)})
    }
    #[must_use]
    pub fn metadata(&self) -> Metadata {
        Metadata {
            artist: self.artist.clone(),
            album: self.album.clone(),
            genre: self.genre.clone(),
            release_year: self.release_year,
            track_number: self.track_number,
            disc_number: self.disc_number,
            musicbrainz_id: self.musicbrainz_id.clone(),
        }
    }
}

pub async fn get(state: &AppState, asset: i64, viewer: Option<i64>) -> Result<Asset, ApiError> {
    sqlx::query_as(asset_query!(" WHERE t.id=$1 ", ""))
        .bind(asset)
        .bind(viewer)
        .fetch_optional(&state.pool)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)
}

/// Internal mutation reads reuse the held asset/account fence.
pub(crate) async fn owned_conn(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    asset: i64,
) -> Result<Asset, ApiError> {
    sqlx::query_as(asset_query!(" WHERE t.id=$1 AND t.owner_id=$2 ", ""))
        .bind(asset)
        .bind(owner)
        .fetch_optional(conn)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)
}

pub async fn catalogue(
    state: &AppState,
    viewer: Option<i64>,
    mine: bool,
    q: &str,
    before: Option<i64>,
) -> Result<Vec<Asset>, ApiError> {
    if q.chars().count() > 200 || q.contains('\0') {
        return Err(ApiError::Unprocessable("Invalid music search".into()));
    }
    if mine && viewer.is_none() {
        return Err(ApiError::NotFound);
    }
    Ok(sqlx::query_as(asset_query!(" WHERE ($1::bigint IS NULL OR t.id<$1) ", " AND (NOT $3 OR t.owner_id=$2) AND ($3 OR (t.visibility='public' AND EXISTS(SELECT 1 FROM accounts author WHERE author.id=t.owner_id AND author.silenced_at IS NULL))) AND NOT EXISTS(SELECT 1 FROM mutes mute WHERE mute.account_id=$2 AND mute.target_account_id=t.owner_id AND (mute.expires_at IS NULL OR mute.expires_at>now())) AND (strpos(lower(t.title),lower($4))>0 OR strpos(lower(r.name),lower($4))>0 OR strpos(lower(COALESCE(a.name,'')),lower($4))>0 OR strpos(lower(t.genre),lower($4))>0 OR strpos(lower(t.description),lower($4))>0 OR strpos(lower(COALESCE(ar.name,'')),lower($4))>0 OR EXISTS(SELECT 1 FROM unnest(t.tags || t.extra_artists || r.tags || COALESCE(a.tags,'{}'::text[]) || COALESCE(a.genres,'{}'::text[]) || COALESCE(ar.tags,'{}'::text[])) term WHERE strpos(lower(term),lower($4))>0)) ORDER BY t.id DESC LIMIT 61"))
        .bind(before).bind(viewer).bind(mine).bind(q).fetch_all(&state.pool).await.map_err(DbError::from)?)
}

/// One authorized source batch for playlist/player/export services.
pub async fn for_ids(
    state: &AppState,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Asset>, ApiError> {
    for_ids_pool(&state.pool, ids, viewer).await
}
pub(crate) async fn for_ids_pool(
    pool: &sqlx::PgPool,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Asset>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > 100 {
        return Err(ApiError::Unprocessable(
            "Music source batch is too large".into(),
        ));
    }
    Ok(sqlx::query_as(asset_query!(" WHERE t.id=ANY($1) ", ""))
        .bind(ids)
        .bind(viewer)
        .fetch_all(pool)
        .await
        .map_err(DbError::from)?)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Edit {
    pub title: String,
    pub visibility: String,
    #[serde(default)]
    pub music: Metadata,
    pub revision: Option<String>,
}
fn validate(input: &Edit) -> Result<(), ApiError> {
    if input.title.trim().is_empty()
        || input.title.chars().count() > 200
        || input.title.contains('\0')
        || !["public", "local", "private"].contains(&input.visibility.as_str())
    {
        return Err(ApiError::Unprocessable(
            "Invalid track title or visibility".into(),
        ));
    }
    crate::audio::validate_metadata(&input.music)
}
pub(crate) async fn entities(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    m: &Metadata,
) -> Result<(i64, Option<i64>), DbError> {
    let artist = if m.artist.trim().is_empty() {
        "Unknown artist"
    } else {
        m.artist.trim()
    };
    let artist_id:i64=sqlx::query_scalar("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,$3) ON CONFLICT(owner_id,name) DO UPDATE SET name=EXCLUDED.name RETURNING id")
        .bind(id::next()).bind(owner).bind(artist).fetch_one(&mut *conn).await?;
    let album_id = if m.album.trim().is_empty() {
        None
    } else {
        Some(sqlx::query_scalar::<_,i64>("INSERT INTO music_albums(id,owner_id,artist_id,name,release_year) VALUES($1,$2,$3,$4,$5) ON CONFLICT(owner_id,artist_id,name,release_year) DO UPDATE SET name=EXCLUDED.name RETURNING id")
        .bind(id::next()).bind(owner).bind(artist_id).bind(m.album.trim()).bind(m.release_year).fetch_one(&mut *conn).await?)
    };
    Ok((artist_id, album_id))
}

pub async fn update(
    state: &AppState,
    owner: i64,
    asset: i64,
    input: &Edit,
) -> Result<Asset, ApiError> {
    validate(input)?;
    let revision = input
        .revision
        .as_deref()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::Unprocessable("Track revision is required".into()))?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock_owner(&mut tx, owner).await?;
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT revision FROM music_assets WHERE id=$1 AND owner_id=$2 FOR UPDATE",
    )
    .bind(asset)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?;
    let found = found.ok_or(ApiError::NotFound)?;
    if revision != found {
        return Err(ApiError::Conflict(
            "Track changed; reload before editing".into(),
        ));
    }
    let previous = owned_conn(&mut tx, owner, asset).await?;
    let deleted = if input.visibility == "public" {
        Vec::new()
    } else {
        crate::music_publications::withdraw_conn(state, &mut tx, owner, asset).await?
    };
    let (artist, album) = edit_entities(&mut tx, owner, &previous, &input.music).await?;
    sqlx::query("UPDATE music_assets SET title=$3,visibility=$4,artist_id=$5,album_id=$6,genre=$7,track_number=$8,disc_number=$9,musicbrainz_id=$10,release_year=$11,indexed_tags_revision=CASE WHEN indexed_tags_revision=revision AND $12 THEN revision+1 ELSE indexed_tags_revision END,revision=revision+1 WHERE id=$1 AND owner_id=$2")
        .bind(asset).bind(owner).bind(input.title.trim()).bind(&input.visibility).bind(artist).bind(album).bind(&input.music.genre).bind(input.music.track_number).bind(input.music.disc_number).bind(&input.music.musicbrainz_id).bind(input.music.release_year).bind(previous.title==input.title.trim() && previous.metadata()==input.music).execute(&mut *tx).await.map_err(DbError::from)?;
    let updated = if input.visibility == "public"
        && (previous.title != input.title.trim() || previous.metadata() != input.music)
    {
        crate::music_publications::synchronize_conn(state, &mut tx, owner, asset).await?
    } else {
        None
    };
    if previous.visibility == "public" || input.visibility == "public" {
        let mut references = crate::music_entities::references(&previous);
        references.push((crate::music_entities::Kind::Artist, artist));
        if let Some(album) = album {
            references.push((crate::music_entities::Kind::Album, album));
        }
        crate::music_entities::reconcile_conn(state, &mut tx, owner, &references).await?;
    }
    tx.commit().await.map_err(DbError::from)?;
    crate::music_publications::publish_deletes(state, &deleted).await;
    crate::music_publications::publish_update(state, updated.as_ref()).await;
    get(state, asset, Some(owner)).await
}

pub async fn delete(state: &AppState, owner: i64, asset: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock_owner(&mut tx, owner).await?;
    let (events, references) = retire_conn(state, &mut tx, owner, asset).await?;
    crate::music_entities::reconcile_conn(state, &mut tx, owner, &references).await?;
    tx.commit().await.map_err(DbError::from)?;
    crate::music_publications::publish_deletes(state, &events).await;
    Ok(())
}
/// Retire retained bytes and publications under the caller's owner fence. Bulk
/// entity deletion reconciles resources once, after removing the whole batch.
pub(crate) async fn retire_conn(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    asset: i64,
) -> Result<
    (
        Vec<plamenu_db::streaming::Event>,
        Vec<(crate::music_entities::Kind, i64)>,
    ),
    ApiError,
> {
    let authorization: Option<i64> =
        sqlx::query_scalar("SELECT id FROM music_assets WHERE id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(asset)
            .bind(owner)
            .fetch_optional(&mut *conn)
            .await
            .map_err(DbError::from)?;
    authorization.ok_or(ApiError::NotFound)?;
    let previous = owned_conn(conn, owner, asset).await?;
    let deleted = crate::music_publications::withdraw_conn(state, conn, owner, asset).await?;
    let keys: Option<(String, String, i64, i64)> = sqlx::query_as(
        "DELETE FROM music_assets WHERE id=$1 AND owner_id=$2 RETURNING original_file,stream_file,CASE WHEN indexed_original THEN 0 ELSE original_bytes END,stream_bytes",
    )
    .bind(asset)
    .bind(owner)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let (a, b, original_bytes, stream_bytes) = keys.ok_or(ApiError::NotFound)?;
    let retained = [(a, original_bytes), (b, stream_bytes)]
        .into_iter()
        .filter(|(_, bytes)| *bytes > 0)
        .collect::<Vec<_>>();
    let keys = retained
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let sizes = retained.iter().map(|(_, size)| *size).collect::<Vec<_>>();
    media_cleanup::enqueue_many(&mut *conn, &keys).await?;
    sqlx::query("INSERT INTO music_storage_debt(file_name,owner_id,size_bytes) SELECT key,$1,size FROM unnest($2::text[],$3::bigint[]) AS input(key,size)")
        .bind(owner).bind(keys).bind(sizes).execute(&mut *conn).await.map_err(DbError::from)?;
    let references = if previous.visibility == "public" {
        crate::music_entities::references(&previous)
    } else {
        Vec::new()
    };
    Ok((deleted, references))
}

pub async fn grant(
    state: &AppState,
    owner: i64,
    asset: i64,
    member: i64,
    enabled: bool,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let own: Option<i64> =
        sqlx::query_scalar("SELECT id FROM music_assets WHERE id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(asset)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?;
    own.ok_or(ApiError::NotFound)?;
    if enabled {
        if !lock_recipient(&mut tx, member).await? {
            return Err(ApiError::NotFound);
        }
        let inserted:Option<i64>=sqlx::query_scalar("INSERT INTO music_asset_grants(asset_id,account_id) VALUES($1,$2) ON CONFLICT DO NOTHING RETURNING id").bind(asset).bind(member).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
        if let Some(grant) = inserted {
            crate::music_shares::notify_grant(
                state,
                &mut tx,
                owner,
                member,
                crate::music_saved::Kind::Track,
                grant,
            )
            .await?;
        }
    } else {
        sqlx::query("DELETE FROM music_asset_grants WHERE asset_id=$1 AND account_id=$2")
            .bind(asset)
            .bind(member)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
/// A key-share lock rechecks recipient eligibility after concurrent retirement,
/// and remains compatible with another music owner's no-key-update fence.
pub(crate) async fn lock_recipient(
    conn: &mut sqlx::PgConnection,
    member: i64,
) -> Result<bool, ApiError> {
    let eligible: Option<i64> = sqlx::query_scalar("SELECT a.id FROM accounts a WHERE a.id=$1 AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) FOR KEY SHARE OF a")
        .bind(member).fetch_optional(conn).await.map_err(DbError::from)?;
    Ok(eligible.is_some())
}

fn tag_metadata(tags: &std::collections::HashMap<String, String>) -> Metadata {
    let bounded = |key: &str| {
        tags.get(key)
            .map(|v| v.replace('\0', "").chars().take(200).collect::<String>())
            .unwrap_or_default()
    };
    let number = |key: &str| {
        tags.get(key)
            .and_then(|v| v.split('/').next())
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|v| (1..=9999).contains(v))
    };
    Metadata {
        artist: bounded("artist"),
        album: bounded("album"),
        genre: bounded("genre"),
        release_year: tags
            .get("date")
            .and_then(|v| v.split('-').next())
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|v| (1..=9999).contains(v))
            .or_else(|| number("year")),
        track_number: number("track"),
        disc_number: number("disc"),
        musicbrainz_id: [
            "musicbrainz_trackid",
            "musicbrainz_recordingid",
            "musicbrainztrackid",
            "musicbrainzrecordingid",
        ]
        .into_iter()
        .filter_map(|key| tags.get(key))
        .map(|v| v.trim())
        .find(|v| crate::audio::valid_mbid(v))
        .map(str::to_owned),
    }
}

pub async fn import(
    state: &AppState,
    owner: i64,
    bytes: Vec<u8>,
    filename: &str,
) -> Result<Asset, ApiError> {
    let limit = crate::media_limits::FileSizeLimit {
        launcher: crate::media_limits::running_executable()
            .map_err(|e| ApiError::Internal(Box::new(e)))?,
        bytes: u64::try_from(crate::media_processing::MAX_AV_UPLOAD_BYTES).unwrap_or(u64::MAX),
    };
    import_with_limit(state, owner, bytes, filename, limit).await
}

/// Explicit encoder launcher for embedders and integration tests. HTTP callers
/// always use the running service binary through `import`.
pub async fn import_with_limit(
    state: &AppState,
    owner: i64,
    bytes: Vec<u8>,
    filename: &str,
    limit: crate::media_limits::FileSizeLimit,
) -> Result<Asset, ApiError> {
    if bytes.is_empty() || bytes.len() > crate::media_processing::MAX_AV_UPLOAD_BYTES {
        return Err(ApiError::Unprocessable(
            "Audio upload exceeds the file limit".into(),
        ));
    }
    let dir = crate::media_transcode::scratch_dir(&state.config).await?;
    let source = dir.path().join("input");
    tokio::fs::write(&source, &bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let probe = crate::media_transcode::probe_music(&state.config, &source).await?;
    let ImportDescription {
        input,
        duration,
        format,
        properties,
        has_artwork,
    } = describe_import(probe, filename)?;
    let hash = fingerprint(state, &source, &properties).await?;
    let duplicate: Option<i64> =
        sqlx::query_scalar("SELECT id FROM music_assets WHERE owner_id=$1 AND audio_hash=$2")
            .bind(owner)
            .bind(&hash)
            .fetch_optional(&state.pool)
            .await
            .map_err(DbError::from)?;
    if let Some(duplicate) = duplicate {
        return get(state, duplicate, Some(owner)).await;
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let mut params = crate::media_transcode::TranscodeParams::from_settings(&settings);
    params.file_size_limit = Some(crate::media_limits::FileSizeLimit {
        bytes: limit
            .bytes
            .min(u64::try_from(params.max_av_bytes).unwrap_or(u64::MAX)),
        ..limit
    });
    let processed =
        crate::media_transcode::process_av_file(&state.config, &source, &params).await?;
    if processed.kind != "audio" {
        return Err(ApiError::Unprocessable("Upload an audio file".into()));
    }
    let picture = if has_artwork {
        match crate::music_artworks::embedded::extract(state, &source).await {
            Ok(bytes) if !bytes.is_empty() => Some(bytes),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(owner,error=%error,"Embedded artwork could not be read");
                None
            }
        }
    } else {
        None
    };
    let imported = persist(
        state,
        owner,
        Imported {
            input: &input,
            source: &source,
            processed: &processed,
            hash: &hash,
            duration,
            original_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            format,
        },
    )
    .await?;
    if let Some(picture) = picture {
        // Storage consumes the upload spool; artwork was extracted beforehand.
        if let Err(error) =
            crate::music_artworks::embedded::upload(state, owner, imported.id, picture).await
        {
            tracing::warn!(asset=imported.id,error=%error,"Embedded artwork could not be imported");
        }
    }
    get(state, imported.id, Some(owner)).await
}

pub(crate) struct ImportDescription {
    pub input: Edit,
    pub duration: f64,
    pub format: &'static str,
    pub properties: Vec<u8>,
    pub has_artwork: bool,
}

pub(crate) fn describe_import(
    probe: crate::media_transcode::Probe,
    filename: &str,
) -> Result<ImportDescription, ApiError> {
    if probe.audio_codec.is_none() || probe.video_codec.is_some() {
        return Err(ApiError::Unprocessable("Upload an audio file".into()));
    }
    let duration = probe
        .duration
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| ApiError::Unprocessable("Audio duration is unavailable".into()))?;
    let format = match probe.format_name.as_str() {
        "mp3" => "mp3",
        "flac" => "flac",
        "wav" => "wav",
        "aac" => "aac",
        "mov,mp4,m4a,3gp,3g2,mj2" => "m4a",
        "ogg" => "ogg",
        "matroska,webm" => "mka",
        "asf" => "wma",
        _ => {
            return Err(ApiError::Unprocessable(
                "Unsupported audio container".into(),
            ));
        }
    };
    let layout = probe
        .audio_layout
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(match probe.audio_channels {
            Some(1) => "mono",
            Some(2) => "stereo",
            _ => "unknown",
        });
    let properties = serde_json::to_vec(&(
        "pcm-f64le-v1",
        probe.audio_sample_rate,
        probe.audio_channels,
        layout,
    ))
    .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let has_artwork = probe.has_artwork;
    let tags = probe
        .tags
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect();
    let music = tag_metadata(&tags);
    let title = tags
        .get("title")
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(filename)
        .replace('\0', "")
        .chars()
        .take(200)
        .collect::<String>();
    let input = Edit {
        title,
        visibility: "private".into(),
        music,
        revision: None,
    };
    validate(&input)?;
    Ok(ImportDescription {
        input,
        duration,
        format,
        properties,
        has_artwork,
    })
}

struct Imported<'a> {
    input: &'a Edit,
    source: &'a std::path::Path,
    processed: &'a crate::media_transcode::ProcessedAv,
    hash: &'a str,
    duration: f64,
    original_bytes: i64,
    format: &'a str,
}
async fn persist(state: &AppState, owner: i64, import: Imported<'_>) -> Result<Asset, ApiError> {
    let Imported {
        input,
        source,
        processed,
        hash,
        duration,
        original_bytes,
        format,
    } = import;
    let asset = id::next();
    let original = format!("music{asset}.orig");
    let stream = format!("music{asset}.mp3");
    let keys = [original.clone(), stream.clone()];
    // Journal is committed before writes. Hold its rows through file creation
    // and commit: cancellation/DB failure leaves cleanup, never orphan bytes.
    reserve(state, owner, &keys, &[original_bytes, processed.file_size]).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock_owner(&mut tx, owner).await?;
    let locked: Vec<String> = sqlx::query_scalar(
        "SELECT file_name FROM media_cleanup_jobs WHERE file_name=ANY($1) AND attempts=0 AND run_at>now() FOR UPDATE",
    )
    .bind(&keys[..])
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if locked.len() != 2 {
        return Err(ApiError::Conflict(
            "Audio import was interrupted; retry".into(),
        ));
    }
    crate::music_queues::active_owner(&mut tx, owner).await?;
    // Serialize duplicate imports for this owner. A concurrent winner remains
    // the identity; this import's journal is cleaned without storing new bytes.
    let duplicate: Option<i64> =
        sqlx::query_scalar("SELECT id FROM music_assets WHERE owner_id=$1 AND audio_hash=$2")
            .bind(owner)
            .bind(hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if let Some(duplicate) = duplicate {
        tx.commit().await.map_err(DbError::from)?;
        return get(state, duplicate, Some(owner)).await;
    }
    let (artist, album) = entities(&mut tx, owner, &input.music).await?;
    state
        .media
        .put_file(&original, source)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    state
        .media
        .put_file(&stream, &processed.file_path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,genre,track_number,disc_number,musicbrainz_id,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format,release_year) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)")
        .bind(asset).bind(owner).bind(input.title.trim()).bind(artist).bind(album).bind(&input.music.genre).bind(input.music.track_number).bind(input.music.disc_number).bind(&input.music.musicbrainz_id).bind(&original).bind(&stream).bind(original_bytes).bind(processed.file_size).bind(hash).bind(duration).bind(format).bind(input.music.release_year).execute(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name=ANY($1)")
        .bind(&keys[..])
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    get(state, asset, Some(owner)).await
}

pub(crate) async fn fingerprint(
    state: &AppState,
    source: &std::path::Path,
    properties: &[u8],
) -> Result<String, ApiError> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use tokio::io::AsyncReadExt;
    let permit = crate::media_gate::acquire().await;
    let mut child = tokio::process::Command::new(&state.config.ffmpeg_path)
        .args(["-nostdin", "-v", "error"])
        .args(crate::media_transcode::local_input_options())
        .arg("-i")
        .arg(source)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-c:a",
            "pcm_f64le",
            "-f",
            "hash",
            "-hash",
            "sha256",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let output = tokio::time::timeout(std::time::Duration::from_mins(5), async {
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("piped output")
            .take(256)
            .read_to_end(&mut output)
            .await?;
        if !child.wait().await?.success() {
            return Err(std::io::Error::other("Audio decoding failed"));
        }
        Ok::<_, std::io::Error>(output)
    })
    .await
    .map_err(|_| ApiError::Unprocessable("Audio decoding timed out".into()))?
    .map_err(|_| ApiError::Unprocessable("Audio decoding failed".into()))?;
    drop(permit);
    let hash = std::str::from_utf8(&output)
        .ok()
        .and_then(|s| s.trim().strip_prefix("SHA256="))
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| ApiError::Unprocessable("Audio fingerprint failed".into()))?
        .to_ascii_lowercase();

    let mut digest = Sha256::new();
    digest.update(properties);
    digest.update(hash.as_bytes());
    Ok(digest
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

pub(crate) async fn reserve(
    state: &AppState,
    owner: i64,
    keys: &[String],
    sizes: &[i64],
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    reserve_conn(state, &mut tx, owner, keys, sizes).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub(crate) async fn reserve_conn(
    state: &AppState,
    tx: &mut sqlx::PgConnection,
    owner: i64,
    keys: &[String],
    sizes: &[i64],
) -> Result<(), ApiError> {
    if keys.len() != sizes.len() || sizes.iter().any(|size| *size <= 0) {
        return Err(ApiError::Unprocessable(
            "Invalid audio storage reservation".into(),
        ));
    }
    lock_owner(tx, owner).await?;
    crate::music_queues::active_owner(tx, owner).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7410141)")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let (account,total):(i64,i64)=sqlx::query_as("SELECT COALESCE(sum(size) FILTER(WHERE owner_id=$1),0)::bigint,COALESCE(sum(size),0)::bigint FROM (SELECT owner_id,(CASE WHEN indexed_original THEN 0 ELSE original_bytes END)+stream_bytes AS size FROM music_assets UNION ALL SELECT owner_id,size_bytes FROM music_artworks UNION ALL SELECT owner_id,size_bytes FROM music_storage_debt UNION ALL SELECT owner_id,size_bytes FROM music_renditions WHERE state='ready' UNION ALL SELECT owner_id,size_bytes FROM music_downloads WHERE state='ready') usage")
        .bind(owner).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    let required = sizes
        .iter()
        .try_fold(0_i64, |n, v| n.checked_add(*v))
        .ok_or_else(|| ApiError::Unprocessable("Audio storage budget exceeded".into()))?;
    let limit = state.config.music;
    let mib = 1024 * 1024;
    if account.saturating_add(required)
        > i64::try_from(limit.per_account_mib * mib).unwrap_or(i64::MAX)
        || total.saturating_add(required)
            > i64::try_from(limit.global_mib * mib).unwrap_or(i64::MAX)
    {
        return Err(ApiError::Unprocessable(
            "Audio storage budget exceeded; remove unused tracks or wait for cleanup".into(),
        ));
    }
    let available = crate::storage::available_bytes(&state.config.media_dir)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    if available
        < u64::try_from(required)
            .unwrap_or(u64::MAX)
            .saturating_add(limit.min_free_mib * mib)
    {
        return Err(ApiError::ServiceUnavailable(
            "Not enough free space for audio import".into(),
        ));
    }
    media_cleanup::enqueue_many(&mut *tx, keys).await?;
    sqlx::query(
        "UPDATE media_cleanup_jobs SET run_at=now()+interval '2 minutes' WHERE file_name=ANY($1)",
    )
    .bind(keys)
    .execute(&mut *tx)
    .await
    .map_err(DbError::from)?;
    sqlx::query("INSERT INTO music_storage_debt(file_name,owner_id,size_bytes) SELECT key,$1,size FROM unnest($2::text[],$3::bigint[]) AS input(key,size)")
        .bind(owner).bind(keys).bind(sizes).execute(&mut *tx).await.map_err(DbError::from)?;
    Ok(())
}

/// A direct edit of a retained Audio's primary metadata updates its source on
/// the same held connection. It never recursively edits the publication.
pub(crate) async fn edit_publication_source_conn(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    status: i64,
    title: Option<&str>,
    metadata: Option<&Metadata>,
    description: Option<(&str, &str)>,
) -> Result<Option<Metadata>, ApiError> {
    lock_owner(conn, owner).await?;
    let source:Option<i64>=sqlx::query_scalar("SELECT t.id FROM music_assets t JOIN music_publications p ON p.asset_id=t.id WHERE p.status_id=$1 AND t.owner_id=$2 AND t.visibility='public' FOR UPDATE OF t").bind(status).bind(owner).fetch_optional(&mut *conn).await.map_err(DbError::from)?;
    let Some(source) = source else {
        return Ok(None);
    };
    let previous = owned_conn(conn, owner, source).await?;
    let input = Edit {
        title: title.unwrap_or(&previous.title).into(),
        music: metadata.cloned().unwrap_or_else(|| previous.metadata()),
        visibility: "public".into(),
        revision: None,
    };
    validate(&input)?;
    let (text, content_type) =
        description.unwrap_or((&previous.description, &previous.description_type));
    if description.is_some() {
        crate::music_track_details::validate_description(text, content_type)?;
    }
    if previous.title != input.title.trim()
        || previous.metadata() != input.music
        || previous.description != text
        || previous.description_type != content_type
    {
        let (artist, album) = edit_entities(conn, owner, &previous, &input.music).await?;
        sqlx::query("UPDATE music_assets SET title=$3,artist_id=$4,album_id=$5,genre=$6,track_number=$7,disc_number=$8,musicbrainz_id=$9,release_year=$10,description=$11,description_type=$12,revision=revision+1 WHERE id=$1 AND owner_id=$2")
            .bind(source).bind(owner).bind(input.title.trim()).bind(artist).bind(album).bind(&input.music.genre).bind(input.music.track_number).bind(input.music.disc_number).bind(&input.music.musicbrainz_id).bind(input.music.release_year).bind(text).bind(content_type).execute(&mut *conn).await.map_err(DbError::from)?;
        let mut references = crate::music_entities::references(&previous);
        references.push((crate::music_entities::Kind::Artist, artist));
        if let Some(album) = album {
            references.push((crate::music_entities::Kind::Album, album));
        }
        crate::music_entities::reconcile_conn(state, conn, owner, &references).await?;
    }
    Ok(Some(owned_conn(conn, owner, source).await?.metadata()))
}

/// All retained-source mutations take the same owner fence before locking
/// assets and reusable metadata, so bulk edits cannot miss concurrent imports.
pub(crate) async fn lock_owner(conn: &mut sqlx::PgConnection, owner: i64) -> Result<(), ApiError> {
    // Retirement locks the account before its music cleanup trigger takes the
    // advisory fence. Use that same order, without blocking another account's
    // foreign-key references (for example, two people saving each other's music).
    sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR NO KEY UPDATE")
        .bind(owner)
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('music-owner:' || $1::text,0))")
        .bind(owner.to_string())
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}

/// A track's own release year is independent of its existing album. Editing
/// other track fields must not silently move it to a duplicate year-keyed album.
async fn edit_entities(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    previous: &Asset,
    metadata: &Metadata,
) -> Result<(i64, Option<i64>), DbError> {
    let artist = if metadata.artist.trim().is_empty() {
        "Unknown artist"
    } else {
        metadata.artist.trim()
    };
    if metadata.album.trim() == previous.album {
        let artist_id = if artist == previous.artist {
            previous.artist_id
        } else {
            let mut performer = metadata.clone();
            performer.album.clear();
            entities(conn, owner, &performer).await?.0
        };
        Ok((artist_id, previous.album_id))
    } else {
        entities(conn, owner, metadata).await
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::lock_owner;
    use crate::error::ApiError;
    use plamenu_db::{PgPool, account};
    use std::time::Duration;

    #[sqlx::test(migrations = "../db/migrations")]
    async fn retirement_and_import_use_the_same_account_lock_order(pool: PgPool) {
        let owner = account::create_local(
            &pool,
            account::NewLocalAccount {
                username: "music_lifecycle",
                display_name: "Music lifecycle",
                note: "",
                public_key_pem: "test",
            },
        )
        .await
        .unwrap()
        .id;
        let mut retirement = pool.begin().await.unwrap();
        let retiring_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *retirement)
            .await
            .unwrap();
        // Admin deletion already holds this lock while rechecking eligibility.
        sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR UPDATE")
            .bind(owner)
            .execute(&mut *retirement)
            .await
            .unwrap();
        let (send, receive) = tokio::sync::oneshot::channel();
        let importing_pool = pool.clone();
        let importing = tokio::spawn(async move {
            let mut tx = importing_pool.begin().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            send.send(pid).unwrap();
            lock_owner(&mut tx, owner).await?;
            // Source rows take this lock when checking their owner foreign key.
            sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR KEY SHARE")
                .bind(owner)
                .execute(&mut *tx)
                .await
                .map_err(plamenu_db::DbError::from)?;
            crate::music_queues::active_owner(&mut tx, owner).await
        });
        let importing_pid = receive.await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT $2=ANY(pg_blocking_pids($1))")
                    .bind(importing_pid)
                    .bind(retiring_pid)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                if waiting {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("import must reach the held account lock");
        let marked = tokio::time::timeout(
            Duration::from_secs(10),
            account::mark_deleted(&mut *retirement, owner),
        )
        .await
        .expect("retirement must not deadlock with the import fence");
        if marked.is_ok() {
            retirement.commit().await.unwrap();
        } else {
            retirement.rollback().await.unwrap();
        }
        let imported = tokio::time::timeout(Duration::from_secs(10), importing)
            .await
            .unwrap()
            .unwrap();
        assert!(marked.is_ok(), "retirement failed: {marked:?}");
        assert!(
            matches!(imported, Err(ApiError::NotFound)),
            "the import must see retirement after waiting: {imported:?}"
        );
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn owner_fence_allows_other_accounts_foreign_key_checks(pool: PgPool) {
        let owner = account::create_local(
            &pool,
            account::NewLocalAccount {
                username: "music_shared_owner",
                display_name: "Shared music",
                note: "",
                public_key_pem: "test",
            },
        )
        .await
        .unwrap()
        .id;
        let mut mutation = pool.begin().await.unwrap();
        lock_owner(&mut mutation, owner).await.unwrap();
        let mut reference = pool.begin().await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(3),
            sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR KEY SHARE")
                .bind(owner)
                .execute(&mut *reference),
        )
        .await
        .expect("foreign-key checks must not wait behind the music owner fence")
        .unwrap();
        reference.rollback().await.unwrap();
        mutation.rollback().await.unwrap();
    }
}
