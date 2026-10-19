use super::{
    Completion, Lease, PgPool, address, bounded, delete_claim, evidence_times, invalid,
    lock_settings, valid_lease,
};
use crate::{DbError, id};
use time::OffsetDateTime;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Entry {
    pub outbox_uri: Option<String>,
    pub outbox_round: i64,
    pub outbox_next_at: Option<OffsetDateTime>,
    pub published_at: Option<OffsetDateTime>,
    pub sensitive: bool,
    pub browse_ordinal: i64,
    pub creator_ordinal: i64,
    pub id: i64,
    pub canonical_uri: String,
    pub source_id: i64,
    pub kind: String,
    pub actor_uri: Option<String>,
    pub account_id: Option<i64>,
    pub state: String,
    pub revision: i64,
    pub policy_revision: i64,
    pub discoverable: String,
    pub indexable: String,
    pub visibility: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub language: Option<String>,
    pub verified_at: OffsetDateTime,
    pub valid_until: OffsetDateTime,
    pub invalidation_reason: Option<String>,
    pub updated_at: OffsetDateTime,
}

/// Raw internal record, including expired/withdrawn evidence. Never render it
/// directly: reevaluate current settings, source state, access and per-use policy.
pub async fn entry(pool: &PgPool, uri: &str) -> Result<Option<Entry>, DbError> {
    let (uri, _) = address(uri)?;
    Ok(sqlx::query_as!(
        Entry,
        "SELECT * FROM discovery_entries WHERE canonical_uri=$1",
        uri
    )
    .fetch_optional(pool)
    .await?)
}

pub struct VerifiedEntry<'a> {
    pub outbox_uri: Option<&'a str>,
    pub published_at: Option<OffsetDateTime>,
    pub sensitive: bool,
    /// A post commit must still match the canonical actor evidence it used.
    pub author_revision: Option<i64>,
    pub kind: &'a str,
    pub actor_uri: &'a str,
    pub account_id: Option<i64>,
    pub discoverable: &'a str,
    pub indexable: &'a str,
    pub visibility: &'a str,
    pub title: &'a str,
    pub summary: &'a str,
    pub language: Option<&'a str>,
    pub verified_at: OffsetDateTime,
    pub valid_until: OffsetDateTime,
}

impl VerifiedEntry<'_> {
    fn metadata_allowed(&self, settings: &super::Settings) -> bool {
        self.visibility == "public"
            && match self.kind {
                "profile" | "community" => {
                    permitted(self.discoverable, settings.allow_missing_discoverable)
                }
                "post" | "video" => permitted(self.indexable, settings.allow_missing_indexable),
                _ => true,
            }
    }
}

pub(super) fn permitted(signal: &str, missing: bool) -> bool {
    signal == "allow" || (signal == "missing" && missing)
}

fn validate(facts: &VerifiedEntry<'_>) -> Result<String, DbError> {
    let (actor, _) = address(facts.actor_uri)?;
    bounded(facts.title, 1024)?;
    bounded(facts.summary, 8192)?;
    if let Some(language) = facts.language {
        bounded(language, 64)?;
    }
    evidence_times(facts.verified_at, facts.valid_until)?;
    if let Some(uri) = facts.outbox_uri
        && (facts.kind != "profile" || address(uri)?.1 != address(facts.actor_uri)?.1)
    {
        return Err(invalid("outbox left its canonical actor origin"));
    }
    if !["source", "profile", "post", "community", "video"].contains(&facts.kind)
        || !["missing", "allow", "deny", "invalid"].contains(&facts.discoverable)
        || !["missing", "allow", "deny", "invalid"].contains(&facts.indexable)
        || ![
            "public", "unlisted", "private", "direct", "local", "members", "unknown",
        ]
        .contains(&facts.visibility)
    {
        return Err(invalid("invalid discovery entry facts"));
    }
    Ok(actor)
}

/// One canonical target per refresh lease. Only a caller that has verified
/// identity, attribution and audience may provide these facts. Refusals clear
/// an older payload; policy/source/entry revisions fence older completions.
pub async fn finish_entry(
    pool: &PgPool,
    lease: &Lease,
    facts: &VerifiedEntry<'_>,
) -> Result<Completion, DbError> {
    finish_entry_with_topics(pool, lease, facts, &[]).await
}

/// Canonical explicit tags commit under the same lease and revision fence as
/// the text. Other callers replace earlier tags with an empty set.
pub async fn finish_entry_with_topics(
    pool: &PgPool,
    lease: &Lease,
    facts: &VerifiedEntry<'_>,
    topics: &[String],
) -> Result<Completion, DbError> {
    finish_catalogue_entry(pool, lease, facts, topics, None).await
}

pub async fn finish_video_entry(
    pool: &PgPool,
    lease: &Lease,
    facts: &VerifiedEntry<'_>,
    video: &super::VerifiedVideo<'_>,
    topics: &[String],
) -> Result<Completion, DbError> {
    finish_catalogue_entry(pool, lease, facts, topics, Some(video)).await
}

async fn invalidate_dependents(
    tx: &mut super::Tx<'_>,
    settings: &super::Settings,
    facts: &VerifiedEntry<'_>,
) -> Result<(), DbError> {
    if ["profile", "community"].contains(&facts.kind)
        && (facts.visibility != "public"
            || !permitted(facts.discoverable, settings.allow_missing_discoverable)
            || !permitted(facts.indexable, settings.allow_missing_indexable))
    {
        super::videos::invalidate_video_actor_locked(
            tx,
            facts.actor_uri,
            false,
            "actor_reverified",
        )
        .await?;
    }
    if facts.kind == "profile"
        && (facts.visibility != "public"
            || !permitted(facts.indexable, settings.allow_missing_indexable))
    {
        super::posts::invalidate_author_locked(tx, facts.actor_uri, "author_indexing_refused")
            .await?;
    }
    Ok(())
}

async fn finish_catalogue_entry(
    pool: &PgPool,
    lease: &Lease,
    facts: &VerifiedEntry<'_>,
    topics: &[String],
    video: Option<&super::VerifiedVideo<'_>>,
) -> Result<Completion, DbError> {
    let actor = validate(facts)?;
    if (facts.kind == "video") != video.is_some() {
        return Err(invalid("video evidence is required"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !["refresh_entry", "revalidate_entry"].contains(&lease.operation.as_str())
        || !valid_lease(&mut tx, &settings, lease).await?
        || facts.verified_at
            < lease.leased_until - time::Duration::seconds(i64::from(settings.lease_seconds))
    {
        return Ok(Completion::Stale);
    }
    if let Some(video) = video
        && (facts.actor_uri != video.hint.owner_uri
            || !super::videos::video_current(&mut tx, &settings, lease, video).await?)
    {
        return Ok(Completion::Stale);
    }
    if facts.kind == "post"
        && !super::posts::author_current(&mut tx, &settings, lease.source_id, facts).await?
    {
        return Ok(Completion::Stale);
    }
    let existing = sqlx::query!("SELECT id,revision,state,source_id,kind,verified_at FROM discovery_entries WHERE canonical_uri=$1",lease.target_uri)
        .fetch_optional(&mut *tx).await?;
    if existing.as_ref().map(|e| e.revision) != lease.entry_revision
        || existing.as_ref().is_some_and(|e| {
            (e.state == "withdrawn" && lease.operation != "revalidate_entry")
                || e.source_id != lease.source_id
                || e.kind != facts.kind
                || e.verified_at > facts.verified_at
        })
    {
        return Ok(Completion::Stale);
    }
    if existing.is_none()
        && !super::expansion::entry_room(&mut tx, &settings, lease.source_id).await?
    {
        return Ok(Completion::Capacity);
    }
    let allowed = facts.metadata_allowed(&settings);
    let state = if allowed { "active" } else { "denied" };
    let reason = (!allowed).then_some("ineligible");
    let title = allowed.then_some(facts.title);
    let summary = allowed.then_some(facts.summary);
    let language = if allowed { facts.language } else { None };
    let ordinal = if existing.is_none() {
        sqlx::query_scalar!("UPDATE discovery_sources SET next_ordinal=(SELECT max(s.next_ordinal)+1 FROM discovery_sources s WHERE s.origin=discovery_sources.origin)
            WHERE id=$1 RETURNING next_ordinal",lease.source_id).fetch_one(&mut *tx).await?
    } else {
        0
    };
    let entry_id = existing.map_or_else(id::next, |e| e.id);
    let outbox = facts.outbox_uri.filter(|_| {
        facts.visibility == "public" && permitted(facts.indexable, settings.allow_missing_indexable)
    });
    sqlx::query!(r#"INSERT INTO discovery_entries(id,canonical_uri,source_id,kind,actor_uri,account_id,
        state,policy_revision,discoverable,indexable,visibility,title,summary,language,verified_at,valid_until,invalidation_reason,browse_ordinal,outbox_uri,outbox_next_at,published_at,sensitive)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,CASE WHEN $19::text IS NULL THEN NULL ELSE now() END,$20,$21)
        ON CONFLICT(canonical_uri) DO UPDATE SET actor_uri=excluded.actor_uri,account_id=excluded.account_id,
        state=excluded.state,policy_revision=excluded.policy_revision,discoverable=excluded.discoverable,
        indexable=excluded.indexable,visibility=excluded.visibility,title=excluded.title,summary=excluded.summary,
        language=excluded.language,verified_at=excluded.verified_at,valid_until=excluded.valid_until,
        outbox_uri=excluded.outbox_uri,outbox_next_at=CASE WHEN excluded.outbox_uri IS NULL THEN NULL WHEN discovery_entries.outbox_uri IS DISTINCT FROM excluded.outbox_uri THEN now() ELSE discovery_entries.outbox_next_at END,
        published_at=excluded.published_at,sensitive=excluded.sensitive,
        invalidation_reason=excluded.invalidation_reason,revision=discovery_entries.revision+1,updated_at=now()"#,
        entry_id,lease.target_uri,lease.source_id,facts.kind,actor,facts.account_id,state,settings.revision,
        facts.discoverable,facts.indexable,facts.visibility,title,summary,language,facts.verified_at,facts.valid_until,reason,ordinal,outbox,if allowed {facts.published_at} else {None},allowed && facts.sensitive
    ).execute(&mut *tx).await?;
    if let Some(video) = video {
        super::videos::save_video(&mut tx, entry_id, video, allowed).await?;
    }
    super::topics::replace_topics(&mut tx, entry_id, if allowed { topics } else { &[] }).await?;
    invalidate_dependents(&mut tx, &settings, facts).await?;
    if lease.operation == "revalidate_entry" {
        sqlx::query!(
            "DELETE FROM discovery_jobs WHERE target_uri=$1 AND withdrawn",
            lease.target_uri
        )
        .execute(&mut *tx)
        .await?;
    }
    delete_claim(&mut tx, lease).await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

#[derive(Debug, Clone, Copy)]
pub enum Invalidation {
    Denied,
    Withdrawn,
}

/// Apply verified withdrawal/access evidence, never an unverified third-party
/// hint. Retained guards count toward entry caps and are not silently evicted.
/// Cancelling a pending first fetch also prevents its old lease from inserting.
pub async fn invalidate_entry(
    pool: &PgPool,
    uri: &str,
    state: Invalidation,
    reason: &str,
) -> Result<bool, DbError> {
    let (uri, _) = address(uri)?;
    bounded(reason, 128)?;
    let state = match state {
        Invalidation::Denied => "denied",
        Invalidation::Withdrawn => "withdrawn",
    };
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    super::videos::invalidate_video_actor_locked(&mut tx, &uri, state == "withdrawn", reason)
        .await?;
    super::posts::invalidate_author_locked(&mut tx, &uri, reason).await?;
    let changed = sqlx::query!("UPDATE discovery_entries SET state=CASE WHEN state='withdrawn' THEN state ELSE $2 END,revision=revision+1,title=NULL,summary=NULL,language=NULL,outbox_uri=NULL,outbox_next_at=NULL,published_at=NULL,sensitive=false,invalidation_reason=$3,updated_at=now() WHERE canonical_uri=$1",uri,state,reason)
        .execute(&mut *tx).await?.rows_affected();
    let cancelled = if changed == 0 && state == "withdrawn" {
        sqlx::query!("UPDATE discovery_jobs SET withdrawn=true,search_run_id=NULL,lease_token=NULL,lease_owner=NULL,leased_until=NULL,cursor=NULL,last_error=$2 WHERE target_uri=$1 AND operation='refresh_entry'",uri,reason)
            .execute(&mut *tx).await?.rows_affected()
    } else {
        sqlx::query!("DELETE FROM discovery_jobs WHERE target_uri=$1 AND operation='refresh_entry' AND NOT withdrawn",uri)
            .execute(&mut *tx).await?.rows_affected()
    };
    sqlx::query!(
        "DELETE FROM discovery_jobs WHERE target_uri=$1 AND operation='revalidate_entry'",
        uri
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(changed + cancelled > 0)
}

/// A current canonical fetch learned a refusal/deletion. This shares the same
/// revision fence as positive completion. Verified inbox invalidation instead
/// uses `invalidate_entry` and can operate even while acquisition is paused.
pub async fn finish_entry_refusal(
    pool: &PgPool,
    lease: &Lease,
    withdrawn: bool,
    reason: &str,
) -> Result<Completion, DbError> {
    bounded(reason, 128)?;
    if !["refresh_entry", "revalidate_entry"].contains(&lease.operation.as_str()) {
        return Err(invalid("not a profile verification job"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    let existing = sqlx::query!(
        "SELECT revision,state FROM discovery_entries WHERE canonical_uri=$1",
        lease.target_uri
    )
    .fetch_optional(&mut *tx)
    .await?;
    if existing.as_ref().map(|e| e.revision) != lease.entry_revision {
        return Ok(Completion::Stale);
    }
    super::videos::invalidate_video_actor_locked(&mut tx, &lease.target_uri, withdrawn, reason)
        .await?;
    super::posts::invalidate_author_locked(&mut tx, &lease.target_uri, reason).await?;
    sqlx::query("INSERT INTO discovery_identity(canonical_uri,kind,source_id,outcome,next_check_at)
        VALUES($1,'candidate',$2,$3,now()+interval '90 days') ON CONFLICT(canonical_uri) DO UPDATE SET outcome=CASE WHEN discovery_identity.outcome='withdrawn' THEN 'withdrawn' ELSE excluded.outcome END,next_check_at=excluded.next_check_at,forget_after=NULL")
        .bind(&lease.target_uri).bind(lease.source_id).bind(if withdrawn {"withdrawn"} else {"denied"}).execute(&mut *tx).await?;
    sqlx::query("SELECT discovery_count(origin,'candidate','acquisition',$2) FROM discovery_sources WHERE id=$1")
        .bind(lease.source_id).bind(format!("refused:{reason}")).execute(&mut *tx).await?;
    if existing.is_some() {
        let state = if withdrawn { "withdrawn" } else { "denied" };
        sqlx::query!("UPDATE discovery_entries SET state=CASE WHEN state='withdrawn' THEN state ELSE $2 END,revision=revision+1,title=NULL,summary=NULL,language=NULL,outbox_uri=NULL,outbox_next_at=NULL,published_at=NULL,sensitive=false,invalidation_reason=$3,updated_at=clock_timestamp() WHERE canonical_uri=$1",lease.target_uri,state,reason).execute(&mut *tx).await?;
        delete_claim(&mut tx, lease).await?;
    } else if withdrawn && lease.operation == "refresh_entry" {
        sqlx::query!("UPDATE discovery_jobs SET withdrawn=true,search_run_id=NULL,lease_token=NULL,lease_owner=NULL,leased_until=NULL,cursor=NULL,last_error=$2 WHERE id=$1",lease.id,reason).execute(&mut *tx).await?;
    } else {
        // No profile payload exists to invalidate. Cache endpoint denials in
        // the bounded job instead of suppressing the entire source.
        sqlx::query!("UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL,run_at=clock_timestamp()+interval '24 hours',last_error=$2 WHERE id=$1",lease.id,reason).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Completion::Committed)
}
