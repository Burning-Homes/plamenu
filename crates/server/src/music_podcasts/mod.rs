//! Native RSS/Atom subscriptions in the Music workspace.
mod discovery;
pub mod feed;
use crate::{AppState, error::ApiError, music_assets, music_queues};
use plamenu_db::{DbError, id};
use plamenu_federation::FetchedFeed;
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Podcast {
    pub id: i64,
    pub feed_url: String,
    pub title: String,
    pub description: String,
    pub author: String,
    pub link: Option<String>,
    pub image_url: Option<String>,
    pub language: String,
    pub categories: Vec<String>,
    pub explicit: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub last_fetched_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    pub next_refresh_at: OffsetDateTime,
    pub revision: i64,
}
impl Podcast {
    #[must_use]
    pub fn entity(&self) -> Value {
        json!({"id":self.id.to_string(),"feed_url":self.feed_url,"title":self.title,"description":self.description,"author":self.author,"link":self.link,"image_url":self.image_url,"language":self.language,"categories":self.categories,"explicit":self.explicit,"last_fetched_at":self.last_fetched_at.and_then(|v|crate::entities::rfc3339(v).ok()),"last_error":self.last_error,"revision":self.revision.to_string(),"url":format!("/music/podcasts/{}",self.id)})
    }
}
#[derive(Debug, sqlx::FromRow)]
pub struct Episode {
    pub id: i64,
    pub podcast_id: i64,
    pub guid: String,
    pub title: String,
    pub description: String,
    pub link: Option<String>,
    pub image_url: Option<String>,
    pub audio_url: String,
    pub audio_type: String,
    pub audio_length: Option<i64>,
    pub duration_seconds: Option<i32>,
    pub published_at: Option<OffsetDateTime>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub episode_type: Option<String>,
    pub played: bool,
}
impl Episode {
    #[must_use]
    pub fn entity(&self) -> Value {
        json!({"id":self.id.to_string(),"podcast_id":self.podcast_id.to_string(),"guid":self.guid,"title":self.title,"description":self.description,"link":self.link,"image_url":self.image_url,"audio_url":self.audio_url,"stream_url":format!("/music/podcasts/episodes/{}/audio",self.id),"audio_type":self.audio_type,"audio_length":self.audio_length,"duration_seconds":self.duration_seconds,"published_at":self.published_at.and_then(|v|crate::entities::rfc3339(v).ok()),"season_number":self.season_number,"episode_number":self.episode_number,"episode_type":self.episode_type,"played":self.played,"url":format!("/music/podcasts/episodes/{}",self.id)})
    }
}
#[derive(sqlx::FromRow)]
struct Subscribed {
    #[sqlx(flatten)]
    podcast: Podcast,
    episode_count: i64,
    unplayed_count: i64,
    latest_episode_at: Option<OffsetDateTime>,
}
#[derive(Default, Deserialize)]
pub struct Query {
    pub q: Option<String>,
    pub sort: Option<String>,
    pub direction: Option<String>,
    pub offset: Option<i64>,
}
pub struct List {
    pub items: Vec<Value>,
    pub next: Option<i64>,
}
fn offset(value: Option<i64>) -> Result<i64, ApiError> {
    let value = value.unwrap_or(0);
    if !(0..=1_000_000).contains(&value) {
        return Err(ApiError::Unprocessable("Invalid podcast page".into()));
    }
    Ok(value)
}
async fn active(state: &AppState, owner: i64) -> Result<(), ApiError> {
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    music_queues::active_owner(&mut conn, owner).await
}
pub async fn get(state: &AppState, id: i64, viewer: i64) -> Result<Podcast, ApiError> {
    active(state, viewer).await?;
    sqlx::query_as("SELECT * FROM music_podcasts WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)
}
pub async fn following(state: &AppState, show: i64, viewer: i64) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_podcast_subscriptions WHERE account_id=$1 AND podcast_id=$2)").bind(viewer).bind(show).fetch_one(&state.pool).await.map_err(DbError::from)?)
}
pub async fn list(state: &AppState, viewer: i64, input: &Query) -> Result<List, ApiError> {
    active(state, viewer).await?;
    let offset = offset(input.offset)?;
    let query = input.q.as_deref().unwrap_or_default().trim();
    if query.chars().count() > 200 || query.contains('\0') {
        return Err(ApiError::Unprocessable("Invalid podcast search".into()));
    }
    let sort = input.sort.as_deref().unwrap_or("latest");
    if !matches!(sort, "latest" | "episodes" | "unplayed" | "name") {
        return Err(ApiError::Unprocessable("Invalid podcast sort".into()));
    }
    let asc = match input.direction.as_deref() {
        None => sort == "name",
        Some("asc") => true,
        Some("desc") => false,
        _ => {
            return Err(ApiError::Unprocessable(
                "Invalid podcast sort direction".into(),
            ));
        }
    };
    let mut sql = sqlx::QueryBuilder::new(
        "SELECT p.*,COALESCE(stats.episode_count,0)::bigint AS episode_count,COALESCE(stats.unplayed_count,0)::bigint AS unplayed_count,stats.latest_episode_at FROM music_podcast_subscriptions s JOIN music_podcasts p ON p.id=s.podcast_id LEFT JOIN LATERAL (SELECT count(*) AS episode_count,count(*) FILTER(WHERE plays.episode_id IS NULL) AS unplayed_count,max(e.published_at) AS latest_episode_at FROM music_podcast_episodes e LEFT JOIN music_podcast_plays plays ON plays.episode_id=e.id AND plays.account_id=s.account_id WHERE e.podcast_id=p.id) stats ON true WHERE s.account_id=",
    );
    sql.push_bind(viewer)
        .push(" AND (")
        .push_bind(query)
        .push("='' OR strpos(lower(p.title || ' ' || p.author || ' ' || p.description),lower(")
        .push_bind(query)
        .push("))>0) ORDER BY ");
    sql.push(match sort {
        "name" => "lower(p.title)",
        "episodes" => "episode_count",
        "unplayed" => "unplayed_count",
        _ => "latest_episode_at",
    })
    .push(if asc {
        " ASC NULLS LAST"
    } else {
        " DESC NULLS LAST"
    })
    .push(",p.id ASC LIMIT 51 OFFSET ")
    .push_bind(offset);
    let mut rows = sql
        .build_query_as::<Subscribed>()
        .fetch_all(&state.pool)
        .await
        .map_err(DbError::from)?;
    let next = if rows.len() > 50 {
        rows.truncate(50);
        Some(offset + 50)
    } else {
        None
    };
    let items = rows
        .into_iter()
        .map(|row| {
            let mut item = row.podcast.entity();
            item["episode_count"] = json!(row.episode_count);
            item["unplayed_count"] = json!(row.unplayed_count);
            item["latest_episode_at"] = json!(
                row.latest_episode_at
                    .and_then(|v| crate::entities::rfc3339(v).ok())
            );
            item["following"] = json!(true);
            item
        })
        .collect();
    Ok(List { items, next })
}
async fn fetch(
    state: &AppState,
    url: &str,
    etag: Option<&str>,
    modified: Option<&str>,
) -> Result<FetchedFeed, ApiError> {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.federation.fetch_feed(url, etag, modified),
    )
    .await
    .map_err(|_| ApiError::Unprocessable("Podcast feed timed out".into()))?
    .map_err(|_| ApiError::Unprocessable("Could not fetch podcast feed".into()))
}
async fn apply(
    conn: &mut sqlx::PgConnection,
    show: i64,
    feed: &feed::Feed,
) -> Result<(), ApiError> {
    // Preserve omitted historical episodes and played state; GUID updates retain identity.
    sqlx::query("UPDATE music_podcasts SET title=$2,description=$3,author=$4,link=$5,image_url=$6,language=$7,categories=$8,explicit=$9 WHERE id=$1")
        .bind(show).bind(&feed.title).bind(&feed.description).bind(&feed.author).bind(&feed.link).bind(&feed.image_url).bind(&feed.language).bind(&feed.categories).bind(feed.explicit).execute(&mut *conn).await.map_err(DbError::from)?;
    let guids = feed
        .episodes
        .iter()
        .map(|e| e.guid.clone())
        .collect::<Vec<_>>();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM music_podcast_episodes WHERE podcast_id=$1 AND NOT(guid=ANY($2))",
    )
    .bind(show)
    .bind(&guids)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if count.saturating_add(i64::try_from(guids.len()).unwrap_or(i64::MAX)) > 100_000 {
        return Err(ApiError::Unprocessable(
            "Podcast episode catalogue limit reached".into(),
        ));
    }
    for batch in feed.episodes.chunks(100) {
        let mut sql = sqlx::QueryBuilder::new(
            "INSERT INTO music_podcast_episodes(id,podcast_id,guid,title,description,link,image_url,audio_url,audio_type,audio_length,duration_seconds,published_at,season_number,episode_number,episode_type) ",
        );
        sql.push_values(batch, |mut row, e| {
            row.push_bind(id::next())
                .push_bind(show)
                .push_bind(&e.guid)
                .push_bind(&e.title)
                .push_bind(&e.description)
                .push_bind(&e.link)
                .push_bind(&e.image_url)
                .push_bind(&e.audio_url)
                .push_bind(&e.audio_type)
                .push_bind(e.audio_length)
                .push_bind(e.duration_seconds)
                .push_bind(e.published_at)
                .push_bind(e.season_number)
                .push_bind(e.episode_number)
                .push_bind(&e.episode_type);
        });
        sql.push(" ON CONFLICT(podcast_id,guid) DO UPDATE SET title=EXCLUDED.title,description=EXCLUDED.description,link=EXCLUDED.link,image_url=EXCLUDED.image_url,audio_url=EXCLUDED.audio_url,audio_type=EXCLUDED.audio_type,audio_length=EXCLUDED.audio_length,duration_seconds=EXCLUDED.duration_seconds,published_at=EXCLUDED.published_at,season_number=EXCLUDED.season_number,episode_number=EXCLUDED.episode_number,episode_type=EXCLUDED.episode_type").build().execute(&mut *conn).await.map_err(DbError::from)?;
    }
    Ok(())
}
pub async fn subscribe(
    state: &AppState,
    owner: i64,
    raw: &str,
) -> Result<(Podcast, bool), ApiError> {
    active(state, owner).await?;
    let mut url = feed::normalize_url(raw)?;
    let existing: Option<Podcast> =
        sqlx::query_as("SELECT * FROM music_podcasts WHERE feed_url=$1")
            .bind(&url)
            .fetch_optional(&state.pool)
            .await
            .map_err(DbError::from)?;
    if let Some(show) = &existing
        && !following(state, show.id, owner).await?
    {
        // A new listener receives a current catalogue. Failure preserves the
        // shared last-good data but does not create a subscription.
        refresh_show(state, show.id, true).await?;
    }
    let fetched = if existing.is_none() {
        let (resolved, fetched, parsed) = resolve_link(state, &url).await?;
        url = resolved;
        Some((fetched, parsed))
    } else {
        None
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM music_podcast_subscriptions WHERE account_id=$1")
            .bind(owner)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if let Some((_, parsed)) = &fetched {
        sqlx::query("INSERT INTO music_podcasts(id,feed_url,title) VALUES($1,$2,$3) ON CONFLICT(feed_url) DO NOTHING").bind(id::next()).bind(&url).bind(&parsed.title).execute(&mut *tx).await.map_err(DbError::from)?;
    }
    let show: Podcast = sqlx::query_as("SELECT * FROM music_podcasts WHERE feed_url=$1 FOR UPDATE")
        .bind(&url)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let already:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_podcast_subscriptions WHERE account_id=$1 AND podcast_id=$2)").bind(owner).bind(show.id).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if count >= 500 && !already {
        return Err(ApiError::Unprocessable(
            "Podcast subscription limit is 500".into(),
        ));
    }
    if let Some((fetched, parsed)) = &fetched {
        apply(&mut tx, show.id, parsed).await?;
        sqlx::query("UPDATE music_podcasts SET etag=$2,last_modified=$3,last_fetched_at=now(),next_refresh_at=now()+interval '1 hour' WHERE id=$1").bind(show.id).bind(&fetched.etag).bind(&fetched.last_modified).execute(&mut *tx).await.map_err(DbError::from)?;
    }
    let changed=sqlx::query("INSERT INTO music_podcast_subscriptions(account_id,podcast_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(owner).bind(show.id).execute(&mut *tx).await.map_err(DbError::from)?.rows_affected()>0;
    tx.commit().await.map_err(DbError::from)?;
    Ok((get(state, show.id, owner).await?, changed))
}

async fn resolve_link(
    state: &AppState,
    url: &str,
) -> Result<(String, FetchedFeed, feed::Feed), ApiError> {
    let fetched = fetch(state, url, None, None).await?;
    let bytes = fetched
        .bytes
        .as_deref()
        .ok_or_else(|| ApiError::Unprocessable("Podcast link returned no content".into()))?;
    if let Ok(parsed) = feed::parse(bytes, &fetched.final_url) {
        return Ok((url.into(), fetched, parsed));
    }
    let candidate = discovery::feed_link(bytes, &fetched.final_url).ok_or_else(|| {
        ApiError::Unprocessable(
            "No podcast was found at this link. Try the podcast's website or its RSS feed link."
                .into(),
        )
    })?;
    let fetched = fetch(state, &candidate, None, None).await?;
    let parsed = feed::parse(
        fetched
            .bytes
            .as_deref()
            .ok_or_else(|| ApiError::Unprocessable("Podcast feed returned no content".into()))?,
        &fetched.final_url,
    )?;
    Ok((candidate, fetched, parsed))
}
pub async fn unsubscribe(state: &AppState, owner: i64, show: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    sqlx::query("DELETE FROM music_podcast_subscriptions WHERE account_id=$1 AND podcast_id=$2")
        .bind(owner)
        .bind(show)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn refresh(state: &AppState, show: i64, viewer: i64) -> Result<Podcast, ApiError> {
    get(state, show, viewer).await?;
    refresh_show(state, show, true).await?;
    get(state, show, viewer).await
}
pub async fn refresh_show(state: &AppState, show: i64, force: bool) -> Result<(), ApiError> {
    let lease = id::next();
    let current:Podcast=sqlx::query_as("UPDATE music_podcasts SET refresh_lease=$2,refresh_until=now()+interval '2 minutes' WHERE id=$1 AND (refresh_until IS NULL OR refresh_until<now()) AND ($3 OR next_refresh_at<=now()) RETURNING *").bind(show).bind(lease).bind(force).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or_else(||ApiError::Conflict("Podcast refresh already running or not due".into()))?;
    let fetched = fetch(
        state,
        &current.feed_url,
        current.etag.as_deref(),
        current.last_modified.as_deref(),
    )
    .await;
    let parsed = fetched.as_ref().ok().and_then(|f| {
        f.bytes
            .as_deref()
            .map(|bytes| feed::parse(bytes, &f.final_url))
    });
    let error = if fetched.is_err() {
        Some("Could not fetch podcast feed")
    } else if parsed.as_ref().is_some_and(Result::is_err) {
        Some("Could not parse podcast feed")
    } else if fetched.as_ref().is_ok_and(|f| f.bytes.is_none()) && current.last_fetched_at.is_none()
    {
        Some("Podcast feed returned no content")
    } else {
        None
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let valid: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM music_podcasts WHERE id=$1 AND refresh_lease=$2 AND revision=$3 FOR UPDATE",
    )
    .bind(show)
    .bind(lease)
    .bind(current.revision)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if valid.is_none() {
        return Err(ApiError::Conflict("Podcast changed during refresh".into()));
    }
    if let Some(message) = error {
        sqlx::query("UPDATE music_podcasts SET refresh_lease=NULL,refresh_until=NULL,last_error=$2,next_refresh_at=now()+interval '15 minutes' WHERE id=$1").bind(show).bind(message).execute(&mut *tx).await.map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        return Err(ApiError::Unprocessable(message.into()));
    }
    if let Some(Ok(parsed)) = parsed {
        apply(&mut tx, show, &parsed).await?;
    }
    let fetched =
        fetched.map_err(|_| ApiError::Unprocessable("Could not fetch podcast feed".into()))?;
    sqlx::query("UPDATE music_podcasts SET etag=CASE WHEN $4 THEN COALESCE($2,etag) ELSE $2 END,last_modified=CASE WHEN $4 THEN COALESCE($3,last_modified) ELSE $3 END,last_fetched_at=now(),last_error=NULL,refresh_lease=NULL,refresh_until=NULL,revision=revision+1,next_refresh_at=now()+interval '1 hour' WHERE id=$1").bind(show).bind(&fetched.etag).bind(&fetched.last_modified).bind(fetched.bytes.is_none()).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn episodes(
    state: &AppState,
    show: i64,
    viewer: i64,
    oldest: bool,
    page: Option<i64>,
) -> Result<List, ApiError> {
    get(state, show, viewer).await?;
    let offset = offset(page)?;
    let mut sql = sqlx::QueryBuilder::new(
        "SELECT e.*,EXISTS(SELECT 1 FROM music_podcast_plays p WHERE p.episode_id=e.id AND p.account_id=",
    );
    sql.push_bind(viewer)
        .push(") AS played FROM music_podcast_episodes e WHERE e.podcast_id=")
        .push_bind(show)
        .push(" ORDER BY e.published_at")
        .push(if oldest {
            " ASC NULLS LAST,e.id ASC"
        } else {
            " DESC NULLS LAST,e.id DESC"
        })
        .push(" LIMIT 51 OFFSET ")
        .push_bind(offset);
    let mut rows = sql
        .build_query_as::<Episode>()
        .fetch_all(&state.pool)
        .await
        .map_err(DbError::from)?;
    let next = if rows.len() > 50 {
        rows.truncate(50);
        Some(offset + 50)
    } else {
        None
    };
    Ok(List {
        items: rows.into_iter().map(|e| e.entity()).collect(),
        next,
    })
}
/// Select the displayed page using the catalogue's bounded ordering and paging.
pub async fn page_sources(
    state: &AppState,
    show: i64,
    owner: i64,
    oldest: bool,
    page: Option<i64>,
) -> Result<Vec<music_queues::Source>, ApiError> {
    let page = episodes(state, show, owner, oldest, page).await?;
    Ok(page
        .items
        .iter()
        .filter_map(|item| item["id"].as_str())
        .map(|episode| music_queues::Source {
            remote_id: None,
            asset_id: None,
            status_id: None,
            episode_id: Some(episode.to_owned()),
        })
        .collect())
}
/// Apply one atomic per-listener change to explicitly selected episodes.
pub async fn played_batch(
    state: &AppState,
    owner: i64,
    show: i64,
    episodes: &[i64],
    enabled: bool,
) -> Result<(), ApiError> {
    if episodes.is_empty() || episodes.len() > 50 {
        return Err(ApiError::Unprocessable(
            "Choose up to 50 podcast episodes".into(),
        ));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let unique: std::collections::HashSet<_> = episodes.iter().copied().collect();
    let found: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM music_podcast_episodes WHERE podcast_id=$1 AND id=ANY($2) FOR KEY SHARE",
    )
    .bind(show)
    .bind(episodes)
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if found.len() != unique.len() {
        return Err(ApiError::NotFound);
    }
    if enabled {
        sqlx::query("INSERT INTO music_podcast_plays(account_id,episode_id) SELECT $1,unnest($2::bigint[]) ON CONFLICT DO NOTHING").bind(owner).bind(episodes).execute(&mut *tx).await.map_err(DbError::from)?;
    } else {
        sqlx::query("DELETE FROM music_podcast_plays WHERE account_id=$1 AND episode_id=ANY($2)")
            .bind(owner)
            .bind(episodes)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn episode(state: &AppState, id: i64, viewer: i64) -> Result<Episode, ApiError> {
    active(state, viewer).await?;
    sqlx::query_as("SELECT e.*,EXISTS(SELECT 1 FROM music_podcast_plays p WHERE p.episode_id=e.id AND p.account_id=$2) AS played FROM music_podcast_episodes e WHERE e.id=$1").bind(id).bind(viewer).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)
}
pub async fn played(
    state: &AppState,
    owner: i64,
    episode: i64,
    enabled: bool,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_podcast_episodes WHERE id=$1)")
            .bind(episode)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if !exists {
        return Err(ApiError::NotFound);
    }
    if enabled {
        sqlx::query("INSERT INTO music_podcast_plays(account_id,episode_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(owner).bind(episode).execute(&mut *tx).await.map_err(DbError::from)?;
    } else {
        sqlx::query("DELETE FROM music_podcast_plays WHERE account_id=$1 AND episode_id=$2")
            .bind(owner)
            .bind(episode)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn opml(state: &AppState, owner: i64) -> Result<String, ApiError> {
    active(state, owner).await?;
    let shows:Vec<Podcast>=sqlx::query_as("SELECT p.* FROM music_podcast_subscriptions s JOIN music_podcasts p ON p.id=s.podcast_id WHERE s.account_id=$1 ORDER BY lower(p.title),p.id").bind(owner).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let xml=maud::html!{opml version="2.0"{head{title{"Plamenu podcasts"}}body{@for show in shows{outline type="rss" text=(show.title) title=(show.title) xmlUrl=(show.feed_url){}}}}}.into_string();
    Ok(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{xml}"))
}
pub async fn import_opml(state: &AppState, owner: i64, bytes: &[u8]) -> Result<Value, ApiError> {
    active(state, owner).await?;
    let urls = feed::parse_opml(bytes)?;
    let mut subscribed = 0;
    let mut skipped = 0;
    let mut errors = Vec::new();
    for url in urls {
        match subscribe(state, owner, &url).await {
            Ok((_, true)) => subscribed += 1,
            Ok((_, false)) => skipped += 1,
            Err(ApiError::Unprocessable(message)) => {
                errors.push(json!({"feed_url":url,"error":message}));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(json!({"subscribed":subscribed,"skipped":skipped,"failed":errors.len(),"errors":errors}))
}
#[must_use]
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result:Result<Vec<i64>,_>=sqlx::query_scalar("SELECT p.id FROM music_podcasts p WHERE p.next_refresh_at<=now() AND (p.refresh_until IS NULL OR p.refresh_until<now()) AND EXISTS(SELECT 1 FROM music_podcast_subscriptions s JOIN accounts a ON a.id=s.account_id WHERE s.podcast_id=p.id AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL))) ORDER BY p.next_refresh_at,p.id LIMIT 20").fetch_all(&state.pool).await;
            match result {
                Ok(ids) => {
                    for id in ids {
                        if state.shutdown.is_cancelled() {
                            return;
                        }
                        if let Err(error) = refresh_show(&state, id, false).await
                            && !matches!(error, ApiError::Conflict(_) | ApiError::Unprocessable(_))
                        {
                            tracing::error!(%error,"Podcast refresh failed");
                        }
                    }
                }
                Err(error) => tracing::error!(%error,"Podcast refresh scan failed"),
            }
            if !crate::workers::pause(&state, std::time::Duration::from_secs(60)).await {
                break;
            }
        }
    })
}

/// RSS episode references resolve current feed metadata in bounded batches.
/// An explicitly readable music container may include these public enclosures;
/// subscription and played state never become collection metadata.
#[derive(sqlx::FromRow)]
struct SourceRow {
    id: i64,
    title: String,
    show_title: String,
    duration_seconds: Option<i32>,
    image_url: Option<String>,
}
pub(crate) async fn playables_pool(
    pool: &sqlx::PgPool,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, crate::music_collections::Playable>, ApiError> {
    if ids.len() > 100 {
        return Err(ApiError::Unprocessable(
            "Podcast source batch is too large".into(),
        ));
    }
    let rows:Vec<SourceRow>=sqlx::query_as("SELECT e.id,e.title,p.title AS show_title,e.audio_url,e.duration_seconds,COALESCE(e.image_url,p.image_url) AS image_url FROM music_podcast_episodes e JOIN music_podcasts p ON p.id=e.podcast_id WHERE e.id=ANY($1)").bind(ids).fetch_all(pool).await.map_err(DbError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.id,
                crate::music_collections::Playable {
                    entry_id: String::new(),
                    title: row.title,
                    artist: row.show_title,
                    url: format!("/music/podcasts/episodes/{}", row.id),
                    stream_url: format!("/music/podcasts/episodes/{}/audio", row.id),
                    duration: f64::from(row.duration_seconds.unwrap_or(0)),
                    image_url: row
                        .image_url
                        .map(|_| format!("/music/podcasts/episodes/{}/image", row.id)),
                },
            )
        })
        .collect())
}

pub async fn details(state: &AppState, show: i64, viewer: i64) -> Result<Value, ApiError> {
    let mut value = get(state, show, viewer).await?.entity();
    let (count,unplayed,latest):(i64,i64,Option<OffsetDateTime>)=sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE NOT EXISTS(SELECT 1 FROM music_podcast_plays played WHERE played.episode_id=e.id AND played.account_id=$2)),max(e.published_at) FROM music_podcast_episodes e WHERE e.podcast_id=$1").bind(show).bind(viewer).fetch_one(&state.pool).await.map_err(DbError::from)?;
    value["episode_count"] = json!(count);
    value["unplayed_count"] = json!(unplayed);
    value["latest_episode_at"] = json!(latest.and_then(|v| crate::entities::rfc3339(v).ok()));
    value["following"] = json!(following(state, show, viewer).await?);
    Ok(value)
}
