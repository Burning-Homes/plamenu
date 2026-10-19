//! Durable identities, useful-work accounting and bounded acquisition backlogs.
use super::{PgPool, Tx};
use crate::DbError;

/// Called while holding the settings lock, before selecting any verification
/// quota. A known unchanged identity requires no per-object network work.
pub(super) async fn new_candidate(tx: &mut Tx<'_>, uri: &str) -> Result<bool, DbError> {
    let (canonical, outcome, queued, changed): (String, Option<String>, bool, bool) = sqlx::query_as(
        "SELECT COALESCE(a.canonical_uri,$1),i.outcome,
          EXISTS(SELECT 1 FROM discovery_jobs j WHERE j.target_uri IN ($1,COALESCE(a.canonical_uri,$1)))
          OR EXISTS(SELECT 1 FROM discovery_candidates c WHERE c.uri=$1),
          COALESCE(i.next_check_at<=now() AND ((i.hint_changed AND i.outcome='active') OR i.outcome='error'),false)
         FROM (SELECT 1) x LEFT JOIN discovery_aliases a ON a.alias=$1
         LEFT JOIN discovery_identity i ON i.canonical_uri=COALESCE(a.canonical_uri,$1)")
        .bind(uri).fetch_one(&mut **tx).await?;
    let known = outcome.as_deref().is_some_and(|s| s != "pending") && !changed;
    sqlx::query("SELECT discovery_count($1,'candidate','acquisition','hint')")
        .bind(super::address(uri)?.1)
        .execute(&mut **tx)
        .await?;

    if known || queued {
        sqlx::query(
            "UPDATE discovery_identity SET last_encountered_at=now(),
          forget_after=CASE WHEN forget_after IS NULL THEN NULL ELSE now()+interval '365 days' END
          WHERE canonical_uri=$1",
        )
        .bind(&canonical)
        .execute(&mut **tx)
        .await?;
        let origin = super::address(uri)?.1;
        sqlx::query("SELECT discovery_count($1,'candidate','acquisition',$2)")
            .bind(origin)
            .bind(if queued {
                "queued_skip"
            } else {
                "duplicate_skip"
            })
            .execute(&mut **tx)
            .await?;
    }
    Ok(!known && !queued)
}

pub(super) async fn defer_candidate(
    tx: &mut Tx<'_>,
    source: i64,
    uri: &str,
    cursor: &str,
    video: Option<serde_json::Value>,
) -> Result<(), DbError> {
    let inserted = sqlx::query(
        "INSERT INTO discovery_candidates(uri,source_id,cursor,video)
      SELECT $1,$2,$3,$4 WHERE (SELECT count(*) FROM discovery_candidates)<2000
      ON CONFLICT DO NOTHING",
    )
    .bind(uri)
    .bind(source)
    .bind(cursor)
    .bind(video)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if inserted == 0
        && !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM discovery_candidates WHERE uri=$1)",
        )
        .bind(uri)
        .fetch_one(&mut **tx)
        .await?
    {
        return Err(super::invalid("discovery candidate backlog full"));
    }
    sqlx::query("SELECT discovery_count(origin,'candidate','acquisition','capacity_deferred') FROM discovery_sources WHERE id=$1")
        .bind(source).execute(&mut **tx).await?;
    Ok(())
}
pub async fn schedule_candidates(pool: &PgPool) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = super::lock_settings(&mut tx).await?;
    if !settings.runs("metadata_cache") {
        return Ok(());
    }
    sqlx::query("SELECT discovery_park_unverified(false)")
        .execute(&mut *tx)
        .await?;
    schedule_source_hints(&mut tx, &settings).await?;
    let rows: Vec<(String,i64,String,Option<serde_json::Value>,Option<i64>)>=sqlx::query_as(
        "SELECT c.uri,c.source_id,c.cursor,c.video,s.introduced_by FROM discovery_candidates c
         JOIN discovery_sources s ON s.id=c.source_id WHERE s.enabled ORDER BY c.created_at LIMIT 32")
        .fetch_all(&mut *tx).await?;
    for (uri, source, cursor, video, introducer) in rows {
        match super::queue::enqueue_locked(
            &mut tx,
            &settings,
            source,
            &uri,
            super::Operation::RefreshEntry,
            introducer,
        )
        .await?
        {
            super::Admission::Added(id) => {
                sqlx::query("UPDATE discovery_jobs SET cursor=NULLIF($2,'') WHERE id=$1")
                    .bind(id)
                    .bind(cursor)
                    .execute(&mut *tx)
                    .await?;
                if let Some(v) = video {
                    sqlx::query("INSERT INTO discovery_video_work(job_id,uuid,owner_uri,channel_uri) VALUES($1,$2,$3,$4)")
                        .bind(id).bind(v["uuid"].as_str()).bind(v["owner_uri"].as_str()).bind(v["channel_uri"].as_str()).execute(&mut *tx).await?;
                }
            }
            super::Admission::Capacity | super::Admission::Disabled => continue,
            _ => {}
        }
        sqlx::query("DELETE FROM discovery_candidates WHERE uri=$1")
            .bind(uri)
            .execute(&mut *tx)
            .await?;
    }
    // Bounded housekeeping; guards have no forget_after and cannot be evicted.
    sqlx::query("DELETE FROM discovery_identity WHERE canonical_uri IN (SELECT canonical_uri FROM discovery_identity WHERE forget_after<now() LIMIT 200)").execute(&mut *tx).await?;
    sqlx::query("DELETE FROM discovery_daily WHERE day<current_date-90")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM discovery_seen WHERE seen_at<now()-interval '90 days'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM discovery_actor_evidence WHERE verified_at<now()-interval '14 days'")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn remember_seen(
    pool: &PgPool,
    viewer: i64,
    ids: &[i64],
    impression: bool,
) -> Result<(), DbError> {
    if ids.len() > 50 {
        return Err(super::invalid("too many discovery impressions"));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR UPDATE")
        .bind(viewer)
        .execute(&mut *tx)
        .await?;
    if impression {
        sqlx::query("SELECT discovery_count(s.origin,e.kind,'browsing',CASE WHEN seen.entry_id IS NULL THEN 'impression_new' ELSE 'impression_repeat' END,count(*))
            FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
            LEFT JOIN discovery_seen seen ON seen.entry_id=e.id AND seen.account_id=$1 AND seen.seen_at>now()-interval '90 days'
            WHERE e.id=ANY($2) AND e.state='active' GROUP BY s.origin,e.kind,seen.entry_id IS NULL")
            .bind(viewer).bind(ids).execute(&mut *tx).await?;
    }

    sqlx::query("INSERT INTO discovery_seen(account_id,entry_id) SELECT $1,id FROM discovery_entries WHERE id=ANY($2) AND state='active' ON CONFLICT(account_id,entry_id) DO UPDATE SET seen_at=now()")
        .bind(viewer).bind(ids).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM discovery_seen WHERE account_id=$1 AND entry_id IN (SELECT entry_id FROM discovery_seen WHERE account_id=$1 ORDER BY seen_at DESC OFFSET 10000)")
        .bind(viewer).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn reset_seen(pool: &PgPool, viewer: i64) -> Result<(), DbError> {
    sqlx::query("DELETE FROM discovery_seen WHERE account_id=$1")
        .bind(viewer)
        .execute(pool)
        .await?;
    Ok(())
}
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct RecoveryMetric {
    pub day: time::Date,
    pub origin: String,
    pub kind: String,
    pub purpose: String,
    pub event: String,
    pub total: i64,
}
pub async fn recovery_metrics(pool: &PgPool) -> Result<Vec<RecoveryMetric>, DbError> {
    Ok(sqlx::query_as("SELECT day,origin,kind,purpose,event,total FROM discovery_daily ORDER BY day DESC,total DESC LIMIT 1000").fetch_all(pool).await?)
}

#[derive(Default, serde::Serialize)]
pub struct DiscoveryOptions<'a> {
    pub kind: &'a str,
    pub sort: &'a str,
    pub language: &'a str,
    pub term: &'a str,
    pub topic: &'a str,
    pub source: Option<i64>,
    pub channel: &'a str,
    pub show_seen: bool,
}
/// The candidate pool and its ranking are bounded. Store the resulting order
/// for two hours, so marking a card seen cannot cause pagination skips/repeats.
pub async fn discovery_journey(
    pool: &PgPool,
    viewer: i64,
    journey: Option<i64>,
    seed: i64,
    options: &DiscoveryOptions<'_>,
) -> Result<(i64, Vec<i64>), DbError> {
    let context = serde_json::to_string(options).map_err(|e| super::invalid(&e.to_string()))?;
    if let Some(id) = journey {
        let entries:Option<Vec<i64>>=sqlx::query_scalar("SELECT entries FROM discovery_journeys WHERE id=$1 AND account_id=$2 AND context=$3 AND created_at>now()-interval '2 hours'")
            .bind(id).bind(viewer).bind(context).fetch_optional(pool).await?;
        return entries
            .map(|e| (id, e))
            .ok_or_else(|| super::invalid("Discovery page expired; reopen Discover"));
    }
    let order = match options.sort {
        "published" => "e.published_at DESC NULLS LAST,e.id DESC",
        "surprise" => "hashtextextended(e.canonical_uri,0)",
        _ => "e.id DESC",
    };
    let threshold = if options.sort == "surprise" {
        "AND hashtextextended(e.canonical_uri,0)>=hashtextextended(($10::bigint)::text,0)"
    } else {
        ""
    };
    let query=format!("WITH pool AS MATERIALIZED (
       SELECT e.id,e.actor_uri,e.source_id,e.published_at,e.summary,e.title,
       COALESCE(i.activity_at,(SELECT max(p.published_at) FROM discovery_entries p WHERE p.actor_uri=e.canonical_uri AND p.state='active')) AS activity_at
       FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
       LEFT JOIN discovery_identity i ON i.canonical_uri=e.canonical_uri
       WHERE e.kind=$2 AND e.state='active' AND e.valid_until>now() AND s.enabled
       AND ($3::bigint IS NULL OR e.source_id=$3)
       AND ($4='' OR lower(COALESCE(NULLIF(e.language,''),'unknown'))=lower($4) OR e.language ILIKE $4 || '-%')
       AND ($5='' OR to_tsvector('simple',coalesce(e.title,'') || ' ' || coalesce(e.summary,'')) @@ plainto_tsquery('simple',$5) OR EXISTS(SELECT 1 FROM discovery_topics t WHERE t.entry_id=e.id AND t.topic=lower(ltrim($5,'#'))) OR EXISTS(SELECT 1 FROM discovery_actor_labels l WHERE l.uri=e.actor_uri AND (l.username ILIKE '%' || $5 || '%' OR l.username || '@' || split_part(split_part(l.uri,'://',2),'/',1)=ltrim($5,'@!'))))
       AND ($6='' OR EXISTS(SELECT 1 FROM discovery_topics t WHERE t.entry_id=e.id AND t.topic=$6))
       AND ($7='' OR EXISTS(SELECT 1 FROM discovery_video_details v WHERE v.entry_id=e.id AND v.channel_uri=$7))
       AND ($8 OR $9<>'new' OR NOT EXISTS(SELECT 1 FROM discovery_seen seen WHERE seen.entry_id=e.id AND seen.account_id=$1 AND seen.seen_at>now()-interval '90 days'))
       AND NOT EXISTS(SELECT 1 FROM discovery_dismissals d WHERE d.entry_id=e.id AND d.account_id=$1)
       {threshold} ORDER BY {order} LIMIT 2000
     ), diversity AS (
       SELECT *,row_number() OVER(PARTITION BY source_id ORDER BY id DESC) AS server_round,
       row_number() OVER(PARTITION BY COALESCE(actor_uri,id::text) ORDER BY id DESC) AS author_round,
       row_number() OVER(PARTITION BY md5(regexp_replace(lower(COALESCE(summary,title,'')),'[0-9[:space:]]+',' ','g')) ORDER BY id DESC) AS text_round
       FROM pool
     ) SELECT id FROM diversity ORDER BY
       CASE WHEN $9='new' THEN CASE WHEN $2='post' AND published_at<now()-interval '30 days' OR $2='video' AND published_at<now()-interval '90 days' THEN 1 ELSE 0 END ELSE 0 END,
       CASE WHEN $9='new' THEN GREATEST(server_round/4,author_round/2,text_round-1) ELSE 0 END,
       CASE WHEN $9='surprise' THEN hashtextextended(id::text,$10) ELSE 0 END,
       CASE WHEN $9='active' THEN activity_at END DESC NULLS LAST,
       CASE WHEN $9='published' OR ($9='new' AND $2 IN ('post','video')) THEN published_at END DESC NULLS LAST,
       id DESC");
    // SQL fragments above are selected only from fixed literals; every user
    // value (including sort, seed and filters) is bound as a parameter.
    let mut entries: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(query.as_str()))
        .bind(viewer)
        .bind(options.kind)
        .bind(options.source)
        .bind(options.language)
        .bind(options.term)
        .bind(options.topic)
        .bind(options.channel)
        .bind(options.show_seen)
        .bind(options.sort)
        .bind(seed)
        .fetch_all(pool)
        .await?;
    if options.sort == "surprise" && entries.len() < 2000 {
        let wrap = query.replace(
            threshold,
            "AND hashtextextended(e.canonical_uri,0)<hashtextextended(($10::bigint)::text,0)",
        );
        let more: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(wrap.as_str()))
            .bind(viewer)
            .bind(options.kind)
            .bind(options.source)
            .bind(options.language)
            .bind(options.term)
            .bind(options.topic)
            .bind(options.channel)
            .bind(options.show_seen)
            .bind(options.sort)
            .bind(seed)
            .fetch_all(pool)
            .await?;
        entries.extend(more.into_iter().take(2000 - entries.len()));
    }
    let id = crate::id::next();
    let mut tx = pool.begin().await?;
    // Serialize this account's bounded session cache, including multiple tabs.
    sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR UPDATE")
        .bind(viewer)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM discovery_journeys WHERE created_at<now()-interval '2 hours' OR (account_id=$1 AND id IN (SELECT id FROM discovery_journeys WHERE account_id=$1 ORDER BY created_at DESC OFFSET 7))")
        .bind(viewer).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO discovery_journeys(id,account_id,context,entries) VALUES($1,$2,$3,$4)",
    )
    .bind(id)
    .bind(viewer)
    .bind(context)
    .bind(&entries)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((id, entries))
}

pub async fn actor_evidence(
    pool: &PgPool,
    uri: &str,
) -> Result<Option<serde_json::Value>, DbError> {
    Ok(sqlx::query_scalar("SELECT document FROM discovery_actor_evidence WHERE uri=$1 AND verified_at>now()-interval '14 days'")
        .bind(uri).fetch_optional(pool).await?)
}
pub async fn save_actor_evidence(
    pool: &PgPool,
    lease: &super::Lease,
    uri: &str,
    document: &serde_json::Value,
) -> Result<(), DbError> {
    if let Some(username) = document
        .get("preferredUsername")
        .and_then(serde_json::Value::as_str)
    {
        super::save_actor_label(pool, lease, uri, username).await?;
    }
    let mut tx = pool.begin().await?;
    let settings = super::lock_settings(&mut tx).await?;
    if !super::valid_lease(&mut tx, &settings, lease).await? {
        return Ok(());
    }
    let mut bounded = serde_json::Map::new();
    for key in [
        "id",
        "type",
        "attributedTo",
        "discoverable",
        "indexable",
        "localOnly",
        "suspended",
        "movedTo",
        "visibility",
        "to",
        "cc",
        "name",
        "preferredUsername",
    ] {
        if let Some(value) = document.get(key) {
            bounded.insert(key.into(), value.clone());
        }
    }
    let value = serde_json::Value::Object(bounded);
    if value.to_string().len() <= 8192 {
        sqlx::query("INSERT INTO discovery_actor_evidence(uri,document) SELECT $1,$2 WHERE (SELECT count(*) FROM discovery_actor_evidence)<10000 OR EXISTS(SELECT 1 FROM discovery_actor_evidence WHERE uri=$1) ON CONFLICT(uri) DO UPDATE SET document=excluded.document,verified_at=now()")
            .bind(uri).bind(value).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn outbox_continuation(pool: &PgPool, actor: &str) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar::<_,Option<String>>("SELECT CASE WHEN rounds%5=0 THEN NULL ELSE next_uri END FROM discovery_outbox_walk WHERE actor_uri=$1")
        .bind(actor).fetch_optional(pool).await?.flatten())
}
pub async fn save_outbox_continuation(
    pool: &PgPool,
    lease: &super::Lease,
    next: Option<&str>,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = super::lock_settings(&mut tx).await?;
    if !super::valid_lease(&mut tx, &settings, lease).await? {
        return Ok(());
    }
    sqlx::query("INSERT INTO discovery_outbox_walk(actor_uri,next_uri,rounds) VALUES($1,$2,1)
        ON CONFLICT(actor_uri) DO UPDATE SET next_uri=excluded.next_uri,rounds=discovery_outbox_walk.rounds+1")
        .bind(&lease.target_uri).bind(next).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct Inventory {
    pub kind: String,
    pub retained: i64,
    pub active: i64,
    pub sources: i64,
}
pub async fn recovery_inventory(pool: &PgPool) -> Result<Vec<Inventory>, DbError> {
    Ok(sqlx::query_as("SELECT e.kind,count(*) retained,count(*) FILTER(WHERE e.state='active' AND e.valid_until>now() AND s.enabled) active,count(DISTINCT e.source_id) sources FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id GROUP BY e.kind ORDER BY e.kind").fetch_all(pool).await?)
}

/// Advertised timestamps nominate maintenance only after the existing check
/// interval. They never supply consent or reset retention/publication dates.
pub async fn observe_versions(
    pool: &PgPool,
    lease: &super::Lease,
    hints: &[(String, String)],
) -> Result<(), DbError> {
    if hints.len() > 32 {
        return Err(super::invalid("too many version hints"));
    }
    let mut tx = pool.begin().await?;
    let s = super::lock_settings(&mut tx).await?;
    if !super::valid_lease(&mut tx, &s, lease).await? {
        return Ok(());
    }
    for (uri, version) in hints {
        if version.len() > 128
            || version.contains('\0')
            || super::address(uri)?.1 != super::address(&lease.target_uri)?.1
        {
            continue;
        }
        sqlx::query("INSERT INTO discovery_identity(canonical_uri,kind,source_id,advertised_version,forget_after)
          VALUES($1,'candidate',$3,$2,now()+interval '365 days')
          ON CONFLICT(canonical_uri) DO UPDATE SET hint_changed=discovery_identity.hint_changed OR (discovery_identity.outcome='active' AND discovery_identity.advertised_version IS NOT NULL AND discovery_identity.advertised_version<>excluded.advertised_version),advertised_version=excluded.advertised_version")
            .bind(uri).bind(version).bind(lease.source_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub(super) async fn schedule_source_hints(
    tx: &mut Tx<'_>,
    settings: &super::Settings,
) -> Result<(), DbError> {
    if !settings.runs("peer_exploration") {
        return Ok(());
    }
    let endpoints:Vec<(String,Option<i64>)>=sqlx::query_as("SELECT endpoint,introduced_by FROM discovery_source_hints
        WHERE next_at<=now() AND (SELECT count(*) FROM discovery_sources WHERE verified_at IS NULL)<8
        ORDER BY created_at LIMIT 2").fetch_all(&mut **tx).await?;
    for (endpoint, parent) in endpoints {
        match super::sources::add_source_locked(tx, settings, &endpoint, parent, "peer").await? {
            super::Admission::Added(id) => {
                super::queue::enqueue_locked(
                    tx,
                    settings,
                    id,
                    &endpoint,
                    super::Operation::InspectSource,
                    parent,
                )
                .await?;
            }
            super::Admission::Existing(_) => {}
            _ => {
                sqlx::query("UPDATE discovery_source_hints SET next_at=now()+interval '1 day' WHERE endpoint=$1").bind(&endpoint).execute(&mut **tx).await?;
                continue;
            }
        }
        sqlx::query("DELETE FROM discovery_source_hints WHERE endpoint=$1")
            .bind(endpoint)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
