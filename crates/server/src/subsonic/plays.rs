//! Explicit play reports share personal history and opt-in now-playing disclosure.
use super::{
    playlists,
    protocol::{Error, Params},
};
use crate::{
    AppState,
    error::ApiError,
    music_clients::auth::Principal,
    music_collections::{self, Entry},
};
use plamenu_db::{DbError, id};
use serde_json::{Value, json};
use time::OffsetDateTime;

pub async fn dispatch(
    state: &AppState,
    principal: &Principal,
    method: &str,
    params: &Params,
) -> Result<Value, Error> {
    if method == "getNowPlaying" {
        return now_playing(state, principal).await;
    }
    principal.require_manage()?;
    let songs = params.all("id");
    let times = params.all("time");
    if songs.len() > 50 {
        return Err(ApiError::Unprocessable("Report up to 50 songs per request".into()).into());
    }
    for (index, song) in songs.into_iter().enumerate() {
        let Ok(source) = playlists::source(song) else {
            continue;
        };
        let mut entry = source.entry(1, 0, 1)?;
        if crate::music_remote::normalize_sources(
            state,
            std::slice::from_mut(&mut entry),
            principal.owner,
        )
        .await
        .is_err()
        {
            continue;
        }
        if music_collections::sources(state, std::slice::from_ref(&entry), Some(principal.owner))
            .await?
            .is_empty()
        {
            continue;
        }
        if method == "scrobble" && params.boolean("submission", true) {
            let provided = times.get(index).and_then(|value| value.parse::<i64>().ok());
            let now = OffsetDateTime::now_utc();
            let played_at = provided
                .and_then(|ms| {
                    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
                })
                .filter(|time| *time <= now + time::Duration::minutes(5))
                .unwrap_or(now);
            let occurrence = provided.map_or_else(
                || format!("subsonic-{}", id::next()),
                |ms| {
                    format!(
                        "subsonic-{}",
                        crate::auth::hash_secret(&format!(
                            "{}:{:?}:{:?}:{:?}:{ms}",
                            principal.owner, entry.asset_id, entry.status_id, entry.remote_id
                        ))
                    )
                },
            );
            crate::music_listening::complete_for_client(
                state,
                principal,
                &source,
                &occurrence,
                played_at,
            )
            .await?;
        } else {
            report(
                state,
                principal,
                &entry,
                params.get("c").unwrap_or_default(),
            )
            .await?;
        }
    }
    Ok(json!({}))
}
async fn report(
    state: &AppState,
    principal: &Principal,
    entry: &Entry,
    player: &str,
) -> Result<(), Error> {
    if player.chars().count() > 100 || player.contains('\0') {
        return Err(ApiError::Unprocessable("Invalid client name".into()).into());
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(DbError::from)
        .map_err(ApiError::from)?;
    crate::music_assets::lock_owner(&mut tx, principal.owner).await?;
    principal.current(state).await?;
    sqlx::query("DELETE FROM music_client_playing WHERE account_id=$1 AND reported_at<now()-interval '10 minutes'").bind(principal.owner).execute(&mut *tx).await.map_err(DbError::from).map_err(ApiError::from)?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM music_client_playing WHERE account_id=$1")
            .bind(principal.owner)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)
            .map_err(ApiError::from)?;
    if count >= 50 {
        sqlx::query("DELETE FROM music_client_playing WHERE account_id=$1 AND source_key IN(SELECT source_key FROM music_client_playing WHERE account_id=$1 ORDER BY reported_at LIMIT 1)").bind(principal.owner).execute(&mut *tx).await.map_err(DbError::from).map_err(ApiError::from)?;
    }
    sqlx::query("INSERT INTO music_client_playing(account_id,credential_id,asset_id,status_id,remote_id,player) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(account_id,source_key) DO UPDATE SET credential_id=EXCLUDED.credential_id,player=EXCLUDED.player,reported_at=now()")
        .bind(principal.owner).bind(principal.key_id).bind(entry.asset_id).bind(entry.status_id).bind(entry.remote_id).bind(player).execute(&mut *tx).await.map_err(DbError::from).map_err(ApiError::from)?;
    tx.commit()
        .await
        .map_err(DbError::from)
        .map_err(ApiError::from)?;
    Ok(())
}
#[derive(sqlx::FromRow)]
struct Playing {
    account_id: i64,
    source_key: String,
    username: String,
    asset_id: Option<i64>,
    status_id: Option<i64>,
    remote_id: Option<i64>,
    player: String,
    reported_at: OffsetDateTime,
}
async fn now_playing(state: &AppState, principal: &Principal) -> Result<Value, Error> {
    let mut cursor: Option<(OffsetDateTime, i64, String)> = None;
    let mut result = Vec::new();
    // Bound physical work independently of the 500 currently playable entries.
    for _ in 0..100 {
        let rows:Vec<Playing>=sqlx::query_as("SELECT p.*,a.username FROM music_client_playing p JOIN accounts a ON a.id=p.account_id JOIN users u ON u.account_id=a.id LEFT JOIN music_client_keys k ON k.id=p.credential_id LEFT JOIN oauth_tokens parent ON parent.id=k.parent_token_id WHERE p.reported_at>now()-interval '10 minutes' AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND u.confirmed_at IS NOT NULL AND u.approved AND NOT u.disabled AND (p.account_id=$1 OR a.silenced_at IS NULL) AND NOT EXISTS(SELECT 1 FROM mutes mute WHERE mute.account_id=$1 AND mute.target_account_id=p.account_id AND (mute.expires_at IS NULL OR mute.expires_at>now())) AND (p.credential_id IS NULL OR (k.id IS NOT NULL AND k.expires_at>now() AND (k.parent_token_id IS NULL OR (parent.revoked_at IS NULL AND (parent.scopes ~ '(^| )(read|read:statuses)( |$)') AND (parent.scopes ~ '(^| )(write|write:statuses)( |$)'))))) AND (p.account_id=$1 OR k.broadcast_playing) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$1 AND b.target_account_id=p.account_id) OR (b.account_id=p.account_id AND b.target_account_id=$1)) AND ($2::timestamptz IS NULL OR p.reported_at<$2 OR (p.reported_at=$2 AND (p.account_id,p.source_key)>($3::bigint,$4::text))) ORDER BY p.reported_at DESC,p.account_id,p.source_key LIMIT 100")
            .bind(principal.owner).bind(cursor.as_ref().map(|c|c.0)).bind(cursor.as_ref().map(|c|c.1)).bind(cursor.as_ref().map(|c|c.2.as_str())).fetch_all(&state.pool).await.map_err(DbError::from).map_err(ApiError::from)?;
        let entries = rows
            .iter()
            .enumerate()
            .map(|(index, row)| Entry {
                remote_id: row.remote_id,
                episode_id: None,
                asset_id: row.asset_id,
                status_id: row.status_id,
                id: i64::try_from(index).unwrap_or(100),
                collection_id: 0,
                position: 1,
            })
            .collect::<Vec<_>>();
        for (entry, mut song) in playlists::songs(state, principal.owner, &entries).await? {
            let Some(row) = usize::try_from(entry.id)
                .ok()
                .and_then(|index| rows.get(index))
            else {
                continue;
            };
            song["username"] = json!(row.username);
            song["minutesAgo"] = json!(
                (OffsetDateTime::now_utc() - row.reported_at)
                    .whole_minutes()
                    .max(0)
            );
            if !row.player.is_empty() {
                song["playerName"] = json!(row.player);
            }
            result.push(song);
            if result.len() == 500 {
                return Ok(json!({"nowPlaying":{"entry":result}}));
            }
        }
        if rows.len() < 100 {
            break;
        }
        cursor = rows
            .last()
            .map(|r| (r.reported_at, r.account_id, r.source_key.clone()));
    }
    Ok(json!({"nowPlaying":{"entry":result}}))
}
