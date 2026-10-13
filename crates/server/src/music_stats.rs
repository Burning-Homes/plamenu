//! Private listening charts aggregate explicit counted sessions over current sources.
use crate::{
    AppState,
    error::ApiError,
    music_assets,
    music_collections::{self, Entry, Playable},
};
use futures_util::TryStreamExt;
use plamenu_db::{DbError, music};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use time::{Date, Duration, Month, OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    #[default]
    Day,
    Week,
    Month,
    Year,
}
impl Group {
    fn sql(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
    fn key(self, date: Date, week_start: i32) -> Result<Date, ApiError> {
        let invalid = || ApiError::Unprocessable("Statistics date is out of range".into());
        match self {
            Self::Day => Ok(date),
            Self::Week => date
                .checked_sub(Duration::days(i64::from(
                    (i32::from(date.weekday().number_days_from_monday()) - week_start + 7) % 7,
                )))
                .ok_or_else(invalid),
            Self::Month => {
                Date::from_calendar_date(date.year(), date.month(), 1).map_err(|_| invalid())
            }
            Self::Year => {
                Date::from_calendar_date(date.year(), Month::January, 1).map_err(|_| invalid())
            }
        }
    }
    fn next(self, date: Date) -> Option<Date> {
        match self {
            Self::Day => date.next_day(),
            Self::Week => date.checked_add(Duration::days(7)),
            Self::Month => {
                let next = date.month().next();
                let year = date.year() + i32::from(next == Month::January);
                Date::from_calendar_date(year, next, 1).ok()
            }
            Self::Year => Date::from_calendar_date(date.year() + 1, Month::January, 1).ok(),
        }
    }
    fn label(self, date: Date) -> String {
        match self {
            Self::Day | Self::Week => date.to_string(),
            Self::Month => format!("{:04}-{:02}", date.year(), u8::from(date.month())),
            Self::Year => format!("{:04}", date.year()),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseGroup {
    Year,
    #[default]
    Decade,
}
#[derive(Clone, Default, Deserialize)]
pub struct Request {
    pub from: Option<String>,
    pub to: Option<String>,
    pub tz: Option<String>,
    #[serde(default)]
    pub group_by: Group,
    #[serde(default)]
    pub release_group: ReleaseGroup,
    pub week_start: Option<i32>,
    pub limit: Option<usize>,
}
#[derive(Serialize)]
pub struct Period {
    pub from: String,
    pub to: String,
    pub tz: String,
    pub group_by: Group,
    pub release_group: ReleaseGroup,
    pub week_start: i32,
    pub limit: usize,
}
struct Range {
    start: OffsetDateTime,
    end: OffsetDateTime,
    zone: String,
    group: Group,
    release: ReleaseGroup,
    week: i32,
    limit: usize,
}
async fn endpoint(
    state: &AppState,
    text: &str,
    zone: &str,
    end: bool,
) -> Result<OffsetDateTime, ApiError> {
    if text.len() > 64 {
        return Err(ApiError::Unprocessable("Invalid statistics date".into()));
    }
    if let Ok(value) = OffsetDateTime::parse(text, &Rfc3339) {
        return Ok(value);
    }
    let date = Date::parse(
        text,
        &time::macros::format_description!("[year]-[month]-[day]"),
    )
    .map_err(|_| ApiError::Unprocessable("Use an ISO date or RFC 3339 timestamp".into()))?;
    let date = if end {
        date.next_day()
            .ok_or_else(|| ApiError::Unprocessable("Statistics date is out of range".into()))?
    } else {
        date
    };
    let value = plamenu_db::tz::local_to_utc(&state.pool, date.midnight(), zone).await?;
    if end {
        value
            .checked_sub(Duration::nanoseconds(1_000))
            .ok_or_else(|| ApiError::Unprocessable("Statistics date is out of range".into()))
    } else {
        Ok(value)
    }
}
async fn range(state: &AppState, input: &Request) -> Result<Range, ApiError> {
    let zone = input
        .tz
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("UTC");
    if zone.len() > 200 || jiff::tz::TimeZone::get(zone).is_err() {
        return Err(ApiError::Unprocessable(
            "Unknown statistics timezone".into(),
        ));
    }
    // The value is bound, never interpolated; accept every supported IANA zone,
    // including aliases outside the native preference dropdown's curated list.
    let known: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_timezone_names WHERE name=$1)")
            .bind(zone)
            .fetch_one(&state.pool)
            .await
            .map_err(DbError::from)?;
    if !known {
        return Err(ApiError::Unprocessable(
            "Unknown statistics timezone".into(),
        ));
    }
    let end = if let Some(text) = input.to.as_deref().filter(|s| !s.is_empty()) {
        endpoint(state, text, zone, true).await?
    } else {
        OffsetDateTime::now_utc()
    };
    let start = if let Some(text) = input.from.as_deref().filter(|s| !s.is_empty()) {
        endpoint(state, text, zone, false).await?
    } else {
        end.checked_sub(Duration::days(30))
            .ok_or_else(|| ApiError::Unprocessable("Statistics date is out of range".into()))?
    };
    let week = input.week_start.unwrap_or(0);
    let limit = input.limit.unwrap_or(10);
    if start > end || !(0..=6).contains(&week) || !(1..=100).contains(&limit) {
        return Err(ApiError::Unprocessable(
            "Invalid statistics range, week start or limit".into(),
        ));
    }
    Ok(Range {
        start,
        end,
        zone: zone.into(),
        group: input.group_by,
        release: input.release_group,
        week,
        limit,
    })
}
#[derive(Clone, Serialize)]
pub struct TopEntry {
    pub key: String,
    pub name: String,
    pub play_count: i64,
    pub url: Option<String>,
    pub image_url: Option<String>,
    pub artist_name: Option<String>,
    pub artist_url: Option<String>,
}
#[derive(Default, Serialize)]
pub struct Top {
    pub tracks: Vec<TopEntry>,
    pub artists: Vec<TopEntry>,
    pub albums: Vec<TopEntry>,
    pub genres: Vec<TopEntry>,
}
#[derive(Serialize)]
pub struct Bucket {
    pub bucket: String,
    pub count: i64,
}
#[derive(Serialize)]
pub struct GenreCount {
    pub name: String,
    pub count: i64,
}
#[derive(Serialize)]
pub struct GenreBucket {
    pub bucket: String,
    pub genres: Vec<GenreCount>,
}
#[derive(Serialize)]
pub struct Hour {
    pub hour: usize,
    pub count: i64,
}
#[derive(Serialize)]
pub struct Report {
    pub period: Period,
    pub top: Top,
    pub plays: Vec<Bucket>,
    pub genres_timeline: Vec<GenreBucket>,
    pub releases: Vec<Bucket>,
    pub clock: Vec<Hour>,
    pub play_count: i64,
}
#[derive(sqlx::FromRow)]
struct SourceRow {
    id: i64,
    asset_id: Option<i64>,
    status_id: Option<i64>,
    remote_id: Option<i64>,
    plays: i64,
}
impl SourceRow {
    fn entry(&self) -> Entry {
        Entry {
            remote_id: self.remote_id,
            episode_id: None,
            id: self.id,
            collection_id: 0,
            asset_id: self.asset_id,
            status_id: self.status_id,
            position: 1,
        }
    }
}
#[derive(sqlx::FromRow)]
struct Point {
    source_id: i64,
    source_kind: i16,
    bucket: Date,
    hour: i32,
    plays: i64,
}
struct Detail {
    track: Playable,
    track_key: String,
    metadata: music::Metadata,
    artist_key: String,
    artist_url: Option<String>,
    album_key: String,
    album_url: Option<String>,
}
#[derive(Default)]
struct Totals {
    tracks: BTreeMap<String, TopEntry>,
    artists: BTreeMap<String, TopEntry>,
    albums: BTreeMap<String, TopEntry>,
    genres: BTreeMap<String, TopEntry>,
    plays: BTreeMap<Date, i64>,
    genre_dates: BTreeMap<Date, BTreeMap<String, i64>>,
    releases: BTreeMap<i32, i64>,
    hours: [i64; 24],
    count: i64,
}
fn genres(raw: &str) -> Vec<String> {
    let mut names = raw
        .split([';', ',', '|', '\t', '\\'])
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}
fn add(map: &mut BTreeMap<String, TopEntry>, key: String, mut entry: TopEntry) {
    entry.key.clone_from(&key);
    if let Some(old) = map.get_mut(&key) {
        old.play_count += entry.play_count;
    } else {
        map.insert(key, entry);
    }
}
fn top(map: BTreeMap<String, TopEntry>, limit: usize) -> Vec<TopEntry> {
    let mut entries = map.into_values().collect::<Vec<_>>();
    entries.sort_by(|a, b| {
        b.play_count
            .cmp(&a.play_count)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.url.cmp(&b.url))
    });
    entries.truncate(limit);
    entries
}
impl Totals {
    fn source(&mut self, detail: &Detail, plays: i64, release: ReleaseGroup) {
        self.count += plays;
        let artist_name =
            (!detail.metadata.artist.is_empty()).then(|| detail.metadata.artist.clone());
        add(
            &mut self.tracks,
            detail.track_key.clone(),
            TopEntry {
                key: String::new(),
                name: detail.track.title.clone(),
                play_count: plays,
                url: Some(detail.track.url.clone()),
                image_url: detail.track.image_url.clone(),
                artist_name: artist_name.clone(),
                artist_url: detail.artist_url.clone(),
            },
        );
        if let Some(name) = artist_name {
            add(
                &mut self.artists,
                detail.artist_key.clone(),
                TopEntry {
                    key: String::new(),
                    name,
                    play_count: plays,
                    url: detail.artist_url.clone(),
                    image_url: None,
                    artist_name: None,
                    artist_url: None,
                },
            );
        }
        if !detail.metadata.album.is_empty() {
            add(
                &mut self.albums,
                detail.album_key.clone(),
                TopEntry {
                    key: String::new(),
                    name: detail.metadata.album.clone(),
                    play_count: plays,
                    url: detail.album_url.clone(),
                    image_url: None,
                    artist_name: Some(detail.metadata.artist.clone()),
                    artist_url: detail.artist_url.clone(),
                },
            );
        }
        for genre in genres(&detail.metadata.genre) {
            let url = format!(
                "/music/assets?{}",
                serde_urlencoded::to_string([("q", genre.as_str())]).unwrap_or_default()
            );
            add(
                &mut self.genres,
                genre.clone(),
                TopEntry {
                    key: String::new(),
                    name: genre,
                    play_count: plays,
                    url: Some(url),
                    image_url: None,
                    artist_name: None,
                    artist_url: None,
                },
            );
        }
        if let Some(year) = detail.metadata.release_year {
            let key = if release == ReleaseGroup::Decade {
                year / 10 * 10
            } else {
                year
            };
            *self.releases.entry(key).or_default() += plays;
        }
    }
    fn point(&mut self, detail: &Detail, point: &Point) -> Result<(), ApiError> {
        let hour = usize::try_from(point.hour)
            .ok()
            .filter(|v| *v < 24)
            .ok_or_else(|| ApiError::Unprocessable("Invalid statistics clock hour".into()))?;
        self.hours[hour] += point.plays;
        *self.plays.entry(point.bucket).or_default() += point.plays;
        let names = self.genre_dates.entry(point.bucket).or_default();
        for genre in genres(&detail.metadata.genre) {
            *names.entry(genre).or_default() += point.plays;
        }
        Ok(())
    }
}
fn asset_detail(asset: &music_assets::Asset, mut track: Playable) -> Detail {
    track.title.clone_from(&asset.title);
    track.artist.clone_from(&asset.artist);
    track.url = format!("/music/assets/{}", asset.id);
    track.stream_url = format!("/music/assets/{}/stream", asset.id);
    track.image_url = asset.artwork_id.map(|id| format!("/music/artwork/{id}"));
    Detail {
        track,
        track_key: format!("asset:{}", asset.id),
        metadata: asset.metadata(),
        artist_key: format!("artist:{}", asset.artist_id),
        artist_url: Some(format!("/music/artists/{}", asset.artist_id)),
        album_key: format!("album:{}", asset.album_id.unwrap_or_default()),
        album_url: asset.album_id.map(|id| format!("/music/albums/{id}")),
    }
}
async fn details(
    state: &AppState,
    rows: &[SourceRow],
    owner: i64,
) -> Result<HashMap<(i16, i64), Detail>, ApiError> {
    let mut result = local_details(state, rows, owner).await?;
    let ids = rows
        .iter()
        .filter_map(|row| row.remote_id)
        .collect::<Vec<_>>();
    let resources = crate::music_remote::for_ids(state, &ids, owner).await?;
    let entries = rows
        .iter()
        .filter(|row| row.remote_id.is_some())
        .map(SourceRow::entry)
        .collect::<Vec<_>>();
    let playable = music_collections::sources(state, &entries, Some(owner))
        .await?
        .into_iter()
        .map(|item| (item.entry_id.clone(), item))
        .collect::<HashMap<_, _>>();
    let uris = resources
        .iter()
        .flat_map(|resource| {
            [
                resource.parent_uri.clone(),
                resource.details.artist_uri.clone(),
            ]
        })
        .flatten()
        .collect::<Vec<_>>();
    let credits = crate::music_remote::for_uris_pool(&state.pool, &uris, Some(owner))
        .await?
        .into_iter()
        .map(|row| (row.uri.clone(), row))
        .collect::<HashMap<_, _>>();
    for resource in resources {
        let Some(row) = rows.iter().find(|row| row.remote_id == Some(resource.id)) else {
            continue;
        };
        let Some(track) = playable.get(&row.id.to_string()).cloned() else {
            continue;
        };
        result.insert((3, resource.id), remote_detail(&resource, track, &credits));
    }
    Ok(result)
}
fn remote_detail(
    resource: &crate::music_remote::Resource,
    track: Playable,
    credits: &HashMap<String, crate::music_remote::Resource>,
) -> Detail {
    let data = &resource.details;
    let namespace = format!("remote:{}", resource.controller_id);
    let artist = data
        .artist_uri
        .as_ref()
        .and_then(|uri| credits.get(uri))
        .filter(|row| row.kind == "artist");
    let album = resource
        .parent_uri
        .as_ref()
        .and_then(|uri| credits.get(uri))
        .filter(|row| row.kind == "album");
    Detail {
        track,
        track_key: format!("remote:{}", resource.uri),
        metadata: music::Metadata {
            artist: data.artist.clone(),
            album: data.album.clone(),
            genre: data.genre.clone(),
            release_year: data.release_year,
            track_number: data.track_number,
            disc_number: data.disc_number,
            musicbrainz_id: data.musicbrainz_id.clone(),
        },
        artist_key: data
            .artist_uri
            .clone()
            .unwrap_or_else(|| format!("{namespace}:artist:{}", data.artist)),
        artist_url: artist.map(|row| format!("/music/remote/{}", row.id)),
        album_key: resource
            .parent_uri
            .clone()
            .unwrap_or_else(|| format!("{namespace}:album:{}", data.album)),
        album_url: album.map(|row| format!("/music/remote/{}", row.id)),
    }
}

async fn local_details(
    state: &AppState,
    rows: &[SourceRow],
    owner: i64,
) -> Result<HashMap<(i16, i64), Detail>, ApiError> {
    let entries = rows.iter().map(SourceRow::entry).collect::<Vec<_>>();
    let playable = music_collections::sources(state, &entries, Some(owner))
        .await?
        .into_iter()
        .map(|p| (p.entry_id.clone(), p))
        .collect::<HashMap<_, _>>();
    let status_ids = rows
        .iter()
        .filter(|r| playable.contains_key(&r.id.to_string()))
        .filter_map(|r| r.status_id)
        .collect::<Vec<_>>();
    let aliases: Vec<(i64, i64)> =
        sqlx::query_as("SELECT status_id,asset_id FROM music_publications WHERE status_id=ANY($1)")
            .bind(&status_ids)
            .fetch_all(&state.pool)
            .await
            .map_err(DbError::from)?;
    let aliases = aliases.into_iter().collect::<HashMap<_, _>>();
    let mut asset_ids = rows
        .iter()
        .filter(|r| playable.contains_key(&r.id.to_string()))
        .filter_map(|r| r.asset_id)
        .chain(aliases.values().copied())
        .collect::<Vec<_>>();
    asset_ids.sort_unstable();
    asset_ids.dedup();
    let assets = music_assets::for_ids(state, &asset_ids, Some(owner))
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect::<HashMap<_, _>>();
    let metadata = music::for_statuses(&state.pool, &status_ids).await?;
    let statuses = plamenu_db::status::find_by_ids(&state.pool, &status_ids)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect::<HashMap<_, _>>();
    let mut result = HashMap::new();
    for row in rows {
        let Some(track) = playable.get(&row.id.to_string()).cloned() else {
            continue;
        };
        if let Some(asset) = row.asset_id.and_then(|id| assets.get(&id)) {
            result.insert((1, asset.id), asset_detail(asset, track));
        } else if let Some(id) = row.status_id {
            if let Some(asset) = aliases.get(&id).and_then(|id| assets.get(id)) {
                // Fold only a verified explicit publication edge; saving or
                // matching a remote artist/title never creates local ownership.
                result.insert((2, id), asset_detail(asset, track));
                continue;
            }
            let m = metadata.get(&id).cloned().unwrap_or_default();
            let namespace = statuses
                .get(&id)
                .map_or_else(|| track.url.clone(), |s| format!("author:{}", s.account_id));
            result.insert(
                (2, id),
                Detail {
                    track,
                    track_key: format!("status:{id}"),
                    artist_key: format!("{namespace}:artist:{}", m.artist),
                    artist_url: None,
                    album_key: format!("{namespace}:artist:{}:album:{}", m.artist, m.album),
                    album_url: None,
                    metadata: m,
                },
            );
        }
    }
    Ok(result)
}

async fn points(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    range: &Range,
    details: &HashMap<(i16, i64), Detail>,
    totals: &mut Totals,
) -> Result<(), ApiError> {
    let assets = details
        .keys()
        .filter_map(|(kind, id)| (*kind == 1).then_some(*id))
        .collect::<Vec<_>>();
    let statuses = details
        .keys()
        .filter_map(|(kind, id)| (*kind == 2).then_some(*id))
        .collect::<Vec<_>>();
    let remotes = details
        .keys()
        .filter_map(|(kind, id)| (*kind == 3).then_some(*id))
        .collect::<Vec<_>>();
    if details.is_empty() {
        return Ok(());
    }
    let mut rows=sqlx::query_as::<_,Point>("SELECT source_id,source_kind,CASE WHEN $7='week' THEN (local_time::date - ((EXTRACT(isodow FROM local_time)::integer-1-$8+7)%7)) ELSE date_trunc($7,local_time)::date END AS bucket,EXTRACT(hour FROM local_time)::integer AS hour,count(*) AS plays FROM (SELECT COALESCE(asset_id,status_id,remote_id) AS source_id,CASE WHEN asset_id IS NOT NULL THEN 1::smallint WHEN status_id IS NOT NULL THEN 2::smallint ELSE 3::smallint END AS source_kind,counted_at AT TIME ZONE $6 AS local_time FROM music_listen_sessions WHERE account_id=$1 AND counted_at >= $2 AND counted_at <= $3 AND (asset_id=ANY($4) OR status_id=ANY($5) OR remote_id=ANY($9))) source GROUP BY source_id,source_kind,bucket,hour")
        .bind(owner).bind(range.start).bind(range.end).bind(assets).bind(statuses).bind(&range.zone).bind(range.group.sql()).bind(range.week).bind(remotes).fetch(conn);
    while let Some(point) = rows.try_next().await.map_err(DbError::from)? {
        if let Some(item) = details.get(&(point.source_kind, point.source_id)) {
            totals.point(item, &point)?;
        }
    }
    Ok(())
}
async fn images(state: &AppState, top: &mut Top) -> Result<(), ApiError> {
    let parse = |url: Option<&str>, kind: &str| {
        url.and_then(|u| u.strip_prefix(&format!("/music/{kind}/")))
            .and_then(|id| id.parse::<i64>().ok())
    };
    let artists = top
        .artists
        .iter()
        .filter_map(|e| parse(e.url.as_deref(), "artists"))
        .collect::<Vec<_>>();
    let albums = top
        .albums
        .iter()
        .filter_map(|e| parse(e.url.as_deref(), "albums"))
        .collect::<Vec<_>>();
    if artists.is_empty() && albums.is_empty() {
        return Ok(());
    }
    let artwork:Vec<(String,i64,i64)>=sqlx::query_as("SELECT DISTINCT ON(target_kind,target_id) target_kind,target_id,id FROM music_artworks a WHERE ((target_kind='artists' AND target_id=ANY($1)) OR (target_kind='albums' AND target_id=ANY($2))) AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=a.file_name) ORDER BY target_kind,target_id,CASE WHEN (target_kind='albums' AND slot='cover') OR (target_kind='artists' AND slot='image') THEN 0 ELSE 1 END").bind(&artists).bind(&albums).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let images = artwork
        .into_iter()
        .map(|(kind, target, id)| ((kind, target), format!("/music/artwork/{id}")))
        .collect::<HashMap<_, _>>();
    // These IDs came exclusively from currently readable source tracks. Protected
    // serving still checks the current parent ACL for every image request.
    for (kind, entries) in [("artists", &mut top.artists), ("albums", &mut top.albums)] {
        for entry in entries {
            if let Some(id) = parse(entry.url.as_deref(), kind) {
                entry.image_url = images.get(&(kind.to_owned(), id)).cloned();
            }
        }
    }
    // Compilations use the album's own artist rather than a played track's artist.
    let album_artists:Vec<(i64,i64,String)>=sqlx::query_as("SELECT a.id,r.id,r.name FROM music_albums a JOIN music_artists r ON r.id=a.artist_id WHERE a.id=ANY($1)").bind(&albums).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let album_artists = album_artists
        .into_iter()
        .map(|(id, artist, name)| (id, (artist, name)))
        .collect::<HashMap<_, _>>();
    for entry in &mut top.albums {
        if let Some((artist, name)) =
            parse(entry.url.as_deref(), "albums").and_then(|id| album_artists.get(&id))
        {
            entry.artist_name = Some(name.clone());
            entry.artist_url = Some(format!("/music/artists/{artist}"));
        }
    }
    Ok(())
}
async fn calendar(state: &AppState, range: &Range) -> Result<Vec<Date>, ApiError> {
    let local =
        plamenu_db::tz::utc_to_local(&state.pool, &[range.start, range.end], &range.zone).await?;
    let mut key = range.group.key(local[0].date(), range.week)?;
    let end = range.group.key(local[1].date(), range.week)?;
    let mut keys = Vec::new();
    while key <= end {
        if keys.len() >= 10_000 {
            return Err(ApiError::Unprocessable(
                "Choose a coarser statistics grouping for this range".into(),
            ));
        }
        keys.push(key);
        if key == end {
            break;
        }
        key = range
            .group
            .next(key)
            .ok_or_else(|| ApiError::Unprocessable("Statistics date is out of range".into()))?;
    }
    Ok(keys)
}
async fn finish(
    state: &AppState,
    range: Range,
    totals: Totals,
    keys: Vec<Date>,
) -> Result<Report, ApiError> {
    let mut plays = Vec::new();
    let mut timeline = Vec::new();
    for key in keys {
        let label = range.group.label(key);
        plays.push(Bucket {
            bucket: label.clone(),
            count: totals.plays.get(&key).copied().unwrap_or_default(),
        });
        let mut genres = totals
            .genre_dates
            .get(&key)
            .map(|m| {
                m.iter()
                    .map(|(name, count)| GenreCount {
                        name: name.clone(),
                        count: *count,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        genres.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        timeline.push(GenreBucket {
            bucket: label,
            genres,
        });
    }
    let mut ranked = Top {
        tracks: top(totals.tracks, range.limit),
        artists: top(totals.artists, range.limit),
        albums: top(totals.albums, range.limit),
        genres: top(totals.genres, range.limit),
    };
    images(state, &mut ranked).await?;
    let releases = totals
        .releases
        .into_iter()
        .map(|(year, count)| Bucket {
            bucket: if range.release == ReleaseGroup::Decade {
                format!("{year}s")
            } else {
                year.to_string()
            },
            count,
        })
        .collect();
    Ok(Report {
        period: Period {
            from: crate::entities::rfc3339(range.start)?,
            to: crate::entities::rfc3339(range.end)?,
            tz: range.zone,
            group_by: range.group,
            release_group: range.release,
            week_start: range.week,
            limit: range.limit,
        },
        top: ranked,
        plays,
        genres_timeline: timeline,
        releases,
        clock: totals
            .hours
            .into_iter()
            .enumerate()
            .map(|(hour, count)| Hour { hour, count })
            .collect(),
        play_count: totals.count,
    })
}
pub async fn report(state: &AppState, owner: i64, input: &Request) -> Result<Report, ApiError> {
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, owner).await?;
    drop(conn);
    let range = range(state, input).await?;
    let keys = calendar(state, &range).await?;
    // The grouped counts and clock/calendar scans share one snapshot. A new
    // report or an erasure cannot produce mismatched totals inside one chart.
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let mut totals = Totals::default();
    let mut cursor = None;
    loop {
        let rows:Vec<SourceRow>=sqlx::query_as("SELECT min(id) AS id,asset_id,status_id,remote_id,count(*) AS plays FROM music_listen_sessions WHERE account_id=$1 AND counted_at >= $2 AND counted_at <= $3 AND episode_id IS NULL GROUP BY asset_id,status_id,remote_id HAVING ($4::bigint IS NULL OR min(id)<$4) ORDER BY min(id) DESC LIMIT 100").bind(owner).bind(range.start).bind(range.end).bind(cursor).fetch_all(&mut *tx).await.map_err(DbError::from)?;
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|r| r.id);
        let detail = details(state, &rows, owner).await?;
        for row in &rows {
            if let Some(item) = detail.get(&(
                if row.asset_id.is_some() {
                    1
                } else if row.status_id.is_some() {
                    2
                } else {
                    3
                },
                row.asset_id
                    .or(row.status_id)
                    .or(row.remote_id)
                    .unwrap_or_default(),
            )) {
                totals.source(item, row.plays, range.release);
            }
        }
        points(&mut tx, owner, &range, &detail, &mut totals).await?;
        if rows.len() < 100 {
            break;
        }
    }
    tx.commit().await.map_err(DbError::from)?;
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, owner).await?;
    drop(conn);
    finish(state, range, totals, keys).await
}
