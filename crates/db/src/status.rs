//! Status (post) storage: originals, replies and boosts (a boost is a status
//! row with `reblog_of_id`, Mastodon's model).

use std::collections::{HashMap, HashSet};

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::user::TimelineOrder;
use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Status {
    pub id: i64,
    /// `ActivityPub` object id; `None` for local statuses.
    pub uri: Option<String>,
    pub account_id: i64,
    pub content: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// `public` | `unlisted` | `private` | `direct` | `local`.
    pub visibility: String,
    pub in_reply_to_id: Option<i64>,
    pub reblog_of_id: Option<i64>,
    /// Set when an edit (`Update(Note)`) was applied.
    pub edited_at: Option<OffsetDateTime>,
    /// Content warning text; empty when the post has none.
    pub spoiler_text: String,
    /// Marks media (and the post) as sensitive.
    pub sensitive: bool,
    /// ISO 639 language code, when declared.
    pub language: Option<String>,
    /// The human web URL (`url`), distinct from the AP id (`uri`): the remote
    /// origin's published `url` for remote statuses, `None` for local ones
    /// (derived as `/@username/{id}` by the serializers).
    pub url: Option<String>,
    /// Quote interaction policy bitmap (Mastodon's `quote_approval_policy`):
    /// the high 16 bits are the automatic-approval sub-policy, the low 16 the
    /// manual one; `0` means nobody may quote. Decoded by
    /// `plamenu_ap::quote_policy`.
    pub quote_approval_policy: i32,
    /// OAuth application that created this local status, when API-posted.
    pub application_id: Option<i64>,
    /// Inbound object title (`name`), hoisted from any top-level remote
    /// object. `None` for local statuses (composer titles are a future
    /// outbound slice), replies and untitled posts.
    pub title: Option<String>,
    /// The `ActivityStreams` type of a remote object ingested as a non-Note
    /// kind (`Article` | `Page` | `Video` | `Audio` | `Image` | `Event` |
    /// `Document`); `None` for Notes/Questions and local statuses.
    pub object_type: Option<String>,
    /// A link post's target URL (Lemmy `attachment: [{type: Link, href}]`):
    /// what the post is about, distinct from `url` (where the post lives).
    pub external_url: Option<String>,
}

const COLS: &str = "id, uri, account_id, content, created_at, updated_at, visibility, \
                    in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive, \
                    language, url, quote_approval_policy, application_id, title, object_type, external_url";
const _: &str = COLS; // documentation: every query selects exactly these

/// Everything stored for a locally-authored status.
#[derive(Debug)]
pub struct NewLocalStatus<'a> {
    pub account_id: i64,
    pub content: &'a str,
    /// Raw source text as typed (kept for `/source` and the edit composer).
    pub text: &'a str,
    /// The format `text` was authored in (`text/plain` | `text/markdown` |
    /// `text/html`) — Pleroma's `content_type` (P4). Stored beside `text`
    /// and, like it, kept out of [`Status`]: only the edit/source paths and
    /// the outgoing AP `source` field read it.
    pub content_type: &'a str,
    pub visibility: &'a str,
    pub in_reply_to_id: Option<i64>,
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub language: Option<&'a str>,
    /// The requested quote-approval bitmap (already resolved from the client
    /// string / user preference); `None` falls back to the visibility default.
    /// Either way a non-distributable post is downgraded to `nobody`.
    pub quote_approval_policy: Option<i32>,
    /// Thread title of a group submission: a titled post federates as a
    /// `Page` with `name` — Lemmy's native post shape. `None` everywhere else
    /// (general composer titles stay out of scope).
    pub title: Option<&'a str>,
    /// A titled link post's target URL, federated as
    /// `attachment: [{type: Link, href}]` (Lemmy's shape).
    pub external_url: Option<&'a str>,
    /// The AS2 object type to serve this post as (`Event`, E4). `None` keeps the
    /// historical inference — a titled post is a `Page`, everything else a
    /// `Note` — so only a kind the author explicitly picked sets this.
    pub object_type: Option<&'a str>,
}

impl<'a> NewLocalStatus<'a> {
    /// A plain post without content warning or language — the common case.
    #[must_use]
    pub fn new(
        account_id: i64,
        content: &'a str,
        visibility: &'a str,
        in_reply_to_id: Option<i64>,
    ) -> Self {
        Self {
            account_id,
            content,
            text: "",
            content_type: "text/plain",
            visibility,
            in_reply_to_id,
            spoiler_text: "",
            sensitive: false,
            language: None,
            quote_approval_policy: None,
            title: None,
            external_url: None,
            object_type: None,
        }
    }
}

/// The default quote-approval policy for a freshly-posted local status:
/// anyone may quote a distributable (`public`/`unlisted`) post, nobody may
/// quote a `private`/`direct`/`local` one — Mastodon's effective default. The value is
/// the automatic-`public` flag in the high 16 bits (`(1 << 1) << 16`).
#[must_use]
pub fn default_quote_policy(visibility: &str) -> i32 {
    if matches!(visibility, "public" | "unlisted") {
        (1 << 1) << 16
    } else {
        0
    }
}

/// The quote policy actually stored for a local status: the requested bitmap
/// (or the visibility default when absent), forced to `nobody` on a
/// non-distributable post — Mastodon's `downgrade_quote_policy`
/// `before_validation`.
#[must_use]
pub fn effective_quote_policy(visibility: &str, requested: Option<i32>) -> i32 {
    if matches!(visibility, "public" | "unlisted") {
        requested.unwrap_or_else(|| default_quote_policy(visibility))
    } else {
        0
    }
}

/// Stores a local status (its URI is derived, so the column stays NULL).
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn create_local<'e, E: PgExecutor<'e>>(
    executor: E,
    new: NewLocalStatus<'_>,
) -> Result<Status, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        INSERT INTO statuses (id, account_id, content, text, content_type, visibility,
                              in_reply_to_id, spoiler_text, sensitive, language,
                              quote_approval_policy, title, object_type, external_url)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        id::next(),
        new.account_id,
        new.content,
        new.text,
        new.content_type,
        new.visibility,
        new.in_reply_to_id,
        new.spoiler_text,
        new.sensitive,
        new.language,
        effective_quote_policy(new.visibility, new.quote_approval_policy),
        new.title,
        // Stored exactly as stated. This used to fall back to "a title means a
        // `Page`", which was a second, independent decision about the wire type —
        // and a wrong one now that `Page` and `Article` are both titled. The
        // caller resolves the kind once (`actions::stored_kind`) and the column is
        // the single record of it; `Note`/`Question` leave it NULL, like inbound.
        new.object_type,
        new.external_url,
    )
    .fetch_one(executor)
    .await?;
    Ok(status)
}

/// A status' raw source: the text as typed and the format it was authored in.
#[derive(Debug, Clone)]
pub struct StatusSource {
    pub text: String,
    /// `text/plain` | `text/markdown` | `text/html`.
    pub content_type: String,
}

impl Default for StatusSource {
    fn default() -> Self {
        Self {
            text: String::new(),
            content_type: "text/plain".to_owned(),
        }
    }
}

/// The raw source of a status (`/source`, edit merging). Not part of
/// [`Status`]: timelines never need it, only the edit paths do.
pub async fn source_of(pool: &PgPool, status_id: i64) -> Result<Option<StatusSource>, DbError> {
    let row = sqlx::query!(
        "SELECT text, content_type FROM statuses WHERE id = $1 -- STUBKEEP: a stub's source is already nulled",
        status_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| StatusSource {
        text: row.text,
        content_type: row.content_type,
    }))
}

/// Source and optional Webxdc invitation metadata needed only by the
/// `ActivityPub` `Note` builder. They share one batch query so adding the
/// invitation extension does not tax every ordinary Note with another round
/// trip.
#[derive(Debug)]
pub struct NoteSidecars {
    pub source: StatusSource,
    pub webxdc_invitation: Option<(i64, String, String)>,
}

/// The Note-only sidecars of a batch of statuses, keyed by status id. The raw
/// source is present for every stored status; the Webxdc tuple is
/// `(session_id, coordinator_uri, session_name)` when the status is an
/// invitation.
pub async fn note_sidecars<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<HashMap<i64, NoteSidecars>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"SELECT st.id AS "id!", st.text AS "text!", st.content_type AS "content_type!",
                  ws.id AS "session_id?", ws.coordinator_uri AS "coordinator_uri?",
                  ws.name AS "session_name?"
           FROM statuses st -- STUBKEEP: a stub's source is already nulled
           LEFT JOIN webxdc_invitations wi ON wi.status_id = st.id
           LEFT JOIN webxdc_sessions ws ON ws.id = wi.session_id
           WHERE st.id = ANY($1)"#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let webxdc_invitation = match (row.session_id, row.coordinator_uri, row.session_name) {
                (Some(id), Some(uri), Some(name)) => Some((id, uri, name)),
                _ => None,
            };
            (
                row.id,
                NoteSidecars {
                    source: StatusSource {
                        text: row.text,
                        content_type: row.content_type,
                    },
                    webxdc_invitation,
                },
            )
        })
        .collect())
}

/// The first `limit` self-replies of each of `status_ids`, keyed by parent id
/// — the batched form of [`replies_page`] with `self_only`, which is what
/// every `Note` advertises as the first page of its `replies` collection.
///
/// Same predicate as `replies_page(status_id, <its author>, true, 0, limit)`:
/// distributable, non-stub replies authored by the parent's own author,
/// oldest first. The author comes from the join rather than a bound array, so
/// callers need not thread the page's author ids through.
pub async fn self_reply_ids_for<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
    limit: i64,
) -> Result<HashMap<i64, Vec<i64>>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT parent_id AS "parent_id!", id AS "id!"
        FROM (
            SELECT r.in_reply_to_id AS parent_id, r.id,
                   row_number() OVER (PARTITION BY r.in_reply_to_id ORDER BY r.id) AS rn
            FROM statuses r
            JOIN statuses p ON p.id = r.in_reply_to_id
            WHERE r.in_reply_to_id = ANY($1)
              AND r.visibility IN ('public', 'unlisted')
              AND r.deleted_at IS NULL -- STUBFILTER: don't advertise blank stubs
              AND r.account_id = p.account_id
        ) ranked
        WHERE rn <= $2
        ORDER BY parent_id, id
        "#,
        status_ids,
        limit,
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
    for row in rows {
        map.entry(row.parent_id).or_default().push(row.id);
    }
    Ok(map)
}

/// The distinct languages an account has declared on its posts — the
/// inventory the per-follow language filter (M32) offers. Untagged posts
/// always pass that filter, so an empty result means filtering this
/// account by language would do nothing.
pub async fn languages_by_account(pool: &PgPool, account_id: i64) -> Result<Vec<String>, DbError> {
    let languages = sqlx::query_scalar!(
        r#"SELECT DISTINCT language AS "language!" FROM statuses -- STUBKEEP: language aggregate
           WHERE account_id = $1 AND language IS NOT NULL
           ORDER BY language"#,
        account_id
    )
    .fetch_all(pool)
    .await?;
    Ok(languages)
}

/// Stores a local boost of `reblog_of_id`. Idempotent per (account, target).
pub async fn create_local_reblog(
    pool: &PgPool,
    account_id: i64,
    reblog_of_id: i64,
) -> Result<Status, DbError> {
    let mut conn = pool.acquire().await?;
    create_local_reblog_conn(&mut conn, account_id, reblog_of_id).await
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn create_local_reblog_conn(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
    reblog_of_id: i64,
) -> Result<Status, DbError> {
    if let Some(existing) = find_reblog_by(&mut *conn, account_id, reblog_of_id).await? {
        return Ok(existing);
    }
    let status = sqlx::query_as!(
        Status,
        r#"
        INSERT INTO statuses (id, account_id, content, visibility, reblog_of_id)
        VALUES ($1, $2, '', 'public', $3)
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        id::next(),
        account_id,
        reblog_of_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(status)
}

/// Everything stored for a remote status arriving via `Create`.
#[derive(Debug, Clone, Copy)]
pub struct NewRemoteStatus<'a> {
    pub uri: &'a str,
    pub account_id: i64,
    pub content: &'a str,
    pub created_at: OffsetDateTime,
    pub visibility: &'a str,
    pub in_reply_to_id: Option<i64>,
    /// The `inReplyTo` IRI of a reply, kept whether or not the parent resolved
    /// (unlike `in_reply_to_id`, which is only the resolved local row). Lets an
    /// orphaned reply stay recognisable as a reply so it is not boosted into
    /// timelines as a root post.
    pub in_reply_to_uri: Option<&'a str>,
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub language: Option<&'a str>,
    /// The status' human web URL (`url`), as published; `None` if absent.
    pub url: Option<&'a str>,
    /// The quote-approval policy parsed from the note's `interactionPolicy`
    /// (Mastodon's bitmap); `0` when absent or non-quotable.
    pub quote_approval_policy: i32,
    /// The object's `name`, hoisted as a title for top-level objects.
    pub title: Option<&'a str>,
    /// The AS type when ingested as a non-Note kind; `None` for Note/Question.
    pub object_type: Option<&'a str>,
    /// A link post's target URL (Lemmy `Link` attachment).
    pub external_url: Option<&'a str>,
}

/// How a remote object entered the cache. The ordering is monotonic: a cold
/// history row may become an explicit resolution and then a live delivery, but
/// replay can never demote a delivered row back into history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestProvenance {
    History,
    ExplicitResolution,
    Delivery,
}

impl IngestProvenance {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::ExplicitResolution => "explicit_resolution",
            Self::Delivery => "delivery",
        }
    }
}

/// Stores a remote status. Idempotent on the object URI: re-delivery of the
/// same `Create` keeps the original row untouched.
///
///
/// Storing the post is also the moment replies that arrived before it can be
/// joined to it, but that is [`adopt_orphan_replies`]'s job and the ingest path
/// calls it once the new row has its conversation — the fold needs it.
pub async fn upsert_remote(pool: &PgPool, new: NewRemoteStatus<'_>) -> Result<Status, DbError> {
    upsert_remote_with_provenance(pool, new, IngestProvenance::Delivery).await
}

/// Provenance-aware remote upsert. Conflict promotion is atomic with the URI
/// dedup, and a newly discovered parent edge can fill an orphan without
/// replacing any canonical content. One-shot delivery effects are claimed
/// separately by [`claim_delivery_side_effects`].
pub async fn upsert_remote_with_provenance(
    pool: &PgPool,
    new: NewRemoteStatus<'_>,
    provenance: IngestProvenance,
) -> Result<Status, DbError> {
    let history = provenance == IngestProvenance::History;
    let status = sqlx::query_as!(
        Status,
        r#"
        INSERT INTO statuses (id, uri, account_id, content, created_at, sort_at, visibility,
                              in_reply_to_id, spoiler_text, sensitive, language, url, quote_approval_policy,
                              title, object_type, external_url, in_reply_to_uri,
                              ingest_provenance, history_fetched_at, history_last_touched_at)
        VALUES ($1, $2, $3, $4, $5, LEAST($5, now()), $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                $17, CASE WHEN $18 THEN now() END, CASE WHEN $18 THEN now() END)
        ON CONFLICT (uri) WHERE uri IS NOT NULL DO UPDATE SET
            uri = EXCLUDED.uri,
            ingest_provenance = CASE
                WHEN statuses.ingest_provenance = 'delivery'
                  OR EXCLUDED.ingest_provenance = 'delivery' THEN 'delivery'
                WHEN statuses.ingest_provenance = 'explicit_resolution'
                  OR EXCLUDED.ingest_provenance = 'explicit_resolution' THEN 'explicit_resolution'
                ELSE 'history'
            END,
            in_reply_to_id = COALESCE(statuses.in_reply_to_id, EXCLUDED.in_reply_to_id),
            in_reply_to_uri = COALESCE(statuses.in_reply_to_uri, EXCLUDED.in_reply_to_uri),
            history_fetched_at = COALESCE(statuses.history_fetched_at, EXCLUDED.history_fetched_at),
            history_last_touched_at = CASE
                WHEN EXCLUDED.ingest_provenance = 'history' THEN now()
                ELSE statuses.history_last_touched_at
            END
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        id::next(),
        new.uri,
        new.account_id,
        new.content,
        new.created_at,
        new.visibility,
        new.in_reply_to_id,
        new.spoiler_text,
        new.sensitive,
        new.language,
        new.url,
        new.quote_approval_policy,
        new.title,
        new.object_type,
        new.external_url,
        new.in_reply_to_uri,
        provenance.as_str(),
        history,
    )
    .fetch_one(pool)
    .await?;
    Ok(status)
}

/// Fast path for a brand-new live delivery. The URI insert and its one-shot
/// side-effect marker are committed by the same statement, so concurrent
/// deliveries cannot both win. `None` means the URI already exists; callers
/// then use the provenance-aware upsert and row-locked promotion claimant.
pub async fn insert_remote_delivery_claimed(
    pool: &PgPool,
    new: NewRemoteStatus<'_>,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        INSERT INTO statuses (id, uri, account_id, content, created_at, sort_at, visibility,
                              in_reply_to_id, spoiler_text, sensitive, language, url,
                              quote_approval_policy, title, object_type, external_url,
                              in_reply_to_uri, ingest_provenance, delivery_side_effects_at)
        VALUES ($1, $2, $3, $4, $5, LEAST($5, now()), $6, $7, $8, $9, $10, $11, $12,
                $13, $14, $15, $16, 'delivery', now())
        ON CONFLICT (uri) WHERE uri IS NOT NULL DO NOTHING
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type,
                  external_url
        "#,
        id::next(),
        new.uri,
        new.account_id,
        new.content,
        new.created_at,
        new.visibility,
        new.in_reply_to_id,
        new.spoiler_text,
        new.sensitive,
        new.language,
        new.url,
        new.quote_approval_policy,
        new.title,
        new.object_type,
        new.external_url,
        new.in_reply_to_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Promotes a remote row to live-delivery provenance and atomically claims its
/// one-shot side effects. `true` means this caller owns notifications,
/// streaming, trends, previews, and media promotion; every replay gets false.
pub async fn claim_delivery_side_effects(pool: &PgPool, status_id: i64) -> Result<bool, DbError> {
    Ok(claim_delivery_side_effects_with_previous(pool, status_id)
        .await?
        .is_some())
}

/// Like [`claim_delivery_side_effects`], returning the provenance observed
/// while holding the row lock. This makes cold-to-live promotion metrics
/// accurate under a concurrent hydration/inbox race.
pub async fn claim_delivery_side_effects_with_previous(
    pool: &PgPool,
    status_id: i64,
) -> Result<Option<String>, DbError> {
    let mut tx = pool.begin().await?;
    let previous = sqlx::query_scalar!(
        r#"SELECT CASE WHEN history_fetched_at IS NOT NULL
                       THEN 'history' ELSE ingest_provenance END AS "ingest_provenance!"
           FROM statuses
           WHERE id = $1 AND uri IS NOT NULL AND delivery_side_effects_at IS NULL
             AND deleted_at IS NULL -- STUBFILTER
           FOR UPDATE"#,
        status_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if previous.is_some() {
        sqlx::query!(
            r#"UPDATE statuses SET
                   ingest_provenance = 'delivery',
                   delivery_side_effects_at = now(),
                   updated_at = now()
               WHERE id = $1"#,
            status_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(previous)
}

pub async fn ingest_provenance(pool: &PgPool, status_id: i64) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT ingest_provenance FROM statuses WHERE id = $1 -- STUBKEEP: provenance inspection includes a soft-delete stub",
        status_id,
    )
    .fetch_optional(pool)
    .await?)
}

/// Joins replies that arrived before their parent to it, once that parent is
/// stored and placed in its conversation. Returns the replies that were adopted.
///
/// A reply whose `inReplyTo` could not be fetched keeps the IRI but no
/// `in_reply_to_id` (the origin 403s our pull, a group announced a comment whose
/// root we never had, the host is simply down). Nothing used to link it
/// afterwards, so a thread stayed permanently split even once the missing post
/// turned up by another route — a delivery from its own author, a boost, a walk
/// down someone else's `replies` collection. Tree views lost the subtree, the
/// reply kept its "parent not fetched" notice for good, and the reply rules that
/// ask "who wrote the parent" could never answer.
///
/// Adopting the reply carries its own subtree with it: descendants already point
/// at the reply, so one edge repairs the whole branch. The conversation the
/// orphan minted for itself is folded in separately
/// ([`crate::conversation::absorb_placeholders`]) — the reply *edge* is what the
/// tree walks, conversation membership is what the flat/context view groups by,
/// and both were split.
///
/// Called from the inbound ingest path rather than from [`upsert_remote`],
/// deliberately: the conversation fold needs the *parent's* conversation, and
/// that is assigned a few steps after the row is stored.
///
/// Cheap on the common path: nothing replies to most posts before they arrive, so
/// the guard is a single probe of `idx_statuses_orphan_reply_uri` — a partial
/// index over the orphan set alone — and everything below runs only when it hits.
pub async fn adopt_orphan_replies(
    conn: &mut sqlx::PgConnection,
    parent_id: i64,
    parent_uri: &str,
) -> Result<Vec<i64>, DbError> {
    // `status_ancestor_ids` keeps this from closing a loop: a ring of posts that
    // answer each other is one malicious (or merely broken) peer away, and this
    // is the only place that links a reply to a *newer* row, so it is the only
    // place where the reply chain is not acyclic by construction. The guard walks
    // the parent's full ancestry, so it catches a ring of any length, not just
    // the two-post case.
    let adopted = sqlx::query_scalar!(
        r#"UPDATE statuses SET in_reply_to_id = $1
           WHERE in_reply_to_id IS NULL
             AND in_reply_to_uri = $2
             AND id <> $1
             AND NOT EXISTS (
                 SELECT 1 FROM status_ancestor_ids($1) a WHERE a = statuses.id
             )
           RETURNING id"#,
        parent_id,
        parent_uri,
    )
    .fetch_all(&mut *conn)
    .await?;
    crate::conversation::absorb_placeholders(&mut *conn, &adopted, parent_id).await?;
    Ok(adopted)
}

/// A status and every status below it in the reply tree, each with its stored
/// visibility — the set a late audience clamp has to cover, since a reply
/// inherits its ceiling from the conversation and so does everything under it.
///
/// `CYCLE` rather than a depth cap for the same reason as
/// [`adopt_orphan_replies`]'s guard: the walk must terminate on any table state,
/// including one an earlier defect left a loop in.
pub async fn subtree_visibilities(
    conn: &mut sqlx::PgConnection,
    root_id: i64,
) -> Result<Vec<(i64, String)>, DbError> {
    subtree_visibilities_many(conn, &[root_id]).await
}

/// [`subtree_visibilities`] from many roots in one walk. Overlapping subtrees
/// yield each node once.
pub async fn subtree_visibilities_many(
    conn: &mut sqlx::PgConnection,
    root_ids: &[i64],
) -> Result<Vec<(i64, String)>, DbError> {
    if root_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        r#"WITH RECURSIVE subtree(id, visibility) AS (
               SELECT s.id, s.visibility FROM statuses s WHERE s.id = ANY($1) -- STUBKEEP: delete fan-out walks the whole subtree, stubs included
               UNION ALL
               SELECT c.id, c.visibility
               FROM statuses c JOIN subtree t ON c.in_reply_to_id = t.id
           ) CYCLE id SET looped USING path
           SELECT DISTINCT id AS "id!", visibility AS "visibility!" FROM subtree"#,
        root_ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.visibility)).collect())
}

/// Narrows the listed statuses to `visibility`. Only ever called with a value the
/// caller has already established is *narrower* than what each row holds
/// (`plamenu_ap::activity::clamp_visibility` decides that — this is the write
/// half, and must never be handed a wider value).
pub async fn narrow_visibility(
    conn: &mut sqlx::PgConnection,
    status_ids: &[i64],
    visibility: &str,
) -> Result<(), DbError> {
    if status_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE statuses SET visibility = $2 WHERE id = ANY($1)",
        status_ids,
        visibility,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The editable fields applied by an `Update(Note)`.
#[derive(Debug)]
pub struct StatusEdit<'a> {
    pub content: &'a str,
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub language: Option<&'a str>,
    pub edited_at: OffsetDateTime,
    /// Re-parsed `interactionPolicy` bitmap: a remote edit can change the quote
    /// policy (Mastodon redistributes an `Update` when it does).
    pub quote_approval_policy: i32,
    /// The re-hoisted title: an edit may add, change or drop the `name`.
    pub title: Option<&'a str>,
    /// The re-read link-post target (`Link` attachment `href`).
    pub external_url: Option<&'a str>,
}

/// Sets the quote-approval policy of a status (the `interaction_policy` REST
/// endpoint; scoped to the owner of a local status). Returns the updated row.
pub async fn update_quote_policy<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
    account_id: i64,
    policy: i32,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        UPDATE statuses SET quote_approval_policy = $3, updated_at = now()
        WHERE id = $1 AND account_id = $2 AND uri IS NULL
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        status_id,
        account_id,
        policy,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Attaches the OAuth application that created a freshly-posted local status.
/// This is metadata only, so it does not change `updated_at`.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn set_application<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
    application_id: i64,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        UPDATE statuses SET application_id = $2
        WHERE id = $1 AND uri IS NULL
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        status_id,
        application_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(status)
}

/// Applies an edit (`Update(Note)`) to a stored status.
pub async fn apply_edit(
    pool: &PgPool,
    status_id: i64,
    edit: StatusEdit<'_>,
) -> Result<Status, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        UPDATE statuses SET content = $2, spoiler_text = $3, sensitive = $4,
                            language = $5, edited_at = $6, quote_approval_policy = $7,
                            title = $8, external_url = $9,
                            updated_at = now()
        WHERE id = $1
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        status_id,
        edit.content,
        edit.spoiler_text,
        edit.sensitive,
        edit.language,
        edit.edited_at,
        edit.quote_approval_policy,
        edit.title,
        edit.external_url,
    )
    .fetch_one(pool)
    .await?;
    Ok(status)
}

/// Replaces only a status' already-sanitized rendered body. Inbound Article
/// ingest uses this after its media rows exist, when remote inline image URLs
/// can finally be rewritten to stable same-origin attachment URLs.
pub async fn replace_content(
    pool: &PgPool,
    status_id: i64,
    content: &str,
) -> Result<Status, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        UPDATE statuses SET content = $2, updated_at = now()
        WHERE id = $1
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        status_id,
        content,
    )
    .fetch_one(pool)
    .await?;
    Ok(status)
}

/// The editable fields of a locally-authored status.
#[derive(Debug)]
pub struct LocalStatusEdit<'a> {
    pub content: &'a str,
    pub text: &'a str,
    /// The format `text` was authored in (P4); an edit may switch it.
    pub content_type: &'a str,
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub language: Option<&'a str>,
    pub edited_at: OffsetDateTime,
    /// The stored quote-approval bitmap after the edit (the caller resolves
    /// the client string and keeps the current value when the param is absent).
    pub quote_approval_policy: i32,
    /// A new headline, or `None` to keep the stored one. Only a post that already
    /// has a title can be given a new one — the caller enforces that, because
    /// gaining a `name` would change the object kind the audience received.
    pub title: Option<&'a str>,
}

/// Applies an edit to a local status, scoped to its owner.
pub async fn apply_local_edit<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
    account_id: i64,
    edit: LocalStatusEdit<'_>,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        UPDATE statuses SET content = $3, text = $4, content_type = $5, spoiler_text = $6,
                            sensitive = $7, language = $8, edited_at = $9,
                            quote_approval_policy = $10,
                            title = COALESCE($11, title), updated_at = now()
        WHERE id = $1 AND account_id = $2 AND uri IS NULL
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        status_id,
        account_id,
        edit.content,
        edit.text,
        edit.content_type,
        edit.spoiler_text,
        edit.sensitive,
        edit.language,
        edit.edited_at,
        edit.quote_approval_policy,
        edit.title,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Stores a remote boost (inbound `Announce`); idempotent on the activity id.
/// `published` is the Announce's own timestamp — Mastodon stores it as the
/// reblog row's `created_at` and mints its snowflake from it.
/// Inserts the boost row for an inbound `Announce`, deduped atomically: the
/// `(account_id, reblog_of_id)` unique index allows at most one boost per
/// account per target. Returns the new row, or `None` when a boost already
/// existed — a redelivered Announce (same or fresh id), a community's compat
/// double-send (`Announce(Page)` alongside the wrapper `Announce(Create)`), or
/// a concurrent delivery of the two. The caller notifies and streams only on a
/// genuine insert, so a duplicate never re-notifies or re-surfaces the post.
pub async fn upsert_remote_reblog(
    pool: &PgPool,
    uri: &str,
    account_id: i64,
    reblog_of_id: i64,
    published: Option<OffsetDateTime>,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        INSERT INTO statuses (id, uri, account_id, content, visibility, reblog_of_id,
                              created_at, sort_at)
        VALUES ($1, $2, $3, '', 'public', $4,
                COALESCE($5, now()), LEAST(COALESCE($5, now()), now()))
        ON CONFLICT (account_id, reblog_of_id) WHERE reblog_of_id IS NOT NULL DO NOTHING
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        id::next(),
        uri,
        account_id,
        reblog_of_id,
        published,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Delivery-aware boost upsert used by the inbox. Unlike the compatibility
/// helper above, this also promotes a boost first learned from history and
/// atomically claims its one-shot live effects. The returned boolean is the
/// caller's authority to notify and stream; replays always receive `false`.
pub async fn upsert_remote_reblog_delivery(
    pool: &PgPool,
    uri: &str,
    account_id: i64,
    reblog_of_id: i64,
    published: Option<OffsetDateTime>,
) -> Result<(Status, bool), DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        INSERT INTO statuses (id, uri, account_id, content, visibility, reblog_of_id,
                              created_at, sort_at, ingest_provenance)
        VALUES ($1, $2, $3, '', 'public', $4,
                COALESCE($5, now()), LEAST(COALESCE($5, now()), now()), 'delivery')
        ON CONFLICT (account_id, reblog_of_id) WHERE reblog_of_id IS NOT NULL DO UPDATE SET
            ingest_provenance = 'delivery', updated_at = now()
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        id::next(),
        uri,
        account_id,
        reblog_of_id,
        published,
    )
    .fetch_one(pool)
    .await?;
    let effects = claim_delivery_side_effects(pool, status.id).await?;
    Ok((status, effects))
}

/// Cold-ingest counterpart of [`upsert_remote_reblog`]. It never promotes an
/// existing boost and its row is excluded from timelines by provenance. A live
/// Announce later uses [`claim_delivery_side_effects`] on the canonical boost
/// row before emitting any effects.
pub async fn upsert_remote_reblog_history(
    pool: &PgPool,
    uri: &str,
    account_id: i64,
    reblog_of_id: i64,
    published: Option<OffsetDateTime>,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        INSERT INTO statuses (id, uri, account_id, content, visibility, reblog_of_id,
                              created_at, sort_at, ingest_provenance,
                              history_fetched_at, history_last_touched_at)
        VALUES ($1, $2, $3, '', 'public', $4,
                COALESCE($5, now()), LEAST(COALESCE($5, now()), now()),
                'history', now(), now())
        ON CONFLICT (account_id, reblog_of_id) WHERE reblog_of_id IS NOT NULL DO NOTHING
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        id::next(),
        uri,
        account_id,
        reblog_of_id,
        published,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Deletes a local status owned by `account_id`, returning the deleted row.
///
/// Runs in one transaction so the status's stored media files are captured and
/// scheduled for durable deletion *before* the `ON DELETE CASCADE` removes their
/// `media_attachments` rows — otherwise the file names are gone and the bytes
/// stay publicly retrievable via `GET /media/{file}` forever. The
/// cleanup jobs are enqueued only when the delete actually matched, so deleting
/// nothing schedules nothing.
pub async fn delete_local(
    pool: &PgPool,
    status_id: i64,
    account_id: i64,
) -> Result<Option<Status>, DbError> {
    let mut conn = pool.begin().await?;
    let result = delete_local_conn(&mut conn, status_id, account_id).await?;
    conn.commit().await?;
    Ok(result)
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn delete_local_conn(
    conn: &mut sqlx::PgConnection,
    status_id: i64,
    account_id: i64,
) -> Result<Option<Status>, DbError> {
    // Capture the media keys before the cascade; enqueue them only if the delete
    // below removes this status (it may not: wrong owner, or a remote post).
    let keys = crate::media_cleanup::collect_status_keys(&mut *conn, status_id).await?;
    let status = sqlx::query_as!(
        Status,
        r#"
        DELETE FROM statuses
        WHERE id = $1 AND account_id = $2 AND uri IS NULL
        RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                  in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                  language, url, quote_approval_policy, application_id, title, object_type, external_url
        "#,
        status_id,
        account_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if status.is_some() {
        crate::media_cleanup::enqueue_many(&mut *conn, &keys).await?;
    }
    Ok(status)
}

/// Permanently deletes any stored root post or reply and writes the admin
/// audit line in the same transaction. Unlike author deletion this is an
/// instance-administrator purge, so ownership/locality are intentionally not
/// part of the predicate; callers must authorize before entering this helper.
pub async fn purge_by_id_with_audit(
    pool: &PgPool,
    status_id: i64,
    audit: crate::admin_action_log::NewActionLog<'_>,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let keys = crate::media_cleanup::collect_status_keys(&mut tx, status_id).await?;
    let deleted = sqlx::query!("DELETE FROM statuses WHERE id = $1", status_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    if deleted {
        crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
        crate::admin_action_log::record_tx(&mut tx, audit).await?;
    }
    tx.commit().await?;
    Ok(deleted)
}

/// Removes all root posts and replies authored by an account, preserving the
/// actor itself. This is Lemmy's site-ban `remove_data` option, not Plamenu's
/// stronger account self-deletion (which also erases profile/settings).
pub async fn purge_authored(pool: &PgPool, account_id: i64) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    let ids = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE account_id = $1 AND reblog_of_id IS NULL -- STUBKEEP: an account-data purge removes soft-delete stubs too",
        account_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    let keys = crate::media_cleanup::collect_status_keys_many(&mut tx, &ids).await?;
    let removed = sqlx::query!("DELETE FROM statuses WHERE id = ANY($1)", &ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    tx.commit().await?;
    Ok(removed)
}

/// Deletes a status by URI, but only when it belongs to `account_id` —
/// a remote actor must not be able to delete someone else's post. Captures the
/// status's cached media (previews, HLS segments) for durable file cleanup in the
/// same transaction, before the cascade removes their rows.
pub async fn delete_by_uri(pool: &PgPool, uri: &str, account_id: i64) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let target = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND account_id = $2 -- STUBKEEP: identity probe; a redelivered Delete must find the stub",
        uri,
        account_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(status_id) = target else {
        tx.commit().await?;
        return Ok(false);
    };
    let keys = crate::media_cleanup::collect_status_keys(&mut tx, status_id).await?;
    sqlx::query!("DELETE FROM statuses WHERE id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    tx.commit().await?;
    Ok(true)
}

/// Deletes a status by URI regardless of author — for a moderator removal a
/// community relays (Lemmy `Delete` where the actor is the mod, not the post's
/// author), which the author-gated [`delete_by_uri`] would never match. Callers
/// MUST authorize out of band: the inbox applies this only when the post's own
/// origin confirms it gone (410/Tombstone), which a forgeable Announce cannot
/// fake. Captures cached media for cleanup, like the gated variant.
pub async fn delete_by_uri_any(pool: &PgPool, uri: &str) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let target = sqlx::query_scalar!("SELECT id FROM statuses WHERE uri = $1 -- STUBKEEP: identity probe; a redelivered Delete must find the stub", uri)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(status_id) = target else {
        tx.commit().await?;
        return Ok(false);
    };
    let keys = crate::media_cleanup::collect_status_keys(&mut tx, status_id).await?;
    sqlx::query!("DELETE FROM statuses WHERE id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    tx.commit().await?;
    Ok(true)
}

/// Strips a soft-deleted status of the child rows a hard delete would have
/// cascaded away, and captures its media keys for durable file cleanup. Keeps
/// exactly two child sets: `status_mentions` (visibility math depends on them;
/// blanked only at render) and `status_conversations` (membership — every
/// conversation read filters `deleted_at`). Idempotent: on an already-stripped
/// stub every statement matches nothing. Returns the media keys to enqueue
/// after the status row's own update matches.
#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive child-table cleanup is clearer as one transaction-scoped inventory"
)]
async fn strip_stub_children(
    tx: &mut sqlx::PgConnection,
    status_id: i64,
) -> Result<Vec<String>, DbError> {
    let keys = crate::media_cleanup::collect_status_keys(tx, status_id).await?;
    // Boosts of this post go, exactly as the hard-delete cascade did.
    sqlx::query!("DELETE FROM statuses WHERE reblog_of_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    // Every CASCADE child keyed by status_id, except the two kept sets above.
    sqlx::query!("DELETE FROM bookmarks WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM custom_filter_statuses WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM favourites WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM group_locked_posts WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM link_crawl_jobs WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM media_attachments WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM notifications WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM polls WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM preview_cards_statuses WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM reply_fetch_jobs WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM status_dislikes WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM status_edits WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM status_events WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM status_pins WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM status_reactions WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM status_reply_fetches WHERE status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM status_tags WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM status_trends WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM tagged_objects WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    // A quote the deleted post made goes; a quote *of* it un-links (the old
    // SET NULL). `notification_requests.last_status_id` likewise un-links.
    sqlx::query!("DELETE FROM quotes WHERE status_id = $1", status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "UPDATE quotes SET quoted_status_id = NULL WHERE quoted_status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE notification_requests SET last_status_id = NULL WHERE last_status_id = $1",
        status_id
    )
    .execute(&mut *tx)
    .await?;
    Ok(keys)
}

/// Whether any status still replies to `status_id` — decides stub vs hard
/// delete: a post with replies is kept as a stub to hold the tree together, a
/// reply-less post has no tree to preserve and is removed outright.
async fn has_live_reply(conn: &mut sqlx::PgConnection, status_id: i64) -> Result<bool, DbError> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM statuses WHERE in_reply_to_id = $1) AS "e!" -- STUBKEEP: a stub child still holds the tree; prune_leaf_stubs collapses chains bottom-up"#,
        status_id,
    )
    .fetch_one(conn)
    .await?;
    Ok(exists)
}

/// Deletes a local status, keeping a "stub" (`GoToSocial`'s tombstone-for-threading)
/// **only when replies still hang off it**: the row survives with content nulled
/// and `deleted_at` set so the reply tree keeps the middle post's edge, while
/// every user-facing list/count filters it out and the maintenance GC reaps it
/// once its replies are gone. A reply-less post is hard-deleted like
/// [`delete_local`] (no tree to preserve), so the stub set stays confined to
/// genuine middle-of-thread posts. Mirrors [`delete_local`]'s owner/local gate
/// and `Option<Status>` return. Idempotent.
pub async fn stub_local(
    pool: &PgPool,
    status_id: i64,
    account_id: i64,
) -> Result<Option<Status>, DbError> {
    let mut conn = pool.begin().await?;
    let result = stub_local_conn(&mut conn, status_id, account_id).await?;
    conn.commit().await?;
    Ok(result)
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn stub_local_conn(
    conn: &mut sqlx::PgConnection,
    status_id: i64,
    account_id: i64,
) -> Result<Option<Status>, DbError> {
    let status = if has_live_reply(&mut *conn, status_id).await? {
        let row = sqlx::query_as!(
            Status,
            r#"
            UPDATE statuses SET content = '', spoiler_text = '', text = '', title = NULL,
                                sensitive = false, language = NULL,
                                deleted_at = COALESCE(deleted_at, now())
            WHERE id = $1 AND account_id = $2 AND uri IS NULL
            RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                      in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                      language, url, quote_approval_policy, application_id, title, object_type, external_url
            "#,
            status_id,
            account_id,
        )
        .fetch_optional(&mut *conn)
        .await?;
        if row.is_some() {
            let keys = strip_stub_children(&mut *conn, status_id).await?;
            crate::media_cleanup::enqueue_many(&mut *conn, &keys).await?;
        }
        row
    } else {
        let keys = crate::media_cleanup::collect_status_keys(&mut *conn, status_id).await?;
        let row = sqlx::query_as!(
            Status,
            r#"
            DELETE FROM statuses
            WHERE id = $1 AND account_id = $2 AND uri IS NULL
            RETURNING id, uri, account_id, content, created_at, updated_at, visibility,
                      in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
                      language, url, quote_approval_policy, application_id, title, object_type, external_url
            "#,
            status_id,
            account_id,
        )
        .fetch_optional(&mut *conn)
        .await?;
        if row.is_some() {
            crate::media_cleanup::enqueue_many(&mut *conn, &keys).await?;
        }
        row
    };
    Ok(status)
}

/// Soft-deletes a status by URI, gated to its author ([`delete_by_uri`]'s stub
/// twin) — an inbound remote `Delete` from the post's own actor.
pub async fn stub_by_uri(pool: &PgPool, uri: &str, account_id: i64) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let target = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND account_id = $2 -- STUBKEEP: identity probe; a redelivered Delete must find the stub",
        uri,
        account_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    stub_resolved(tx, target).await
}

/// Soft-deletes a status by URI regardless of author ([`delete_by_uri_any`]'s
/// stub twin) — a moderator removal a community relays. Callers authorize out
/// of band, exactly as for the hard-delete variant.
pub async fn stub_by_uri_any(pool: &PgPool, uri: &str) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let target = sqlx::query_scalar!("SELECT id FROM statuses WHERE uri = $1 -- STUBKEEP: identity probe; a redelivered Delete must find the stub", uri)
        .fetch_optional(&mut *tx)
        .await?;
    stub_resolved(tx, target).await
}

/// Deletes an already-resolved status id within the caller's transaction, then
/// commits — the shared tail of the by-URI stub variants. Keeps a stub only if
/// replies still hang off it (like [`stub_local`]); else hard-deletes. `None`
/// id means the lookup matched nothing (returns `false`).
async fn stub_resolved(
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    status_id: Option<i64>,
) -> Result<bool, DbError> {
    let Some(status_id) = status_id else {
        tx.commit().await?;
        return Ok(false);
    };
    if has_live_reply(&mut tx, status_id).await? {
        sqlx::query!(
            r#"UPDATE statuses SET content = '', spoiler_text = '', text = '', title = NULL,
                                   sensitive = false, language = NULL,
                                   deleted_at = COALESCE(deleted_at, now())
               WHERE id = $1"#,
            status_id,
        )
        .execute(&mut *tx)
        .await?;
        let keys = strip_stub_children(&mut tx, status_id).await?;
        crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    } else {
        let keys = crate::media_cleanup::collect_status_keys(&mut tx, status_id).await?;
        sqlx::query!("DELETE FROM statuses WHERE id = $1", status_id)
            .execute(&mut *tx)
            .await?;
        crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// The subset of `status_ids` that are soft-deleted stubs — a batched lookup so
/// the renderer can emit a placeholder without carrying `deleted_at` on every
/// `Status` (and without a per-row query). Empty input returns empty.
pub async fn deleted_ids<'e, E: PgExecutor<'e>>(
    executor: E,
    status_ids: &[i64],
) -> Result<HashSet<i64>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let ids = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE id = ANY($1) AND deleted_at IS NOT NULL -- STUBKEEP: this IS the stub lookup",
        status_ids,
    )
    .fetch_all(executor)
    .await?;
    Ok(ids.into_iter().collect())
}

/// Maps status id → the AP URI of its reply parent, for the subset of
/// `status_ids` that are replies whose parent was never fetched
/// (`in_reply_to_id` NULL but `in_reply_to_uri` set — e.g. the parent lives on
/// a host that black-holed our pull). Batched like [`deleted_ids`] so the
/// renderer can surface an "unfetched parent" notice without carrying
/// `in_reply_to_uri` on every `Status` or issuing a per-row query. Empty input
/// returns empty.
pub async fn unresolved_reply_parents<'e, E: PgExecutor<'e>>(
    executor: E,
    status_ids: &[i64],
) -> Result<Vec<(i64, String)>, DbError> {
    if status_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        "SELECT id, in_reply_to_uri FROM statuses -- STUBKEEP: parent-chasing keeps stub edges
         WHERE id = ANY($1) AND in_reply_to_id IS NULL AND in_reply_to_uri IS NOT NULL",
        status_ids,
    )
    .fetch_all(executor)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.in_reply_to_uri.map(|uri| (row.id, uri)))
        .collect())
}

/// Whether a single status is a soft-deleted stub — the AP object route uses
/// this to answer `410 Gone` instead of serving an emptied Note.
pub async fn is_deleted<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<bool, DbError> {
    let deleted = sqlx::query_scalar!(
        r#"SELECT (deleted_at IS NOT NULL) AS "deleted!" FROM statuses WHERE id = $1 -- STUBKEEP: this IS the stub lookup"#,
        status_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(deleted.unwrap_or(false))
}

/// Hard-wipes soft-deleted stubs that have become *leaves* — nothing replies to
/// them any more — and were deleted before `older_than`. A stub exists only to
/// keep a reply tree connected across a deleted middle post; once its own
/// replies are gone it serves no purpose, so the maintenance GC reaps it (`GtS`'s
/// `PruneLeafStubs`). Bounded by `limit` per call; returns how many were wiped.
/// Reaping a leaf can turn its parent stub into a new leaf, collected next run.
/// A leaf has no status children to orphan and its media/child rows were
/// already stripped at stub time, so a plain delete is clean.
pub async fn prune_leaf_stubs(
    pool: &PgPool,
    older_than: OffsetDateTime,
    limit: i64,
) -> Result<u64, DbError> {
    let result = sqlx::query!(
        r#"
        DELETE FROM statuses
        WHERE id IN (
            SELECT s.id FROM statuses s
            WHERE s.deleted_at IS NOT NULL
              AND s.deleted_at < $1
              AND NOT EXISTS (SELECT 1 FROM statuses c WHERE c.in_reply_to_id = s.id)
            ORDER BY s.deleted_at
            LIMIT $2
        )
        "#,
        older_than,
        limit,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Purges every post/comment authored by `author_id` that `group_id` announced
/// (rows this group boosted) — the local reflection of a Lemmy community ban
/// with `removeData: true`, which purges the banned user's content in that
/// community and sends no per-post `Delete`. Scoped to the group's own
/// attributed content, so a community can only reach its own space. Returns how
/// many statuses were removed; cascades each one's boost rows and captures
/// cached media for cleanup.
pub async fn remove_group_content_of(
    pool: &PgPool,
    group_id: i64,
    author_id: i64,
) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    let ids = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT s.id
        FROM statuses s -- STUBKEEP: a moderation sweep must catch stubs too
        JOIN statuses boost ON boost.reblog_of_id = s.id AND boost.account_id = $1
        WHERE s.account_id = $2
        "#,
        group_id,
        author_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    if ids.is_empty() {
        tx.commit().await?;
        return Ok(0);
    }
    let keys = crate::media_cleanup::collect_status_keys_many(&mut tx, &ids).await?;
    let removed = sqlx::query!("DELETE FROM statuses WHERE id = ANY($1)", &ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    tx.commit().await?;
    Ok(removed)
}

/// Fetches any status (local or remote) by primary key.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn find_by_id<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at, visibility,
               in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
               language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses -- STUBKEEP: single fetch; callers decide (AP GET 410s, context serves the placeholder)
        WHERE id = $1
        "#,
        status_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(status)
}

/// Walks `in_reply_to_id` up to the thread root — the top-level post a reply
/// chain descends from. Returns `status_id` itself when it has no parent (it
/// *is* a root) or when a parent id dangles. Bounded to Mastodon's 40-deep
/// context so a pathological chain can't loop the CTE unbounded. Used by group
/// thread-lock enforcement: a comment's lock is the root post's lock.
pub async fn thread_root(pool: &PgPool, status_id: i64) -> Result<i64, DbError> {
    let root = sqlx::query_scalar!(
        r#"
        WITH RECURSIVE chain(id, in_reply_to_id, depth) AS (
            SELECT id, in_reply_to_id, 0 FROM statuses WHERE id = $1 -- STUBKEEP: the root walk crosses stubs so the tree holds
            UNION ALL
            SELECT s.id, s.in_reply_to_id, chain.depth + 1
            FROM statuses s
            JOIN chain ON s.id = chain.in_reply_to_id
            WHERE chain.depth < 40
        )
        SELECT id AS "id!" FROM chain WHERE in_reply_to_id IS NULL
        ORDER BY depth DESC
        LIMIT 1
        "#,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    // No NULL-parent row found (dangling ancestor) → fall back to the input.
    Ok(root.unwrap_or(status_id))
}

/// Fetches statuses by a set of ids. Missing ids are simply absent from the
/// result; ordering is unspecified (callers reorder as needed).
pub async fn find_by_ids<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    ids: &[i64],
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at, visibility,
               in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
               language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses -- STUBKEEP: single fetch; callers decide (AP GET 410s, context serves the placeholder)
        WHERE id = ANY($1)
        "#,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

pub async fn find_by_uri(pool: &PgPool, uri: &str) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at, visibility,
               in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
               language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses -- STUBKEEP: identity fetch; ingest must dedup against a stub
        WHERE uri = $1
        "#,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Looks up a status by its human-facing permalink. This avoids a network
/// round trip when a client searches for a known Discourse topic/post URL
/// whose canonical `ActivityPub` object ID lives under `/ap/object/...`.
pub async fn find_by_url(pool: &PgPool, url: &str) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at, visibility,
               in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
               language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses -- STUBKEEP: exact human-permalink lookup for URL search
        WHERE url = $1
        ORDER BY id
        LIMIT 1
        "#,
        url,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Batch lookup by stored URI, the set-based form of [`find_by_uri`] — resolves
/// many reported-object URIs from one inbound `Flag` in a single query (QC
/// audit #28). Missing URIs are simply absent; callers match by `uri`.
pub async fn find_by_uris(pool: &PgPool, uris: &[&str]) -> Result<Vec<Status>, DbError> {
    let owned: Vec<String> = uris.iter().map(|u| (*u).to_owned()).collect();
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at, visibility,
               in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
               language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses -- STUBKEEP: identity fetch; ingest must dedup against a stub
        WHERE uri = ANY($1)
        "#,
        &owned,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// Fetches a local status by id, scoped to its owner.
pub async fn find_local(
    pool: &PgPool,
    account_id: i64,
    status_id: i64,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at, visibility,
               in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
               language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses -- STUBKEEP: single fetch; callers decide (AP GET 410s, context serves the placeholder)
        WHERE id = $1 AND account_id = $2 AND uri IS NULL
        "#,
        status_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Distributable (public/unlisted) replies to a status, oldest first,
/// keyset-paginated by `min_id`: the parent author's own replies when
/// `self_only`, everyone else's otherwise — the two halves of Mastodon's
/// replies collection.
pub async fn replies_page(
    pool: &PgPool,
    status_id: i64,
    author_id: i64,
    self_only: bool,
    min_id: i64,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at, visibility,
               in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
               language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses
        WHERE in_reply_to_id = $1
          AND visibility IN ('public', 'unlisted')
          AND deleted_at IS NULL -- STUBFILTER: don't advertise blank stubs in the replies collection
          AND (account_id = $2) = $3
          AND id > $4
        ORDER BY id ASC
        LIMIT $5
        "#,
        status_id,
        author_id,
        self_only,
        min_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// A chronological page of **every** post in a conversation (any visibility,
/// boosts excluded), keyset by `id > after_id`. For the FEP-171b container
/// (`/contexts/{id}/history`), which is served only to an authorized requester
/// — unlike the public posts collection, it lists private/direct posts too.
pub async fn conversation_page(
    pool: &PgPool,
    conversation_id: i64,
    after_id: i64,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at, s.visibility,
               s.in_reply_to_id, s.reblog_of_id, s.edited_at, s.spoiler_text, s.sensitive,
               s.language, s.url, s.quote_approval_policy, s.application_id, s.title,
               s.object_type, s.external_url
        FROM statuses s
        JOIN status_conversations sc ON sc.status_id = s.id
        WHERE sc.conversation_id = $1
          AND s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.id > $2
        ORDER BY s.id ASC
        LIMIT $3
        "#,
        conversation_id,
        after_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// An account's boost of a given status, if any.
pub async fn find_reblog_by<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    reblog_of_id: i64,
) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at, visibility,
               in_reply_to_id, reblog_of_id, edited_at, spoiler_text, sensitive,
               language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses -- STUBKEEP: a boost wrapper never stubs (nothing replies to it)
        WHERE account_id = $1 AND reblog_of_id = $2
        "#,
        account_id,
        reblog_of_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// Whether a status is a reply — it names an `inReplyTo`, whether or not the
/// parent resolved. `in_reply_to_uri` is set for every reply (orphans included,
/// since the parent may be unreachable at ingest); `in_reply_to_id` catches
/// pre-migration rows that predate the URI column. Used to keep replies out of
/// group-boost timelines: a community announces every comment, and a reply
/// whose parent could not be fetched must not be mistaken for a root post.
pub async fn is_reply(pool: &PgPool, status_id: i64) -> Result<bool, DbError> {
    let is_reply = sqlx::query_scalar!(
        r#"SELECT (in_reply_to_id IS NOT NULL OR in_reply_to_uri IS NOT NULL) AS "is_reply!"
           FROM statuses WHERE id = $1 -- STUBKEEP: a stub edge still threads"#,
        status_id,
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(false);
    Ok(is_reply)
}

/// Whether a status is a reply to *somebody else* — the rows the public and
/// local timelines drop when `public_timeline_replies` is off. Same three-state
/// reasoning as the predicate carried by those queries (see above
/// `public_timeline_received`), so a reply whose parent never arrived counts:
/// its author is unknown, so it cannot be shown to be a self-thread.
///
/// Exists for the streaming hub, which holds a [`Status`] and so cannot see
/// `in_reply_to_uri` or `in_reply_to_account_id` — both live in WHERE clauses
/// only. The SSE page and the REST page must agree.
pub async fn is_non_self_reply(pool: &PgPool, status_id: i64) -> Result<bool, DbError> {
    let hidden = sqlx::query_scalar!(
        r#"SELECT ((in_reply_to_id IS NOT NULL OR in_reply_to_uri IS NOT NULL)
                   AND (in_reply_to_account_id IS NULL
                        OR in_reply_to_account_id <> account_id)) AS "hidden!"
           FROM statuses WHERE id = $1 -- STUBKEEP: a stub edge still threads"#,
        status_id,
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(false);
    Ok(hidden)
}

/// Resolves a status-id pagination cursor into the `(sort_at, id)` keyset
/// bound the [`TimelineOrder::Published`] queries paginate by. When the
/// anchor row is gone (deleted after the client saw the page), the bound
/// falls back to the ingest time embedded in the snowflake id — always ≥ the
/// row's clamped `sort_at`, so pagination keeps going and at worst repeats a
/// few statuses instead of dead-ending.
pub(crate) async fn sort_anchor(
    pool: &PgPool,
    max_id: Option<i64>,
) -> Result<Option<(OffsetDateTime, i64)>, DbError> {
    let Some(max_id) = max_id else {
        return Ok(None);
    };
    let sort_at = sqlx::query_scalar!(
        "SELECT sort_at FROM statuses WHERE id = $1 -- STUBKEEP: ordering probe",
        max_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(Some((
        sort_at.unwrap_or_else(|| id::time_of(max_id)),
        max_id,
    )))
}

/// The home timeline: own statuses plus those of accepted follows, newest
/// first, keyset-paginated (the cursor is a status id under either ordering).
/// Direct messages never appear here — they live in `/api/v1/conversations`,
/// like Mastodon. Statuses by (and boosts of) blocked or muted authors are
/// hidden, and so are members of the viewer's *exclusive* lists (their posts
/// live on the list timeline instead; the viewer's own statuses always stay).
pub async fn home_timeline(
    pool: &PgPool,
    account_id: i64,
    order: TimelineOrder,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    match order {
        TimelineOrder::Published => {
            let anchor = sort_anchor(pool, max_id).await?;
            home_timeline_published(pool, account_id, anchor, limit).await
        }
        TimelineOrder::Received => home_timeline_received(pool, account_id, max_id, limit).await,
    }
}

// The two orderings are separate compile-checked queries whose WHERE bodies
// must stay in sync; only the keyset predicate and ORDER BY differ (the
// `published` variant merges each author's `idx_statuses_account_sort_at`
// (account_id, sort_at, id) slice, the `received` one their
// `idx_statuses_account` (account_id, id) slice).
//
// Both are structured as a UNION of two *driven* arms rather than a single
// scan of the global feed order:
//
//   * arm A — self plus accepted follows, reached by nested-looping the follow set into each
//     author's per-account index slice;
//   * arm B — the followed-hashtag injection (Mastodon's `deliver_to_hashtag_followers!`), reached
//     via `tag_follows`.
//
// Both arms are keyset-bounded and feed-ordered. Arm A takes `LIMIT n` per
// author; arm B takes it per followed tag, so neither walks an unbounded
// history while applying `account_hidden()` and the M32/M37 subplans (the
// pathology the old shapes hit). Taking the top `n` from those candidates is
// exact: a row in the combined top `n` has fewer than `n` rows above it in at
// least one author/tag slice it belongs to, so it survives that slice's limit.
// `UNION` dedupes posts reached through both arms or multiple followed tags.
//
// Arm A additionally splits the visibility filter by altitude: predicates
// depending only on (viewer, author) — block/mute/domain hiding, instance
// policy, exclusive lists, and the M32 show_reblogs/languages and
// with_replies follow columns
// — are evaluated once per author on the driving follow scan, not once per
// candidate row. At 500 follows that is ~500 evaluations instead of
// (#follows + 1) x limit ≈ 10k (the dense-home pathology; 194ms → ~70ms).
// Arm B keeps the full per-row filter because its author varies per row, and
// it is the copy that must stay semantically in line with the list timeline's
// equivalent filters.
#[allow(
    clippy::too_many_lines,
    reason = "one keyset-bounded UNION query; the length is the SQL literal, which can't be split"
)]
async fn home_timeline_received(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id AS "id!", uri, account_id AS "account_id!", content AS "content!",
               created_at AS "created_at!", updated_at AS "updated_at!",
               visibility AS "visibility!", in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text AS "spoiler_text!", sensitive AS "sensitive!", language, url,
               quote_approval_policy AS "quote_approval_policy!", application_id,
               title, object_type, external_url
        FROM (
            -- Arm A — self plus accepted follows. Driven per author: for each
            -- followed account (and self) take that author's own newest slice
            -- via `idx_statuses_account`, capped at `$3`, so the candidate set
            -- is bounded by (#follows + 1) * limit regardless of how prolific
            -- any author is or how deep the cursor has paged.
            (SELECT af.id, af.uri, af.account_id, af.content, af.created_at, af.updated_at,
                    af.visibility, af.in_reply_to_id, af.reblog_of_id, af.edited_at,
                    af.spoiler_text, af.sensitive, af.language, af.url, af.quote_approval_policy,
                    af.application_id,
                    af.title, af.object_type, af.external_url
             FROM (SELECT aid, show_reblogs, with_replies, languages
                   FROM (SELECT target_account_id AS aid, show_reblogs, with_replies, languages
                         FROM follows
                         WHERE account_id = $1 AND NOT pending
                         UNION ALL SELECT $1, TRUE, TRUE, NULL) all_authors
                   -- Author-level home filters, hoisted out of the per-status
                   -- LATERAL: they depend only on (viewer, author), so they
                   -- run once per author instead of once per candidate row
                   -- ((#follows + 1) x limit times — the dense-home
                   -- pathology). The block/mute/domain arms inline
                   -- account_hidden($1, aid) so each becomes a hashed subplan
                   -- built once per query; account_hidden() stays the source
                   -- of truth for those semantics (as with the local
                   -- timeline's inlined copy). MUST stay in sync with the
                   -- other ordering's copy. Arm B below keeps the full
                   -- per-row filter because its author varies per row.
                   WHERE (aid = $1 OR (
                          aid NOT IN (SELECT b.target_account_id FROM blocks b
                                      WHERE b.account_id = $1)
                      AND aid NOT IN (SELECT b.account_id FROM blocks b
                                      WHERE b.target_account_id = $1)
                      AND aid NOT IN (SELECT m.target_account_id FROM mutes m
                                      WHERE m.account_id = $1
                                        AND (m.expires_at IS NULL OR m.expires_at > now()))
                      AND aid NOT IN (SELECT adb.account_id
                                      FROM account_domain_blocks adb
                                      JOIN accounts v ON v.id = $1 AND adb.domain = v.domain)))
                     AND EXISTS (SELECT 1 FROM accounts a
                                 WHERE a.id = aid AND a.suspended_at IS NULL
                                   AND (a.portable OR instance_domain_allowed(a.domain))
                                   AND (aid = $1 OR a.portable OR a.domain IS NULL OR a.domain NOT IN (
                                        SELECT adb.domain FROM account_domain_blocks adb
                                        WHERE adb.account_id = $1)))
                     AND (aid = $1 OR NOT EXISTS (
                            SELECT 1 FROM list_accounts la JOIN lists l ON l.id = la.list_id
                            WHERE l.account_id = $1 AND l.exclusive AND la.account_id = aid))
                  ) authors
             CROSS JOIN LATERAL (
                 SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
                        s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
                        s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy,
                        s.application_id,
                        s.title, s.object_type, s.external_url
                 FROM statuses s
                 WHERE s.account_id = authors.aid
                   AND s.deleted_at IS NULL -- STUBFILTER
                   AND s.ingest_provenance = 'delivery'
                   -- Row-level home filters (author-level ones are hoisted
                   -- above; the show_reblogs/with_replies/languages follow columns carried
                   -- from the driving scan replace the M32 sf-subquery — the
                   -- driving row IS that follow row). MUST stay in sync with
                   -- the other ordering's copy and, semantically, with the
                   -- arm-B full filter below.
                   AND s.visibility <> 'direct'
                   -- The boost target's account_hidden($1, target_author),
                   -- inlined: as a function call it ran per boost row
                   -- (~0.1ms each, the dominant dense-home cost — ~2.4k
                   -- calls/page at 1.5k follows). One probe into the target
                   -- status, block/mute arms as hashed subplans, the domain
                   -- arms as in the hoisted author filter. MUST stay in sync
                   -- with the other ordering's copy and, semantically, with
                   -- account_hidden().
                   AND (s.reblog_of_id IS NULL OR NOT EXISTS (
                          SELECT 1 FROM statuses t
                          WHERE t.id = s.reblog_of_id
                            AND t.account_id <> $1
                            AND (t.account_id IN (SELECT b.target_account_id FROM blocks b
                                                  WHERE b.account_id = $1)
                              OR t.account_id IN (SELECT b.account_id FROM blocks b
                                                  WHERE b.target_account_id = $1)
                              OR t.account_id IN (SELECT m.target_account_id FROM mutes m
                                                  WHERE m.account_id = $1
                                                    AND (m.expires_at IS NULL
                                                         OR m.expires_at > now()))
                              OR t.account_id IN (SELECT adb.account_id
                                                  FROM account_domain_blocks adb
                                                  JOIN accounts v2
                                                    ON v2.id = $1 AND adb.domain = v2.domain)
                              OR EXISTS (SELECT 1 FROM accounts ta
                                         WHERE ta.id = t.account_id
                                           AND NOT ta.portable
                                           AND ta.domain IN (SELECT adb.domain
                                                             FROM account_domain_blocks adb
                                                             WHERE adb.account_id = $1)))))
                   -- The boost target's instance policy, plus the rule that with the
                   -- per-follow reply switch off, an Announce is judged by what
                   -- it announces, because a wrapper's own in_reply_to_id is
                   -- always NULL and the reply conjunct below would let every
                   -- announced comment through (for a community follow, that is
                   -- all of them). Folded into this probe rather than added as a
                   -- third one — arm A already reads the target twice, and
                   -- inlining those is what took dense home 194ms -> ~70ms.
                   -- Deliberately tests `in_reply_to_id` alone: an announce of a
                   -- reply whose parent never arrived is unclassifiable, and
                   -- home's standing choice for that state is to keep it.
                   AND (s.reblog_of_id IS NULL OR EXISTS (
                          SELECT 1 FROM statuses target
                          JOIN accounts target_author ON target_author.id = target.account_id
                          WHERE target.id = s.reblog_of_id
                            AND target_author.suspended_at IS NULL
                            AND (target_author.portable OR instance_domain_allowed(target_author.domain))
                            AND (authors.with_replies OR target.in_reply_to_id IS NULL)))
                   -- PeerTube delivers one Video twice when the viewer follows
                   -- both its owning Person and its channel Group: the Person's
                   -- Create plus the channel's Announce. If the target author
                   -- is itself followed, the original already occupies home;
                   -- suppress only that Video-shaped Group wrapper. Ordinary
                   -- community announces remain distinct facts, and following
                   -- only the channel still yields its wrapper.
                   AND (s.reblog_of_id IS NULL OR NOT EXISTS (
                          SELECT 1
                          FROM statuses video
                          JOIN accounts wrapper_author ON wrapper_author.id = s.account_id
                          JOIN follows video_author_follow
                            ON video_author_follow.account_id = $1
                           AND video_author_follow.target_account_id = video.account_id
                           AND NOT video_author_follow.pending
                          WHERE video.id = s.reblog_of_id
                            AND video.object_type = 'Video'
                            AND wrapper_author.actor_type = 'Group'))
                   AND (authors.aid = $1 OR (
                          (s.reblog_of_id IS NULL OR authors.show_reblogs)
                          AND (s.language IS NULL OR authors.languages IS NULL
                               OR s.language = ANY (authors.languages))))
                   -- Per-follow `with_replies`. Off, the row-level
                   -- fallback keeps the three exemptions Mastodon, Sharkey and
                   -- GoToSocial all agree on. The `EXISTS` is the expensive arm
                   -- and evaluates only for reply rows of flag-off follows; it
                   -- is a follow-set probe rather than a `statuses` probe, which
                   -- is what the denormalized author column bought. A reply
                   -- whose parent never arrived has in_reply_to_id NULL and so
                   -- stays visible — deliberate, and a divergence from Mastodon
                   -- (see FEEDS_DESIGN). MUST stay in sync with the other
                   -- ordering's copy and with the list timeline's equivalent.
                   AND (authors.aid = $1 OR authors.with_replies
                        OR s.in_reply_to_id IS NULL
                        OR s.in_reply_to_account_id = s.account_id
                        OR s.in_reply_to_account_id = $1
                        OR EXISTS (SELECT 1 FROM follows rf
                                   WHERE rf.account_id = $1
                                     AND rf.target_account_id = s.in_reply_to_account_id
                                     AND NOT rf.pending))
                   AND (authors.aid = $1 OR s.language IS NULL OR NOT EXISTS (
                          SELECT 1 FROM users u
                          WHERE u.account_id = $1 AND u.chosen_languages IS NOT NULL
                            AND s.language <> ALL (u.chosen_languages)))
                   AND ($2::bigint IS NULL OR s.id < $2)
                 ORDER BY s.id DESC
                 LIMIT $3) af)
            UNION
            -- Arm B — followed hashtags inject public, non-reblog statuses,
            -- like Mastodon's hashtag-follower delivery (broadcastable only).
            -- A reply is injected only when its parent is known and authored by
            -- the viewer, the poster (self-reply), or someone the viewer
            -- follows — Mastodon's home-feed filter applies the same rule.
            (SELECT tagged.id, tagged.uri, tagged.account_id, tagged.content,
                    tagged.created_at, tagged.updated_at, tagged.visibility,
                    tagged.in_reply_to_id, tagged.reblog_of_id, tagged.edited_at,
                    tagged.spoiler_text, tagged.sensitive, tagged.language, tagged.url,
                    tagged.quote_approval_policy, tagged.application_id,
                    tagged.title, tagged.object_type, tagged.external_url
             FROM tag_follows tf
             CROSS JOIN LATERAL (
                 -- Drive each followed tag down its feed-order membership and
                 -- stop after one page. Previously the semijoin gathered every
                 -- status for every followed tag before this arm's LIMIT.
                 SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
                        s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
                        s.spoiler_text, s.sensitive, s.language, s.url,
                        s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url
                 FROM (
                     SELECT status_id
                     FROM status_tags
                     WHERE tag_id = tf.tag_id
                     ORDER BY status_id DESC
                     OFFSET 0
                 ) st
                 JOIN statuses s ON s.id = st.status_id
                 WHERE s.reblog_of_id IS NULL
                   AND s.deleted_at IS NULL -- STUBFILTER
                   AND s.ingest_provenance = 'delivery'
                   AND s.visibility = 'public'
                   AND (s.in_reply_to_id IS NULL
                        OR EXISTS (
                            SELECT 1 FROM statuses p
                            WHERE p.id = s.in_reply_to_id
                              AND (p.account_id = $1
                                   OR p.account_id = s.account_id
                                   OR EXISTS (
                                       SELECT 1 FROM follows rf
                                       WHERE rf.account_id = $1
                                         AND rf.target_account_id = p.account_id
                                         AND NOT rf.pending))))
                   -- Full home visibility filter, per row (arm B's author
                   -- varies per row, so nothing can hoist). MUST stay in sync
                   -- with the other ordering's arm-B copy and, semantically,
                   -- with arm A's hoisted + row-level split.
                   AND s.visibility <> 'direct'
                   AND NOT account_hidden($1, s.account_id)
                   AND (s.reblog_of_id IS NULL OR NOT account_hidden(
                          $1, (SELECT t.account_id FROM statuses t WHERE t.id = s.reblog_of_id)))
                   AND EXISTS (SELECT 1 FROM accounts a
                               WHERE a.id = s.account_id
                                 AND (a.portable OR instance_domain_allowed(a.domain)))
                   AND (s.reblog_of_id IS NULL OR EXISTS (
                          SELECT 1 FROM statuses target
                          JOIN accounts target_author ON target_author.id = target.account_id
                          WHERE target.id = s.reblog_of_id
                            AND target_author.suspended_at IS NULL
                            AND (target_author.portable OR instance_domain_allowed(target_author.domain))))
                   AND (s.account_id = $1 OR NOT EXISTS (
                          SELECT 1 FROM list_accounts la JOIN lists l ON l.id = la.list_id
                          WHERE l.account_id = $1 AND l.exclusive AND la.account_id = s.account_id))
                   AND (s.account_id = $1 OR NOT EXISTS (
                          SELECT 1 FROM follows sf
                          WHERE sf.account_id = $1 AND sf.target_account_id = s.account_id
                            AND NOT sf.pending
                            AND ((s.reblog_of_id IS NOT NULL AND NOT sf.show_reblogs)
                                 OR (s.language IS NOT NULL AND sf.languages IS NOT NULL
                                     AND s.language <> ALL (sf.languages)))))
                   AND (s.account_id = $1 OR s.language IS NULL OR NOT EXISTS (
                          SELECT 1 FROM users u
                          WHERE u.account_id = $1 AND u.chosen_languages IS NOT NULL
                            AND s.language <> ALL (u.chosen_languages)))
                   AND ($2::bigint IS NULL OR s.id < $2)
                 ORDER BY st.status_id DESC
                 LIMIT $3
             ) tagged
             WHERE tf.account_id = $1)
        ) merged
        ORDER BY id DESC
        LIMIT $3
        "#,
        account_id,
        max_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

#[allow(
    clippy::too_many_lines,
    reason = "one keyset-bounded UNION query; the length is the SQL literal, which can't be split"
)]
async fn home_timeline_published(
    pool: &PgPool,
    account_id: i64,
    anchor: Option<(OffsetDateTime, i64)>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let (anchor_sort_at, anchor_id) = anchor.unzip();
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id AS "id!", uri, account_id AS "account_id!", content AS "content!",
               created_at AS "created_at!", updated_at AS "updated_at!",
               visibility AS "visibility!", in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text AS "spoiler_text!", sensitive AS "sensitive!", language, url,
               quote_approval_policy AS "quote_approval_policy!", application_id,
               title, object_type, external_url
        FROM (
            -- Arm A — self plus accepted follows, driven per author via
            -- `idx_statuses_account_sort_at` and capped at `$4` per author; see
            -- the `received` variant for why this bounds the candidate set.
            (SELECT af.id, af.uri, af.account_id, af.content, af.created_at, af.updated_at,
                    af.visibility, af.in_reply_to_id, af.reblog_of_id, af.edited_at,
                    af.spoiler_text, af.sensitive, af.language, af.url, af.quote_approval_policy,
                    af.application_id,
                    af.title, af.object_type, af.external_url, af.sort_at
             FROM (SELECT aid, show_reblogs, with_replies, languages
                   FROM (SELECT target_account_id AS aid, show_reblogs, with_replies, languages
                         FROM follows
                         WHERE account_id = $1 AND NOT pending
                         UNION ALL SELECT $1, TRUE, TRUE, NULL) all_authors
                   -- Author-level home filters, hoisted; see the `received`
                   -- variant for the full rationale. MUST stay in sync with
                   -- that copy.
                   WHERE (aid = $1 OR (
                          aid NOT IN (SELECT b.target_account_id FROM blocks b
                                      WHERE b.account_id = $1)
                      AND aid NOT IN (SELECT b.account_id FROM blocks b
                                      WHERE b.target_account_id = $1)
                      AND aid NOT IN (SELECT m.target_account_id FROM mutes m
                                      WHERE m.account_id = $1
                                        AND (m.expires_at IS NULL OR m.expires_at > now()))
                      AND aid NOT IN (SELECT adb.account_id
                                      FROM account_domain_blocks adb
                                      JOIN accounts v ON v.id = $1 AND adb.domain = v.domain)))
                     AND EXISTS (SELECT 1 FROM accounts a
                                 WHERE a.id = aid AND a.suspended_at IS NULL
                                   AND (a.portable OR instance_domain_allowed(a.domain))
                                   AND (aid = $1 OR a.portable OR a.domain IS NULL OR a.domain NOT IN (
                                        SELECT adb.domain FROM account_domain_blocks adb
                                        WHERE adb.account_id = $1)))
                     AND (aid = $1 OR NOT EXISTS (
                            SELECT 1 FROM list_accounts la JOIN lists l ON l.id = la.list_id
                            WHERE l.account_id = $1 AND l.exclusive AND la.account_id = aid))
                  ) authors
             CROSS JOIN LATERAL (
                 SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
                        s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
                        s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy,
                        s.application_id,
                        s.title, s.object_type, s.external_url, s.sort_at
                 FROM statuses s
                 WHERE s.account_id = authors.aid
                   AND s.deleted_at IS NULL -- STUBFILTER
                   AND s.ingest_provenance = 'delivery'
                   -- Row-level home filters; see the `received` variant (incl.
                   -- the inlined boost-target account_hidden rationale). MUST
                   -- stay in sync with that copy and, semantically, with the
                   -- arm-B full filter below.
                   AND s.visibility <> 'direct'
                   AND (s.reblog_of_id IS NULL OR NOT EXISTS (
                          SELECT 1 FROM statuses t
                          WHERE t.id = s.reblog_of_id
                            AND t.account_id <> $1
                            AND (t.account_id IN (SELECT b.target_account_id FROM blocks b
                                                  WHERE b.account_id = $1)
                              OR t.account_id IN (SELECT b.account_id FROM blocks b
                                                  WHERE b.target_account_id = $1)
                              OR t.account_id IN (SELECT m.target_account_id FROM mutes m
                                                  WHERE m.account_id = $1
                                                    AND (m.expires_at IS NULL
                                                         OR m.expires_at > now()))
                              OR t.account_id IN (SELECT adb.account_id
                                                  FROM account_domain_blocks adb
                                                  JOIN accounts v2
                                                    ON v2.id = $1 AND adb.domain = v2.domain)
                              OR EXISTS (SELECT 1 FROM accounts ta
                                         WHERE ta.id = t.account_id
                                           AND NOT ta.portable
                                           AND ta.domain IN (SELECT adb.domain
                                                             FROM account_domain_blocks adb
                                                             WHERE adb.account_id = $1)))))
                   -- Boost-target instance policy plus the reply-target rule; see the `received`
                   -- variant for why the reply test rides this probe.
                   AND (s.reblog_of_id IS NULL OR EXISTS (
                          SELECT 1 FROM statuses target
                          JOIN accounts target_author ON target_author.id = target.account_id
                          WHERE target.id = s.reblog_of_id
                            AND target_author.suspended_at IS NULL
                            AND (target_author.portable OR instance_domain_allowed(target_author.domain))
                            AND (authors.with_replies OR target.in_reply_to_id IS NULL)))
                   -- PeerTube Person Create + channel Group Announce
                   -- de-duplication; keep in sync with the received ordering.
                   AND (s.reblog_of_id IS NULL OR NOT EXISTS (
                          SELECT 1
                          FROM statuses video
                          JOIN accounts wrapper_author ON wrapper_author.id = s.account_id
                          JOIN follows video_author_follow
                            ON video_author_follow.account_id = $1
                           AND video_author_follow.target_account_id = video.account_id
                           AND NOT video_author_follow.pending
                          WHERE video.id = s.reblog_of_id
                            AND video.object_type = 'Video'
                            AND wrapper_author.actor_type = 'Group'))
                   AND (authors.aid = $1 OR (
                          (s.reblog_of_id IS NULL OR authors.show_reblogs)
                          AND (s.language IS NULL OR authors.languages IS NULL
                               OR s.language = ANY (authors.languages))))
                   -- Per-follow `with_replies`; see the `received`
                   -- variant. MUST stay in sync with that copy.
                   AND (authors.aid = $1 OR authors.with_replies
                        OR s.in_reply_to_id IS NULL
                        OR s.in_reply_to_account_id = s.account_id
                        OR s.in_reply_to_account_id = $1
                        OR EXISTS (SELECT 1 FROM follows rf
                                   WHERE rf.account_id = $1
                                     AND rf.target_account_id = s.in_reply_to_account_id
                                     AND NOT rf.pending))
                   AND (authors.aid = $1 OR s.language IS NULL OR NOT EXISTS (
                          SELECT 1 FROM users u
                          WHERE u.account_id = $1 AND u.chosen_languages IS NOT NULL
                            AND s.language <> ALL (u.chosen_languages)))
                   AND ($2::timestamptz IS NULL OR (s.sort_at, s.id) < ($2, $3::bigint))
                 ORDER BY s.sort_at DESC, s.id DESC
                 LIMIT $4) af)
            UNION
            -- Arm B — followed-hashtag injection; see the `received` variant for
            -- the reply-parent gate rationale.
            (SELECT tagged.id, tagged.uri, tagged.account_id, tagged.content,
                    tagged.created_at, tagged.updated_at, tagged.visibility,
                    tagged.in_reply_to_id, tagged.reblog_of_id, tagged.edited_at,
                    tagged.spoiler_text, tagged.sensitive, tagged.language, tagged.url,
                    tagged.quote_approval_policy, tagged.application_id,
                    tagged.title, tagged.object_type, tagged.external_url, tagged.sort_at
             FROM tag_follows tf
             CROSS JOIN LATERAL (
                 -- As in the received-order arm, one ordered LIMIT per tag is
                 -- exact: a row in the combined top N cannot have N rows above
                 -- it within every tag it carries.
                 SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
                        s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
                        s.spoiler_text, s.sensitive, s.language, s.url,
                        s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url, s.sort_at
                 FROM (
                     -- OFFSET 0 is an intentional planner barrier: it keeps
                     -- the indexed feed order as the nested loop's outer path
                     -- instead of flattening into a gather-then-sort join.
                     SELECT status_id, sort_at
                     FROM status_tags
                     WHERE tag_id = tf.tag_id
                     ORDER BY sort_at DESC, status_id DESC
                     OFFSET 0
                 ) st
                 JOIN statuses s ON s.id = st.status_id
                 WHERE s.reblog_of_id IS NULL
                   AND s.deleted_at IS NULL -- STUBFILTER
                   AND s.ingest_provenance = 'delivery'
                   AND s.visibility = 'public'
                   AND (s.in_reply_to_id IS NULL
                        OR EXISTS (
                            SELECT 1 FROM statuses p
                            WHERE p.id = s.in_reply_to_id
                              AND (p.account_id = $1
                                   OR p.account_id = s.account_id
                                   OR EXISTS (
                                       SELECT 1 FROM follows rf
                                       WHERE rf.account_id = $1
                                         AND rf.target_account_id = p.account_id
                                         AND NOT rf.pending))))
                   -- Full home visibility filter, per row (arm B's author
                   -- varies per row, so nothing can hoist). MUST stay in sync
                   -- with the other ordering's arm-B copy and, semantically,
                   -- with arm A's hoisted + row-level split.
                   AND s.visibility <> 'direct'
                   AND NOT account_hidden($1, s.account_id)
                   AND (s.reblog_of_id IS NULL OR NOT account_hidden(
                          $1, (SELECT t.account_id FROM statuses t WHERE t.id = s.reblog_of_id)))
                   AND EXISTS (SELECT 1 FROM accounts a
                               WHERE a.id = s.account_id
                                 AND (a.portable OR instance_domain_allowed(a.domain)))
                   AND (s.reblog_of_id IS NULL OR EXISTS (
                          SELECT 1 FROM statuses target
                          JOIN accounts target_author ON target_author.id = target.account_id
                          WHERE target.id = s.reblog_of_id
                            AND target_author.suspended_at IS NULL
                            AND (target_author.portable OR instance_domain_allowed(target_author.domain))))
                   AND (s.account_id = $1 OR NOT EXISTS (
                          SELECT 1 FROM list_accounts la JOIN lists l ON l.id = la.list_id
                          WHERE l.account_id = $1 AND l.exclusive AND la.account_id = s.account_id))
                   AND (s.account_id = $1 OR NOT EXISTS (
                          SELECT 1 FROM follows sf
                          WHERE sf.account_id = $1 AND sf.target_account_id = s.account_id
                            AND NOT sf.pending
                            AND ((s.reblog_of_id IS NOT NULL AND NOT sf.show_reblogs)
                                 OR (s.language IS NOT NULL AND sf.languages IS NOT NULL
                                     AND s.language <> ALL (sf.languages)))))
                   AND (s.account_id = $1 OR s.language IS NULL OR NOT EXISTS (
                          SELECT 1 FROM users u
                          WHERE u.account_id = $1 AND u.chosen_languages IS NOT NULL
                            AND s.language <> ALL (u.chosen_languages)))
                   AND ($2::timestamptz IS NULL OR (s.sort_at, s.id) < ($2, $3::bigint))
                 ORDER BY st.sort_at DESC, st.status_id DESC
                 LIMIT $4
             ) tagged
             WHERE tf.account_id = $1)
        ) merged
        ORDER BY sort_at DESC, id DESC
        LIMIT $4
        "#,
        account_id,
        anchor_sort_at,
        anchor_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// The public (federated or local-only) timeline: public originals, plus
/// local-only originals on the authenticated local timeline. Keyset-paginated
/// (the cursor is a status id under either ordering).
///
/// `include_replies` is the operator's `public_timeline_replies` setting. Off
/// (the default) these feeds match Mastodon's `PublicFeed`: originals and
/// self-threads only. The predicate is spelled out above the four queries.
///
/// The two locality flavors are separate queries with deliberately different
/// shapes. The federated feed scans the global `idx_statuses_sort_at` order —
/// nearly every row passes its filters, so the shared moderation helpers run
/// only a handful of times per page. The local feed scans the partial
/// `idx_statuses_local_*` indexes (migration 0130) with the author-level
/// checks inlined; see `local_timeline_published` for why.
pub async fn public_timeline(
    pool: &PgPool,
    local_only: bool,
    viewer: Option<i64>,
    include_replies: bool,
    order: TimelineOrder,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    match (order, local_only) {
        (TimelineOrder::Published, false) => {
            let anchor = sort_anchor(pool, max_id).await?;
            public_timeline_published(pool, viewer, include_replies, anchor, limit).await
        }
        (TimelineOrder::Published, true) => {
            let anchor = sort_anchor(pool, max_id).await?;
            local_timeline_published(pool, viewer, include_replies, anchor, limit).await
        }
        (TimelineOrder::Received, false) => {
            public_timeline_received(pool, viewer, include_replies, max_id, limit).await
        }
        (TimelineOrder::Received, true) => {
            local_timeline_received(pool, viewer, include_replies, max_id, limit).await
        }
    }
}

// The reply predicate carried by all four queries below, verbatim in
// each (sqlx's compile-time checking requires literal query strings, so there
// is no builder to share):
//
//     AND ($k::boolean
//          OR (s.in_reply_to_id IS NULL AND s.in_reply_to_uri IS NULL)
//          OR s.in_reply_to_account_id = s.account_id)
//
// It sorts rows into three states, not two:
//
// - *Not a reply* — both reply columns NULL. Kept. Testing `in_reply_to_id`
//   alone would leak the third state onto the timeline as pseudo-top-level
//   posts, so this mirrors `is_reply` and checks both columns.
// - *Self-thread* — `in_reply_to_account_id = account_id`. Kept: Mastodon's
//   scope is `not_reply OR reply_to_account`, and a thread its author wrote
//   alone is a post, not half a conversation.
// - *Reply whose parent never arrived* — `in_reply_to_uri` set,
//   `in_reply_to_id` NULL, so `in_reply_to_account_id` is NULL too and the
//   third clause cannot rescue it either. Hidden, deliberately: its author is
//   unknowable, so it cannot be shown to be a self-thread. Adoption makes
//   that state temporary wherever the parent is reachable — once it links, the
//   row is judged like any other reply. Home takes the *opposite* choice for
//   the same state, because there the author is already followed.
//
// The federated pair lands this on every row of the hot path, which is why it
// is a column test and not a subquery.

// Same keep-in-sync rule as the home timeline pair above — and the local
// pair below must keep selecting the same rows as this pair restricted to
// local authors (`public_timeline_both` in the tests runs every scenario
// under both orderings and both localities where applicable).

async fn public_timeline_received(
    pool: &PgPool,
    viewer: Option<i64>,
    include_replies: bool,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url
        FROM statuses s
        JOIN accounts a ON a.id = s.account_id
        WHERE s.visibility = 'public'
          AND s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.ingest_provenance = 'delivery'
          AND NOT account_hidden($1, s.account_id)
          AND (a.portable OR instance_domain_allowed(a.domain))
          -- Suspended and silenced authors both vanish from the shared public
          -- timelines for everyone (Mastodon's `without_silenced`). Followers
          -- still receive silenced authors through home and list timelines.
          AND a.suspended_at IS NULL
          AND NOT account_silenced(s.account_id)
          -- Replies: kept only when the operator opted in, or the row
          -- is not a reply, or it continues its own author's thread.
          AND ($4::boolean
               OR (s.in_reply_to_id IS NULL AND s.in_reply_to_uri IS NULL)
               OR s.in_reply_to_account_id = s.account_id)
          AND ($2::bigint IS NULL OR s.id < $2)
        ORDER BY s.id DESC
        LIMIT $3
        "#,
        viewer,
        max_id,
        limit,
        include_replies,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

async fn public_timeline_published(
    pool: &PgPool,
    viewer: Option<i64>,
    include_replies: bool,
    anchor: Option<(OffsetDateTime, i64)>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let (anchor_sort_at, anchor_id) = anchor.unzip();
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url
        FROM statuses s
        JOIN accounts a ON a.id = s.account_id
        WHERE s.visibility = 'public'
          AND s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.ingest_provenance = 'delivery'
          AND NOT account_hidden($1, s.account_id)
          AND (a.portable OR instance_domain_allowed(a.domain))
          -- Suspended and silenced authors both vanish from the shared public
          -- timelines for everyone (Mastodon's `without_silenced`). Followers
          -- still receive silenced authors through home and list timelines.
          AND a.suspended_at IS NULL
          AND NOT account_silenced(s.account_id)
          -- Replies: kept only when the operator opted in, or the row
          -- is not a reply, or it continues its own author's thread.
          AND ($5::boolean
               OR (s.in_reply_to_id IS NULL AND s.in_reply_to_uri IS NULL)
               OR s.in_reply_to_account_id = s.account_id)
          AND ($2::timestamptz IS NULL OR (s.sort_at, s.id) < ($2, $3::bigint))
        ORDER BY s.sort_at DESC, s.id DESC
        LIMIT $4
        "#,
        viewer,
        anchor_sort_at,
        anchor_id,
        limit,
        include_replies,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

// The local-only pair. Semantically it is the federated pair restricted to
// local authors (plus the signed-in `local` visibility arm), but the SQL is
// specialized so a page costs O(limit) instead of O(total local statuses):
//
// - `s.uri IS NULL` identifies Plamenu-owned originals (their URI is derived),
//   matching the `idx_statuses_local_*` partial indexes so the common cohort
//   streams in feed order and stops at `limit`. Portable originals retain
//   their client-owned URI and join this timeline through `a.portable`.
// - The shared author-moderation helpers are inlined (the slice-6c lesson:
//   non-inlined STABLE SQL functions re-run per row as black boxes). Portable
//   authors keep account-level silence/block/mute treatment but are exempt
//   from instance/user domain policy, matching migration 0061's helpers.

async fn local_timeline_received(
    pool: &PgPool,
    viewer: Option<i64>,
    include_replies: bool,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url
        FROM statuses s
        WHERE s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND (s.visibility = 'public'
               OR ($1::bigint IS NOT NULL AND s.visibility = 'local'))
          AND EXISTS (
              SELECT 1 FROM accounts a
              WHERE a.id = s.account_id
                AND (s.uri IS NULL OR a.portable)
                AND a.suspended_at IS NULL
                AND a.silenced_at IS NULL
                AND (a.id = $1 OR (
                     NOT EXISTS (
                         SELECT 1 FROM blocks b
                         WHERE (b.account_id = $1 AND b.target_account_id = a.id)
                            OR (b.account_id = a.id AND b.target_account_id = $1))
                     AND NOT EXISTS (
                         SELECT 1 FROM mutes m
                         WHERE m.account_id = $1 AND m.target_account_id = a.id
                           AND (m.expires_at IS NULL OR m.expires_at > now())))))
          -- Replies: kept only when the operator opted in, or the row
          -- is not a reply, or it continues its own author's thread. Same
          -- predicate as the federated pair above; keep them in step.
          AND ($4::boolean
               OR (s.in_reply_to_id IS NULL AND s.in_reply_to_uri IS NULL)
               OR s.in_reply_to_account_id = s.account_id)
          AND ($2::bigint IS NULL OR s.id < $2)
        ORDER BY s.id DESC
        LIMIT $3
        "#,
        viewer,
        max_id,
        limit,
        include_replies,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

async fn local_timeline_published(
    pool: &PgPool,
    viewer: Option<i64>,
    include_replies: bool,
    anchor: Option<(OffsetDateTime, i64)>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let (anchor_sort_at, anchor_id) = anchor.unzip();
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url
        FROM statuses s
        WHERE s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND (s.visibility = 'public'
               OR ($1::bigint IS NOT NULL AND s.visibility = 'local'))
          AND EXISTS (
              SELECT 1 FROM accounts a
              WHERE a.id = s.account_id
                AND (s.uri IS NULL OR a.portable)
                AND a.suspended_at IS NULL
                AND a.silenced_at IS NULL
                AND (a.id = $1 OR (
                     NOT EXISTS (
                         SELECT 1 FROM blocks b
                         WHERE (b.account_id = $1 AND b.target_account_id = a.id)
                            OR (b.account_id = a.id AND b.target_account_id = $1))
                     AND NOT EXISTS (
                         SELECT 1 FROM mutes m
                         WHERE m.account_id = $1 AND m.target_account_id = a.id
                           AND (m.expires_at IS NULL OR m.expires_at > now())))))
          -- Replies: kept only when the operator opted in, or the row
          -- is not a reply, or it continues its own author's thread. Same
          -- predicate as the federated pair above; keep them in step.
          AND ($5::boolean
               OR (s.in_reply_to_id IS NULL AND s.in_reply_to_uri IS NULL)
               OR s.in_reply_to_account_id = s.account_id)
          AND ($2::timestamptz IS NULL OR (s.sort_at, s.id) < ($2, $3::bigint))
        ORDER BY s.sort_at DESC, s.id DESC
        LIMIT $4
        "#,
        viewer,
        anchor_sort_at,
        anchor_id,
        limit,
        include_replies,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// Filters for an account's statuses listing — the Mastodon
/// `/api/v1/accounts/{id}/statuses` parameters.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // independent, orthogonal filter flags
pub struct AccountStatusesFilter {
    pub exclude_replies: bool,
    pub exclude_reblogs: bool,
    pub only_media: bool,
    /// When set, `only_media` looks through a reblog to the boosted post's
    /// media, so a Group's feed (all announces) can drive a Media wall. A
    /// person's Media tab leaves this off and keeps its own-media semantics.
    pub media_through_reblog: bool,
    pub tagged: Option<String>,
    pub max_id: Option<i64>,
    pub since_id: Option<i64>,
}

/// An account's statuses, visibility-gated for `viewer`: `private` posts only
/// for the author, accepted followers, and accounts the post mentions;
/// `direct` posts only for the author and mentioned accounts (Mastodon's
/// `AccountStatusesFilter`). `exclude_replies` keeps self-replies, like
/// Mastodon's `without_replies`.
///
/// Honors the viewer's [`TimelineOrder`], like the home/public/list/tag
/// timelines: `Received` reads newest-ingested first (the snowflake `id`
/// order), `Published` reads by the author's own publish time (`sort_at`), so
/// a remote profile's backfilled posts interleave the way the origin ordered
/// them rather than the order we happened to fetch them. Anonymous and
/// evidence-picker callers pass the default (`Published`); for a purely local
/// author the two orders coincide (`sort_at` tracks `id`).
pub async fn by_account(
    pool: &PgPool,
    account_id: i64,
    viewer: Option<i64>,
    filter: &AccountStatusesFilter,
    order: TimelineOrder,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    match order {
        TimelineOrder::Published => {
            let max_anchor = sort_anchor(pool, filter.max_id).await?;
            let since_anchor = sort_anchor(pool, filter.since_id).await?;
            by_account_published(
                pool,
                account_id,
                viewer,
                filter,
                max_anchor,
                since_anchor,
                limit,
            )
            .await
        }
        TimelineOrder::Received => {
            by_account_received(pool, account_id, viewer, filter, limit).await
        }
    }
}

// The two orderings are separate compile-checked queries whose visibility/
// filter bodies ($1..$7) are byte-identical; only the keyset predicate and
// ORDER BY differ (the `published` variant walks `idx_statuses_account_sort_at`
// (account_id, sort_at, id), the `received` one `idx_statuses_account`
// (account_id, id)). MUST stay in sync when touching the filter body.

async fn by_account_received(
    pool: &PgPool,
    account_id: i64,
    viewer: Option<i64>,
    filter: &AccountStatusesFilter,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url
        FROM statuses s
        WHERE s.account_id = $1
          AND EXISTS (SELECT 1 FROM accounts author
                      WHERE author.id = $1 AND author.suspended_at IS NULL)
          AND (s.reblog_of_id IS NULL OR (
                NOT account_relationship_hidden($2, (SELECT target.account_id FROM statuses target
                                                     WHERE target.id = s.reblog_of_id))
                AND EXISTS (
                    SELECT 1 FROM statuses target
                    -- Keep this correlated to the current boost. PostgreSQL
                    -- otherwise decorrelates the EXISTS into a hash of every
                    -- status/account before the outer LIMIT can stop at one
                    -- page (notably a Group's announce-only timeline).
                    CROSS JOIN LATERAL (
                        SELECT a.domain, a.portable, a.suspended_at
                        FROM accounts a
                        WHERE a.id = target.account_id
                        OFFSET 0
                    ) target_author
                    WHERE target.id = s.reblog_of_id
                      AND target_author.suspended_at IS NULL
                      AND (target_author.portable OR instance_domain_allowed(target_author.domain))
                    OFFSET 0)))
          AND s.deleted_at IS NULL -- STUBFILTER (the reply-parent probe below keeps stubs: a stubbed parent still roots a chain)
          -- GoToSocial can explicitly forbid exposing federated content to
          -- unauthenticated web visitors. Signed-in viewers still see the
          -- bounded cache; anonymous viewers do not see cold rows in that case.
          AND ($2::bigint IS NOT NULL OR s.ingest_provenance <> 'history'
               OR NOT EXISTS (
                   SELECT 1 FROM remote_history_states h
                   WHERE h.account_id = $1
                     AND h.anonymous_visibility = 'restricted'))
          -- A silenced author's posts are withheld from anonymous visitors on
          -- the profile page; any logged-in viewer sees them normally.
          AND ($2::bigint IS NOT NULL OR NOT account_silenced($1))
          AND (s.visibility IN ('public', 'unlisted')
               OR $2::bigint = $1
               OR ($2::bigint IS NOT NULL AND s.visibility = 'local'
                   AND EXISTS (SELECT 1 FROM accounts author
                               WHERE author.id = $1
                                 AND (author.domain IS NULL OR author.portable)))
               OR ($2 IS NOT NULL AND (
                     (s.visibility = 'private'
                      AND EXISTS (SELECT 1 FROM follows f
                                  WHERE f.account_id = $2 AND f.target_account_id = $1
                                    AND NOT f.pending))
                     OR EXISTS (SELECT 1 FROM status_mentions m
                                WHERE m.status_id = s.id AND m.account_id = $2))))
          AND (NOT $3 OR s.in_reply_to_id IS NULL
               OR EXISTS (SELECT 1 FROM statuses p
                          WHERE p.id = s.in_reply_to_id AND p.account_id = $1))
          AND (NOT $4 OR s.reblog_of_id IS NULL)
          -- `only_media` matches the row's own attachments, unless
          -- `media_through_reblog` ($7) is set, when a boost matches on the
          -- boosted post's media — so a Group's announce-only feed drives a
          -- Media wall while a person's stays own-media-only.
          AND (NOT $5 OR EXISTS (
                 SELECT 1 FROM media_attachments ma
                 WHERE ma.status_id = CASE WHEN $7 AND s.reblog_of_id IS NOT NULL
                                           THEN s.reblog_of_id ELSE s.id END))
          AND ($6::text IS NULL OR EXISTS (
                 SELECT 1 FROM status_tags st
                 JOIN tags t ON t.id = st.tag_id
                 WHERE st.status_id = s.id AND lower(t.name) = lower($6)))
          AND ($8::bigint IS NULL OR s.id < $8)
          AND ($9::bigint IS NULL OR s.id > $9)
        ORDER BY s.id DESC
        LIMIT $10
        "#,
        account_id,
        viewer,
        filter.exclude_replies,
        filter.exclude_reblogs,
        filter.only_media,
        filter.tagged.as_deref(),
        filter.media_through_reblog,
        filter.max_id,
        filter.since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

async fn by_account_published(
    pool: &PgPool,
    account_id: i64,
    viewer: Option<i64>,
    filter: &AccountStatusesFilter,
    max_anchor: Option<(OffsetDateTime, i64)>,
    since_anchor: Option<(OffsetDateTime, i64)>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let (max_sort_at, max_id) = max_anchor.unzip();
    let (since_sort_at, since_id) = since_anchor.unzip();
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url
        FROM statuses s
        WHERE s.account_id = $1
          AND EXISTS (SELECT 1 FROM accounts author
                      WHERE author.id = $1 AND author.suspended_at IS NULL)
          AND (s.reblog_of_id IS NULL OR (
                NOT account_relationship_hidden($2, (SELECT target.account_id FROM statuses target
                                                     WHERE target.id = s.reblog_of_id))
                AND EXISTS (
                    SELECT 1 FROM statuses target
                    -- Keep this correlated to the current boost. PostgreSQL
                    -- otherwise decorrelates the EXISTS into a hash of every
                    -- status/account before the outer LIMIT can stop at one
                    -- page (notably a Group's announce-only timeline).
                    CROSS JOIN LATERAL (
                        SELECT a.domain, a.portable, a.suspended_at
                        FROM accounts a
                        WHERE a.id = target.account_id
                        OFFSET 0
                    ) target_author
                    WHERE target.id = s.reblog_of_id
                      AND target_author.suspended_at IS NULL
                      AND (target_author.portable OR instance_domain_allowed(target_author.domain))
                    OFFSET 0)))
          AND s.deleted_at IS NULL -- STUBFILTER (the reply-parent probe below keeps stubs: a stubbed parent still roots a chain)
          AND ($2::bigint IS NOT NULL OR s.ingest_provenance <> 'history'
               OR NOT EXISTS (
                   SELECT 1 FROM remote_history_states h
                   WHERE h.account_id = $1
                     AND h.anonymous_visibility = 'restricted'))
          -- A silenced author's posts are withheld from anonymous visitors on
          -- the profile page; any logged-in viewer sees them normally.
          AND ($2::bigint IS NOT NULL OR NOT account_silenced($1))
          AND (s.visibility IN ('public', 'unlisted')
               OR $2::bigint = $1
               OR ($2::bigint IS NOT NULL AND s.visibility = 'local'
                   AND EXISTS (SELECT 1 FROM accounts author
                               WHERE author.id = $1
                                 AND (author.domain IS NULL OR author.portable)))
               OR ($2 IS NOT NULL AND (
                     (s.visibility = 'private'
                      AND EXISTS (SELECT 1 FROM follows f
                                  WHERE f.account_id = $2 AND f.target_account_id = $1
                                    AND NOT f.pending))
                     OR EXISTS (SELECT 1 FROM status_mentions m
                                WHERE m.status_id = s.id AND m.account_id = $2))))
          AND (NOT $3 OR s.in_reply_to_id IS NULL
               OR EXISTS (SELECT 1 FROM statuses p
                          WHERE p.id = s.in_reply_to_id AND p.account_id = $1))
          AND (NOT $4 OR s.reblog_of_id IS NULL)
          -- `only_media` matches the row's own attachments, unless
          -- `media_through_reblog` ($7) is set, when a boost matches on the
          -- boosted post's media — so a Group's announce-only feed drives a
          -- Media wall while a person's stays own-media-only.
          AND (NOT $5 OR EXISTS (
                 SELECT 1 FROM media_attachments ma
                 WHERE ma.status_id = CASE WHEN $7 AND s.reblog_of_id IS NOT NULL
                                           THEN s.reblog_of_id ELSE s.id END))
          AND ($6::text IS NULL OR EXISTS (
                 SELECT 1 FROM status_tags st
                 JOIN tags t ON t.id = st.tag_id
                 WHERE st.status_id = s.id AND lower(t.name) = lower($6)))
          AND ($8::timestamptz IS NULL OR (s.sort_at, s.id) < ($8, $9::bigint))
          AND ($10::timestamptz IS NULL OR (s.sort_at, s.id) > ($10, $11::bigint))
        ORDER BY s.sort_at DESC, s.id DESC
        LIMIT $12
        "#,
        account_id,
        viewer,
        filter.exclude_replies,
        filter.exclude_reblogs,
        filter.only_media,
        filter.tagged.as_deref(),
        filter.media_through_reblog,
        max_sort_at,
        max_id,
        since_sort_at,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// One outbox page: the account's distributable statuses for `viewer`,
/// boosts included, newest first. Anonymous (`viewer: None`) gets the
/// public/unlisted slice; a signed requester additionally gets `private`
/// statuses if they are an accepted follower, and statuses that mention them
/// (however addressed, `direct` included) — Mastodon's
/// `AccountStatusesFilter` as the outbox uses it. Boosts of an author the
/// viewer blocks (or is blocked by) are withheld, like
/// `excluded_from_timeline_account_ids` — the federated slice of it; mutes
/// and the viewer's own domain blocks never federate, so there is nothing to
/// filter by. `local` visibility never leaves the instance. `min_id` selects
/// the rows immediately above it (Mastodon's `paginate_by_min_id`: ascending,
/// then reversed back to newest-first) and ignores `since_id`; otherwise
/// `max_id`/`since_id` bound a newest-first scan.
pub async fn outbox_page(
    pool: &PgPool,
    account_id: i64,
    viewer: Option<i64>,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    if let Some(min_id) = min_id {
        let mut statuses = sqlx::query_as!(
            Status,
            r#"
            SELECT id, uri, account_id, content, created_at, updated_at,
                   visibility, in_reply_to_id, reblog_of_id, edited_at,
                   spoiler_text, sensitive, language, url, quote_approval_policy, application_id, title, object_type, external_url
            FROM statuses
            WHERE account_id = $1
              AND deleted_at IS NULL -- STUBFILTER
              AND (visibility IN ('public', 'unlisted')
                   OR ($2::bigint IS NOT NULL AND (
                         (visibility = 'private'
                          AND EXISTS (SELECT 1 FROM follows f
                                      WHERE f.account_id = $2 AND f.target_account_id = $1
                                        AND NOT f.pending))
                      OR (visibility <> 'local'
                          AND EXISTS (SELECT 1 FROM status_mentions m
                                      WHERE m.status_id = statuses.id AND m.account_id = $2)))))
              AND ($2::bigint IS NULL OR reblog_of_id IS NULL
                   OR NOT EXISTS (SELECT 1 FROM blocks b
                                  JOIN statuses t ON t.id = statuses.reblog_of_id
                                  WHERE (b.account_id = $2 AND b.target_account_id = t.account_id)
                                     OR (b.account_id = t.account_id AND b.target_account_id = $2)))
              AND id > $3
              AND ($4::bigint IS NULL OR id < $4)
            ORDER BY id ASC
            LIMIT $5
            "#,
            account_id,
            viewer,
            min_id,
            max_id,
            limit,
        )
        .fetch_all(pool)
        .await?;
        statuses.reverse();
        return Ok(statuses);
    }
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at,
               visibility, in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text, sensitive, language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses
        WHERE account_id = $1
          AND deleted_at IS NULL -- STUBFILTER
          AND (visibility IN ('public', 'unlisted')
               OR ($2::bigint IS NOT NULL AND (
                     (visibility = 'private'
                      AND EXISTS (SELECT 1 FROM follows f
                                  WHERE f.account_id = $2 AND f.target_account_id = $1
                                    AND NOT f.pending))
                  OR (visibility <> 'local'
                      AND EXISTS (SELECT 1 FROM status_mentions m
                                  WHERE m.status_id = statuses.id AND m.account_id = $2)))))
          AND ($2::bigint IS NULL OR reblog_of_id IS NULL
               OR NOT EXISTS (SELECT 1 FROM blocks b
                              JOIN statuses t ON t.id = statuses.reblog_of_id
                              WHERE (b.account_id = $2 AND b.target_account_id = t.account_id)
                                 OR (b.account_id = t.account_id AND b.target_account_id = $2)))
          AND ($3::bigint IS NULL OR id < $3)
          AND ($4::bigint IS NULL OR id > $4)
        ORDER BY id DESC
        LIMIT $5
        "#,
        account_id,
        viewer,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// Number of local original statuses (nodeinfo `usage.localPosts` /
/// `/api/v1/instance` `stats.status_count`). Boosts are excluded; every
/// visibility counts. Callers sit behind response caching — this is a
/// full count, not an indexed probe.
pub async fn count_local(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           WHERE (s.uri IS NULL OR a.portable)
             AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL -- STUBFILTER"#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Root posts and replies authored by one account, excluding boosts. Lemmy's
/// person aggregates split these while Mastodon's account entity combines
/// them as `statuses_count`.
pub async fn count_posts_comments_by_account(
    pool: &PgPool,
    account_id: i64,
) -> Result<(i64, i64), DbError> {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT count(*) FILTER (WHERE in_reply_to_id IS NULL AND reblog_of_id IS NULL),
                count(*) FILTER (WHERE in_reply_to_id IS NOT NULL AND reblog_of_id IS NULL)
         FROM statuses WHERE account_id = $1 AND deleted_at IS NULL -- STUBFILTER",
    )
    .bind(account_id)
    .fetch_one(pool)
    .await
    .map_err(DbError::from)
}

/// Local root posts and replies, excluding group boosts. These are the two
/// independent counters Lemmy's site aggregate exposes.
pub async fn count_local_posts_comments(pool: &PgPool) -> Result<(i64, i64), DbError> {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT count(*) FILTER (WHERE s.in_reply_to_id IS NULL AND s.reblog_of_id IS NULL),
                count(*) FILTER (WHERE s.in_reply_to_id IS NOT NULL AND s.reblog_of_id IS NULL)
         FROM statuses s
         JOIN accounts a ON a.id = s.account_id
         WHERE (a.domain IS NULL OR a.portable)
           AND s.deleted_at IS NULL -- STUBFILTER",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::from)
}

/// Number of remote original statuses on record — the landing page's
/// "posts elsewhere we know of" counter. Same shape as [`count_local`]:
/// boosts excluded, every visibility counted, callers sit behind caching.
pub async fn count_remote(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           WHERE s.uri IS NOT NULL AND NOT a.portable
             AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL -- STUBFILTER"#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// What the outbox envelope advertises as `totalItems`: every status but
/// direct messages, like Mastodon's `statuses_count` counter cache.
pub async fn count_outbox_by_account(pool: &PgPool, account_id: i64) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM statuses
           WHERE account_id = $1 AND visibility NOT IN ('direct', 'local')
             AND deleted_at IS NULL -- STUBFILTER"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// A page of an account's statuses for the archive outbox — *every*
/// visibility (unlike [`outbox_page`], which is public/unlisted only), since a
/// personal backup includes private and direct posts. Keyset-paginated
/// ascending by id: pass `after_id = None` for the first page and the last id
/// seen for each subsequent page, matching Mastodon's `find_in_batches`.
pub async fn archive_page(
    pool: &PgPool,
    account_id: i64,
    after_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id, uri, account_id, content, created_at, updated_at,
               visibility, in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text, sensitive, language, url, quote_approval_policy, application_id, title, object_type, external_url
        FROM statuses
        WHERE account_id = $1 -- STUBKEEP: a full account backup retains stubs so a restore can rebuild trees
          AND ($2::bigint IS NULL OR id > $2)
        ORDER BY id ASC
        LIMIT $3
        "#,
        account_id,
        after_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// Parameters for [`search`]. Search is viewer-scoped: there is no
/// anonymous status search, matching Mastodon.
#[derive(Debug)]
pub struct StatusSearch {
    pub viewer: i64,
    /// Restrict to statuses authored by this account (`account_id` param).
    pub account_id: Option<i64>,
    pub max_id: Option<i64>,
    pub min_id: Option<i64>,
    pub limit: i64,
    pub offset: i64,
}

/// Newest matching originals the viewer-scoped filters are applied to.
///
/// Search is linear in *match count*, not corpus size: `account_hidden()` and
/// the visibility arms are evaluated per matched row, and the planner cannot
/// help — it estimates the same ~1,500 rows for a term matching 900 statuses
/// and one matching 200,000. Measured on the benchmark dataset, a common term
/// (42% of the corpus) cost **9.0 s** on a synchronous, user-facing endpoint
/// while a rare one cost 50 ms.
///
/// Bounding the *candidate* set instead is what makes the cost describe the
/// page rather than the term: the FTS index still finds every match, but only
/// the newest [`SEARCH_CANDIDATES`] of them are asked the expensive questions.
/// 2,000 is 100 pages deep — far past where any client keeps scrolling, and
/// wide enough that a viewer whose blocks hide most of a page still gets a
/// full one.
///
/// `max_id`/`min_id` are applied *inside* the window, so page 2 re-anchors its
/// own 2,000 candidates below the caller's cursor rather than paging inside a
/// window that was fixed at page 1 — without that, deep pagination would
/// silently stop at candidate 2,000.
const SEARCH_CANDIDATES: i64 = 2_000;

/// Full-text status search (`websearch_to_tsquery` semantics: quoted
/// phrases, `-exclusion`, `or`). Scope: originals the viewer may see —
/// public/unlisted posts, their own, `private` posts they follow the author
/// of, and posts (including DMs) that mention them.
///
/// Bounded by [`SEARCH_CANDIDATES`]; see there for why.
pub async fn search(
    pool: &PgPool,
    terms: &str,
    params: &StatusSearch,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        WITH candidates AS MATERIALIZED (
            SELECT c.id, c.account_id, c.visibility
            FROM statuses c
            WHERE websearch_to_tsquery('simple', $1) @@ to_tsvector('simple', c.content)
              AND c.reblog_of_id IS NULL
              AND c.deleted_at IS NULL -- STUBFILTER
              AND c.ingest_provenance <> 'history'
              AND ($3::bigint IS NULL OR c.account_id = $3)
              AND ($4::bigint IS NULL OR c.id < $4)
              AND ($5::bigint IS NULL OR c.id > $5)
            ORDER BY c.id DESC
            LIMIT $8
        )
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy, s.application_id,
                        s.title, s.object_type, s.external_url
        FROM candidates c
        JOIN statuses s ON s.id = c.id
        WHERE NOT account_hidden($2, c.account_id)
          AND (c.visibility IN ('public', 'unlisted')
               OR c.account_id = $2
               OR (c.visibility = 'local'
                   AND EXISTS (SELECT 1 FROM accounts author
                               WHERE author.id = c.account_id
                                 AND (author.domain IS NULL OR author.portable)))
               OR (c.visibility = 'private'
                   AND EXISTS (SELECT 1 FROM follows f
                               WHERE f.account_id = $2 AND f.target_account_id = c.account_id
                                 AND NOT f.pending))
               OR EXISTS (SELECT 1 FROM status_mentions m
                          WHERE m.status_id = c.id AND m.account_id = $2))
        ORDER BY s.id DESC
        LIMIT $6 OFFSET $7
        "#,
        terms,
        params.viewer,
        params.account_id,
        params.max_id,
        params.min_id,
        params.limit,
        params.offset,
        SEARCH_CANDIDATES,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

const THREAD_DEPTH_LIMIT: i32 = 40;

/// Ancestors of a status (the reply chain above it) in chain order, thread
/// root first. Ordered by walk depth, not id — a backfilled parent can carry
/// a larger id than its child.
pub async fn ancestors(pool: &PgPool, status_id: i64) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        WITH RECURSIVE thread AS (
            SELECT s.*, 1 AS depth FROM statuses s -- STUBKEEP: ancestors keep stubs so the tree holds; the renderer serves placeholders
            WHERE s.id = (SELECT in_reply_to_id FROM statuses WHERE id = $1)
            UNION ALL
            SELECT p.*, t.depth + 1 FROM statuses p
            JOIN thread t ON p.id = t.in_reply_to_id
            WHERE t.depth < $2
        )
        SELECT id AS "id!", uri, account_id AS "account_id!", content AS "content!",
               created_at AS "created_at!", updated_at AS "updated_at!",
               visibility AS "visibility!", in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text AS "spoiler_text!", sensitive AS "sensitive!", language, url,
               quote_approval_policy AS "quote_approval_policy!",
               application_id, title, object_type, external_url
        FROM thread ORDER BY depth DESC
        "#,
        status_id,
        THREAD_DEPTH_LIMIT,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// Descendants of a status (replies below it) in depth-first tree order —
/// each reply directly followed by its own replies, branches in id order.
/// This is Mastodon's context shape (its recursive CTE `ORDER BY path`);
/// callers replicating Mastodon then hoist the root author's continuations via
/// [`promote_self_replies`].
pub async fn descendants(pool: &PgPool, status_id: i64) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        WITH RECURSIVE replies AS (
            SELECT s.*, 1 AS depth, ARRAY[s.id] AS path
            FROM statuses s WHERE s.in_reply_to_id = $1 -- STUBKEEP: descendants keep stubs so the tree holds; the renderer serves placeholders
            UNION ALL
            SELECT c.*, r.depth + 1, r.path || c.id FROM statuses c
            JOIN replies r ON c.in_reply_to_id = r.id
            WHERE r.depth < $2
        )
        SELECT id AS "id!", uri, account_id AS "account_id!", content AS "content!",
               created_at AS "created_at!", updated_at AS "updated_at!",
               visibility AS "visibility!", in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text AS "spoiler_text!", sensitive AS "sensitive!", language, url,
               quote_approval_policy AS "quote_approval_policy!",
               application_id, title, object_type, external_url
        FROM replies ORDER BY path
        "#,
        status_id,
        THREAD_DEPTH_LIMIT,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// The whole conversation around a status in flat arrival order (ascending
/// local id), split at the focal post — Pleroma's context shape. Membership is
/// the status' *conversation* (the AP `context`/`conversation` grouping Plamenu
/// records at ingest via [`crate::conversation::ensure_for_status`]), matching
/// Pleroma's `fetch_activities_for_context` (group by `context`, `ORDER BY id`)
/// + `context.json` (split `id < focal → ancestor`). Because membership is the
///   context IRI and not the reply chain, a thread whose middle post we never
///   received still coheres — the orphaned subtree shares the context — where a
///   reply-tree walk would drop it. Boosts are excluded (Pleroma lists only
///   `Create`s). A status with no recorded conversation falls back to the
///   reply-tree walk ([`thread_flat_by_tree`]).
pub async fn thread_flat(
    pool: &PgPool,
    status_id: i64,
) -> Result<(Vec<Status>, Vec<Status>), DbError> {
    let statuses = match crate::conversation::of_status(pool, status_id).await? {
        Some(conversation_id) => {
            sqlx::query_as!(
                Status,
                r#"
                SELECT s.id AS "id!", s.uri, s.account_id AS "account_id!",
                       s.content AS "content!", s.created_at AS "created_at!",
                       s.updated_at AS "updated_at!", s.visibility AS "visibility!",
                       s.in_reply_to_id, s.reblog_of_id, s.edited_at,
                       s.spoiler_text AS "spoiler_text!", s.sensitive AS "sensitive!",
                       s.language, s.url,
                       s.quote_approval_policy AS "quote_approval_policy!",
                       s.application_id, s.title, s.object_type, s.external_url
                FROM statuses s
                JOIN status_conversations sc ON sc.status_id = s.id
                WHERE sc.conversation_id = $2
                  AND s.reblog_of_id IS NULL
                  AND s.deleted_at IS NULL -- STUBFILTER: flat mode drops deleted posts, like Pleroma
                  AND s.id <> $1
                ORDER BY s.id
                "#,
                status_id,
                conversation_id,
            )
            .fetch_all(pool)
            .await?
        }
        None => thread_flat_by_tree(pool, status_id).await?,
    };
    let mut ancestors = statuses;
    let descendants = ancestors.split_off(ancestors.partition_point(|s| s.id < status_id));
    Ok((ancestors, descendants))
}

/// Reply-tree fallback for [`thread_flat`], used only when a status carries no
/// conversation mapping (every status ingested since the conversation model
/// landed has one, so this is defensive). Walks up to the thread root (bounded
/// like [`ancestors`]) and back down through the reply tree, flattened by id —
/// the pre-conversation flat shape.
async fn thread_flat_by_tree(pool: &PgPool, status_id: i64) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        WITH RECURSIVE up AS (
            SELECT s.id, s.in_reply_to_id, 1 AS depth FROM statuses s WHERE s.id = $1 -- STUBKEEP: flat mode keeps the walk; its page query drops stubs
            UNION ALL
            SELECT p.id, p.in_reply_to_id, u.depth + 1 FROM statuses p
            JOIN up u ON p.id = u.in_reply_to_id
            WHERE u.depth < $2
        ),
        thread AS (
            SELECT s.*, 1 AS depth FROM statuses s
            WHERE s.id = (SELECT id FROM up ORDER BY depth DESC LIMIT 1)
            UNION ALL
            SELECT c.*, t.depth + 1 FROM statuses c
            JOIN thread t ON c.in_reply_to_id = t.id
            WHERE t.depth < (SELECT max(depth) FROM up) + $2
        )
        SELECT id AS "id!", uri, account_id AS "account_id!", content AS "content!",
               created_at AS "created_at!", updated_at AS "updated_at!",
               visibility AS "visibility!", in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text AS "spoiler_text!", sensitive AS "sensitive!", language, url,
               quote_approval_policy AS "quote_approval_policy!",
               application_id, title, object_type, external_url
        FROM thread WHERE id <> $1 ORDER BY id
        "#,
        status_id,
        THREAD_DEPTH_LIMIT,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// Which members of `descendants` continue the thread root author's own
/// uninterrupted chain.
///
/// Mastodon represents that distinction through carried-over semantics on
/// `in_reply_to_account_id`: when somebody replies to their own reply to a
/// different author, the different author's id is retained. Plamenu's column
/// deliberately identifies the direct parent's author instead, so reproduce
/// the carried-over predicate from the tree itself. A descendant qualifies
/// only when it has the root's author and its parent already belongs to the
/// root's continuation. This keeps a participant's nested self-reply in its
/// branch rather than promoting it as though the participant wrote the root.
///
/// `ancestors` must be root-first and `descendants` parent-before-child, the
/// orders returned by [`ancestors`] and [`descendants`]. If the oldest loaded
/// ancestor still has a parent, the depth limit hid the real root; fail closed
/// rather than promoting against a guessed root. Compute this over the
/// unfiltered tree, then promote the filtered descendant list.
#[must_use]
pub fn root_self_reply_ids(
    ancestors: &[Status],
    focal: &Status,
    descendants: &[Status],
) -> HashSet<i64> {
    let root = ancestors.first().unwrap_or(focal);
    if root.in_reply_to_id.is_some() {
        return HashSet::new();
    }

    let root_account_id = root.account_id;
    let capacity = ancestors
        .len()
        .saturating_add(descendants.len())
        .saturating_add(1);
    let mut continuation = HashSet::with_capacity(capacity);
    continuation.insert(root.id);

    for status in ancestors.iter().skip(1).chain(std::iter::once(focal)) {
        if status.account_id == root_account_id
            && status
                .in_reply_to_id
                .is_some_and(|parent| continuation.contains(&parent))
        {
            continuation.insert(status.id);
        }
    }

    let mut promoted = HashSet::new();
    for status in descendants {
        if status.account_id == root_account_id
            && status
                .in_reply_to_id
                .is_some_and(|parent| continuation.contains(&parent))
        {
            continuation.insert(status.id);
            promoted.insert(status.id);
        }
    }
    promoted
}

/// Bring root-author self-replies to the top of a descendant list, preserving
/// relative order on both sides of the split. A two-bucket stable partition
/// avoids the quadratic repeated `Vec::remove`/`Vec::insert` form of Mastodon's
/// `promote_by!` (`Status::ThreadingConcern`).
pub fn promote_self_replies<S: std::hash::BuildHasher>(
    statuses: &mut Vec<Status>,
    self_replies: &HashSet<i64, S>,
) {
    let promoted_len = statuses
        .iter()
        .filter(|status| self_replies.contains(&status.id))
        .count();
    if promoted_len == 0 || promoted_len == statuses.len() {
        return;
    }

    let original = std::mem::take(statuses);
    let mut promoted = Vec::with_capacity(promoted_len);
    let mut remaining = Vec::with_capacity(original.len() - promoted_len);
    for status in original {
        if self_replies.contains(&status.id) {
            promoted.push(status);
        } else {
            remaining.push(status);
        }
    }
    promoted.append(&mut remaining);
    *statuses = promoted;
}

/// Reply/boost/favourite/quote/dislike counters for a batch of statuses.
#[derive(Debug, Clone, Copy, Default)]
pub struct Engagement {
    pub replies: i64,
    pub reblogs: i64,
    pub favourites: i64,
    pub quotes: i64,
    /// Downvotes (group votes); a group post's score is
    /// `favourites - dislikes`. Zero outside group contexts.
    pub dislikes: i64,
}

pub async fn engagement_for(
    pool: &PgPool,
    ids: &[i64],
) -> Result<HashMap<i64, Engagement>, DbError> {
    let mut conn = pool.acquire().await?;
    engagement_for_conn(&mut conn, ids).await
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn engagement_for_conn(
    conn: &mut sqlx::PgConnection,
    ids: &[i64],
) -> Result<HashMap<i64, Engagement>, DbError> {
    let mut map: HashMap<i64, Engagement> = HashMap::with_capacity(ids.len());
    let replies = sqlx::query!(
        r#"SELECT in_reply_to_id AS "sid!", count(*) AS "count!"
           FROM statuses
           WHERE in_reply_to_id = ANY($1) AND deleted_at IS NULL -- STUBFILTER
           GROUP BY 1"#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in replies {
        map.entry(row.sid).or_default().replies = row.count;
    }
    let reblogs = sqlx::query!(
        r#"SELECT reblog_of_id AS "sid!", count(*) AS "count!"
           FROM statuses WHERE reblog_of_id = ANY($1) GROUP BY 1 -- STUBKEEP: a boost wrapper never stubs"#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in reblogs {
        map.entry(row.sid).or_default().reblogs = row.count;
    }
    let favourites = sqlx::query!(
        r#"SELECT status_id AS "sid!", count(*) AS "count!"
           FROM favourites WHERE status_id = ANY($1) GROUP BY 1"#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in favourites {
        map.entry(row.sid).or_default().favourites = row.count;
    }
    let dislikes = sqlx::query!(
        r#"SELECT status_id AS "sid!", count(*) AS "count!"
           FROM status_dislikes WHERE status_id = ANY($1) GROUP BY 1"#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in dislikes {
        map.entry(row.sid).or_default().dislikes = row.count;
    }
    // Accepted quotes only, matching Mastodon's `quotes_count` counter cache.
    let quotes = sqlx::query!(
        r#"SELECT quoted_status_id AS "sid!", count(*) AS "count!"
           FROM quotes
           WHERE quoted_status_id = ANY($1) AND state = 'accepted'
           GROUP BY 1"#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in quotes {
        map.entry(row.sid).or_default().quotes = row.count;
    }
    Ok(map)
}

/// Which of `ids` the viewer has boosted.
pub async fn reblogged_of(
    pool: &PgPool,
    account_id: i64,
    ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT reblog_of_id AS "id!" FROM statuses -- STUBKEEP: a boost wrapper never stubs
           WHERE account_id = $1 AND reblog_of_id = ANY($2)"#,
        account_id,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// [`reblogged_of`] across a set of viewers in one query — `(viewer, boosted
/// status)` pairs where the viewer boosted the status.
pub async fn reblogged_of_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    ids: &[i64],
) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id, reblog_of_id AS "reblog_of_id!"
           FROM statuses -- STUBKEEP: a boost wrapper never stubs
           WHERE account_id = ANY($1) AND reblog_of_id = ANY($2)"#,
        viewer_ids,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.account_id, row.reblog_of_id))
        .collect())
}

/// One entry of a status's `reblogged_by` listing: the boost row
/// (pagination key) and who boosted.
#[derive(Debug, Clone, Copy)]
pub struct RebloggerEntry {
    pub row_id: i64,
    pub account_id: i64,
}

/// Accounts that boosted a status, newest boost first, keyset-paginated by
/// the boost's status id. Only distributable (public/unlisted) boosts are
/// listed, and accounts hidden from the viewer (a block in either
/// direction, or an active mute) are excluded — Mastodon's
/// `distributable_visibility` + `not_excluded_by_account`.
pub async fn rebloggers_of(
    pool: &PgPool,
    status_id: i64,
    viewer: Option<i64>,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<RebloggerEntry>, DbError> {
    let entries = sqlx::query_as!(
        RebloggerEntry,
        r#"
        SELECT id AS "row_id!", account_id AS "account_id!"
        FROM statuses -- STUBKEEP: a boost wrapper never stubs
        WHERE reblog_of_id = $1
          AND visibility IN ('public', 'unlisted')
          AND ($2::bigint IS NULL OR id < $2)
          AND ($3::bigint IS NULL OR id > $3)
          AND NOT account_hidden($4, account_id)
        ORDER BY id DESC
        LIMIT $5
        "#,
        status_id,
        max_id,
        since_id,
        viewer,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

/// Local accounts that currently boost a status, ascending — the audience
/// of an `update` notification when the status is edited (Mastodon's
/// `notify_about_update!`).
pub async fn local_rebloggers_of(pool: &PgPool, status_id: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT s.account_id
        FROM statuses s -- STUBKEEP: a boost wrapper never stubs
        JOIN accounts a ON a.id = s.account_id
        WHERE s.reblog_of_id = $1 AND a.domain IS NULL
        ORDER BY s.account_id
        "#,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Number of statuses by an account (entity counters).
/// When the account last posted (Mastodon's `account_stat.last_status_at`):
/// the newest live status row's creation time, so deletions roll it back
/// where Mastodon's cached stat would not.
pub async fn last_created_at(
    pool: &PgPool,
    account_id: i64,
) -> Result<Option<OffsetDateTime>, DbError> {
    let at = sqlx::query_scalar!(
        r#"SELECT max(created_at) FROM statuses
           WHERE account_id = $1 AND deleted_at IS NULL -- STUBFILTER"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(at)
}

pub async fn count_by_account(pool: &PgPool, account_id: i64) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM statuses
           WHERE account_id = $1 AND deleted_at IS NULL -- STUBFILTER"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// The status count an account renders with, for every id in `account_ids`
/// in one query: the origin-advertised outbox total when one has been
/// synced (remote accounts, `accounts.remote_statuses_count`), else the
/// count of locally-stored statuses. Ids without an account row are absent
/// from the map (treat as 0).
pub async fn count_by_account_batch(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<HashMap<i64, u64>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT a.id AS "id!", COALESCE(a.remote_statuses_count, s.cnt, 0) AS "count!"
        FROM accounts a
        LEFT JOIN (
            SELECT account_id, count(*) AS cnt
            FROM statuses
            WHERE account_id = ANY($1) AND deleted_at IS NULL -- STUBFILTER
            GROUP BY 1
        ) s ON s.account_id = a.id
        WHERE a.id = ANY($1)
        "#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, u64::try_from(row.count).unwrap_or(0)))
        .collect())
}

/// [`last_created_at`] for every id in `account_ids` in one query; ids with
/// no statuses are absent from the map (treat as `None`).
pub async fn last_created_at_batch(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<HashMap<i64, OffsetDateTime>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id AS "id!", max(created_at) AS "at!"
           FROM statuses
           WHERE account_id = ANY($1) AND deleted_at IS NULL -- STUBFILTER
           GROUP BY 1"#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| (row.id, row.at)).collect())
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::account::{self, NewLocalAccount, RemoteAccountData};

    async fn local_account(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn search_is_visibility_scoped_and_filterable(pool: PgPool) {
        let author = local_account(&pool, "author").await;
        let viewer = local_account(&pool, "viewer").await;
        let public = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>winter swimming</p>", "public", None),
        )
        .await
        .unwrap();
        let hidden = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>winter secrets</p>", "private", None),
        )
        .await
        .unwrap();
        let own = create_local(
            &pool,
            NewLocalStatus::new(viewer, "<p>winter is mine</p>", "private", None),
        )
        .await
        .unwrap();
        // Markup never matches: "p" appears in every tag.
        create_local(
            &pool,
            NewLocalStatus::new(author, "<p>unrelated</p>", "public", None),
        )
        .await
        .unwrap();

        let params = |account_id, max_id| StatusSearch {
            viewer,
            account_id,
            max_id,
            min_id: None,
            limit: 10,
            offset: 0,
        };

        // The viewer sees public posts and their own private one, not the
        // author's private post.
        let hits = search(&pool, "winter", &params(None, None)).await.unwrap();
        let ids: Vec<i64> = hits.iter().map(|s| s.id).collect();
        assert_eq!(ids, [own.id, public.id]);

        // Following the author makes their private post visible.
        crate::follow::create(&pool, viewer, author, None)
            .await
            .unwrap();
        let hits = search(&pool, "winter", &params(None, None)).await.unwrap();
        assert_eq!(hits.len(), 3);

        // `account_id` and keyset filters narrow the result.
        let hits = search(&pool, "winter", &params(Some(author), None))
            .await
            .unwrap();
        assert_eq!(
            hits.iter().map(|s| s.id).collect::<Vec<_>>(),
            [hidden.id, public.id]
        );
        let hits = search(&pool, "winter", &params(None, Some(public.id)))
            .await
            .unwrap();
        assert!(hits.is_empty(), "max_id excludes everything newer");

        // Phrase queries must match in order.
        let hits = search(&pool, "\"swimming winter\"", &params(None, None))
            .await
            .unwrap();
        assert!(hits.is_empty());
        let hits = search(&pool, "\"winter swimming\"", &params(None, None))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);

        // Markup is not searchable.
        let hits = search(&pool, "p", &params(None, None)).await.unwrap();
        assert!(hits.is_empty());
    }

    async fn remote_account(pool: &PgPool) -> i64 {
        account::upsert_remote(
            pool,
            RemoteAccountData {
                username: "bob",
                domain: "remote.example",
                uri: "https://remote.example/users/bob",
                display_name: "",
                note: "",
                inbox_url: "https://remote.example/users/bob/inbox",
                shared_inbox_url: "",
                public_key_pem: "pub",
                public_key_id: "https://remote.example/users/bob#main-key",
                avatar_remote_url: None,
                header_remote_url: None,
                avatar_description: "",
                header_description: "",
                created_at: None,
                fields: Vec::new(),
                featured_collection_url: None,
                locked: false,
                also_known_as: &[],
                moved_to_uri: None,
                url: None,
                discoverable: false,
                feature_approval_policy: 0,
                is_bot: false,
                indexable: false,
                show_media: None,
                show_media_replies: None,
                show_featured: None,
                memorial: false,
                actor_type: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// Runs the public timeline under both orderings and asserts they select
    /// the same rows — the WHERE bodies are duplicated per ordering and must
    /// not drift. Returns the `published` (default) result.
    async fn public_timeline_both(
        pool: &PgPool,
        local_only: bool,
        viewer: Option<i64>,
        include_replies: bool,
        max_id: Option<i64>,
        limit: i64,
    ) -> Vec<Status> {
        let published = public_timeline(
            pool,
            local_only,
            viewer,
            include_replies,
            TimelineOrder::Published,
            max_id,
            limit,
        )
        .await
        .unwrap();
        let received = public_timeline(
            pool,
            local_only,
            viewer,
            include_replies,
            TimelineOrder::Received,
            max_id,
            limit,
        )
        .await
        .unwrap();
        let mut a: Vec<i64> = published.iter().map(|s| s.id).collect();
        let mut b: Vec<i64> = received.iter().map(|s| s.id).collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "both orderings must select the same rows");
        published
    }

    fn remote_note<'a>(uri: &'a str, account_id: i64, content: &'a str) -> NewRemoteStatus<'a> {
        NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri,
            account_id,
            content,
            created_at: datetime!(2026-06-01 12:00 UTC),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        }
    }

    #[sqlx::test]
    async fn cold_history_is_profile_only_until_delivery_claim_promotes_it(pool: PgPool) {
        let author = remote_account(&pool).await;
        let viewer = local_account(&pool, "viewer").await;
        let uri = "https://remote.example/users/bob/statuses/history-1";
        let cold = upsert_remote_with_provenance(
            &pool,
            remote_note(uri, author, "<p>remote history needle</p>"),
            IngestProvenance::History,
        )
        .await
        .unwrap();
        assert_eq!(
            ingest_provenance(&pool, cold.id).await.unwrap().as_deref(),
            Some("history")
        );

        let profile = by_account(
            &pool,
            author,
            Some(viewer),
            &AccountStatusesFilter::default(),
            TimelineOrder::Published,
            20,
        )
        .await
        .unwrap();
        assert_eq!(
            profile.iter().map(|row| row.id).collect::<Vec<_>>(),
            [cold.id]
        );
        crate::remote_history::set_actor_metadata(
            &pool,
            author,
            Some("https://remote.example/users/bob/outbox"),
            Some(true),
        )
        .await
        .unwrap();
        assert!(
            by_account(
                &pool,
                author,
                None,
                &AccountStatusesFilter::default(),
                TimelineOrder::Published,
                20,
            )
            .await
            .unwrap()
            .is_empty(),
            "positive GoToSocial audience hints hide cold rows from signed-out web"
        );
        assert!(
            public_timeline(
                &pool,
                false,
                Some(viewer),
                true,
                TimelineOrder::Published,
                None,
                20,
            )
            .await
            .unwrap()
            .is_empty(),
            "cold rows do not enter public discovery"
        );
        crate::follow::create(&pool, viewer, author, None)
            .await
            .unwrap();
        assert!(
            home_timeline(&pool, viewer, TimelineOrder::Published, None, 20)
                .await
                .unwrap()
                .is_empty(),
            "following the actor does not inject their cold history into home"
        );
        let list = crate::list::create(&pool, viewer, "remote", "list", false)
            .await
            .unwrap();
        crate::list::add_members(&pool, list.id, viewer, &[author])
            .await
            .unwrap()
            .unwrap();
        assert!(
            crate::list::timeline(&pool, &list, TimelineOrder::Published, None, 20)
                .await
                .unwrap()
                .is_empty(),
            "cold history does not enter list timelines"
        );
        let tag_id = crate::tag::ensure(&pool, "hydrated").await.unwrap();
        crate::tag::attach(&pool, cold.id, tag_id).await.unwrap();
        assert!(
            crate::tag::timeline(
                &pool,
                "hydrated",
                Some(viewer),
                TimelineOrder::Published,
                None,
                20,
            )
            .await
            .unwrap()
            .is_empty(),
            "cold history does not enter hashtag timelines"
        );
        assert!(
            search(
                &pool,
                "needle",
                &StatusSearch {
                    viewer,
                    account_id: None,
                    max_id: None,
                    min_id: None,
                    limit: 20,
                    offset: 0,
                },
            )
            .await
            .unwrap()
            .is_empty(),
            "cold rows are not indexed by v1 search"
        );

        assert!(claim_delivery_side_effects(&pool, cold.id).await.unwrap());
        assert!(
            !claim_delivery_side_effects(&pool, cold.id).await.unwrap(),
            "only one concurrent delivery path may own effects"
        );
        assert_eq!(
            ingest_provenance(&pool, cold.id).await.unwrap().as_deref(),
            Some("delivery")
        );
        let public = public_timeline(
            &pool,
            false,
            Some(viewer),
            true,
            TimelineOrder::Published,
            None,
            20,
        )
        .await
        .unwrap();
        assert_eq!(
            public.iter().map(|row| row.id).collect::<Vec<_>>(),
            [cold.id]
        );
    }

    #[sqlx::test]
    async fn concurrent_new_deliveries_claim_side_effects_once(pool: PgPool) {
        let author = remote_account(&pool).await;
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..50 {
            let pool = pool.clone();
            tasks.spawn(async move {
                insert_remote_delivery_claimed(
                    &pool,
                    remote_note(
                        "https://remote.example/users/bob/statuses/racing-delivery",
                        author,
                        "racing delivery",
                    ),
                )
                .await
                .unwrap()
                .is_some()
            });
        }
        let mut winners = 0;
        while let Some(result) = tasks.join_next().await {
            winners += usize::from(result.unwrap());
        }
        assert_eq!(winners, 1, "the URI insert atomically owns side effects");
        let stored = find_by_uri(
            &pool,
            "https://remote.example/users/bob/statuses/racing-delivery",
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            !claim_delivery_side_effects(&pool, stored.id).await.unwrap(),
            "the insert winner already persisted the delivery marker"
        );
    }

    #[sqlx::test]
    async fn provenance_is_monotonic_on_one_canonical_uri(pool: PgPool) {
        let author = remote_account(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/provenance";
        let history = upsert_remote_with_provenance(
            &pool,
            remote_note(uri, author, "history"),
            IngestProvenance::History,
        )
        .await
        .unwrap();
        let resolved = upsert_remote_with_provenance(
            &pool,
            remote_note(uri, author, "resolution"),
            IngestProvenance::ExplicitResolution,
        )
        .await
        .unwrap();
        assert_eq!(resolved.id, history.id);
        assert_eq!(
            ingest_provenance(&pool, history.id)
                .await
                .unwrap()
                .as_deref(),
            Some("explicit_resolution")
        );
        let replayed_history = upsert_remote_with_provenance(
            &pool,
            remote_note(uri, author, "history replay"),
            IngestProvenance::History,
        )
        .await
        .unwrap();
        assert_eq!(replayed_history.id, history.id);
        assert_eq!(
            ingest_provenance(&pool, history.id)
                .await
                .unwrap()
                .as_deref(),
            Some("explicit_resolution"),
            "a weaker discovery path never demotes provenance"
        );
        assert!(
            public_timeline(
                &pool,
                false,
                None,
                false,
                TimelineOrder::Published,
                None,
                20,
            )
            .await
            .unwrap()
            .is_empty(),
            "exact object resolution is searchable but not public-feed injection"
        );
        let delivered = upsert_remote_with_provenance(
            &pool,
            remote_note(uri, author, "delivery"),
            IngestProvenance::Delivery,
        )
        .await
        .unwrap();
        assert_eq!(delivered.id, history.id);
        assert_eq!(
            ingest_provenance(&pool, history.id)
                .await
                .unwrap()
                .as_deref(),
            Some("delivery")
        );
        let rows = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM statuses WHERE uri = $1 -- STUBKEEP: uniqueness assertion includes soft-delete stubs"#,
            uri,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 1);
    }

    #[sqlx::test]
    async fn remote_status_lifecycle(pool: PgPool) {
        let account_id = remote_account(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/1";

        let status = upsert_remote(&pool, remote_note(uri, account_id, "<p>hello</p>"))
            .await
            .unwrap();
        assert_eq!(status.content, "<p>hello</p>");
        assert_eq!(status.visibility, "public");

        // Re-delivery keeps the original row.
        let again = upsert_remote(&pool, remote_note(uri, account_id, "<p>changed</p>"))
            .await
            .unwrap();
        assert_eq!(again.id, status.id);
        assert_eq!(again.content, "<p>hello</p>");

        // The owner can delete it; a different account id cannot.
        assert!(!delete_by_uri(&pool, uri, account_id + 1).await.unwrap());
        assert!(delete_by_uri(&pool, uri, account_id).await.unwrap());
        assert!(find_by_uri(&pool, uri).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn replies_thread_and_context(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let reply = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>reply</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let nested = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>nested</p>", "public", Some(reply.id)),
        )
        .await
        .unwrap();

        let up = ancestors(&pool, nested.id).await.unwrap();
        assert_eq!(
            up.iter().map(|s| s.id).collect::<Vec<_>>(),
            [root.id, reply.id]
        );
        let down = descendants(&pool, root.id).await.unwrap();
        assert_eq!(
            down.iter().map(|s| s.id).collect::<Vec<_>>(),
            [reply.id, nested.id]
        );
        assert!(ancestors(&pool, root.id).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn thread_orders_tree_and_flat(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let r1 = create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>r1</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let r1b = create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>r1b</p>", "public", Some(r1.id)),
        )
        .await
        .unwrap();
        let r2 = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>r2</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let r2a = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>r2a</p>", "public", Some(r2.id)),
        )
        .await
        .unwrap();
        let r1a = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>r1a</p>", "public", Some(r1.id)),
        )
        .await
        .unwrap();

        // Tree: depth-first — r1's branch runs to completion before the
        // later sibling r2, even though r2's id sorts between them.
        let down = descendants(&pool, root.id).await.unwrap();
        assert_eq!(
            down.iter().map(|s| s.id).collect::<Vec<_>>(),
            [r1.id, r1b.id, r1a.id, r2.id, r2a.id]
        );

        // Mastodon's promotion intent: r2 continues the root author. Although
        // r1b answers another bob post, that participant self-reply remains in
        // its branch because the branch did not originate at bob's own root.
        let self_replies = root_self_reply_ids(&[], &root, &down);
        assert_eq!(self_replies, HashSet::from([r2.id, r2a.id]));
        let mut promoted = down.clone();
        promote_self_replies(&mut promoted, &self_replies);
        assert_eq!(
            promoted.iter().map(|s| s.id).collect::<Vec<_>>(),
            [r2.id, r2a.id, r1.id, r1b.id, r1a.id]
        );

        // Opening the participant's branch still resolves alice's actual root;
        // the focal post does not become a synthetic root for promotion.
        let up = ancestors(&pool, r1.id).await.unwrap();
        let branch = descendants(&pool, r1.id).await.unwrap();
        assert!(root_self_reply_ids(&up, &r1, &branch).is_empty());

        // Flat from the root: everything below in arrival order, unsplit.
        let (up, down) = thread_flat(&pool, root.id).await.unwrap();
        assert!(up.is_empty());
        assert_eq!(
            down.iter().map(|s| s.id).collect::<Vec<_>>(),
            [r1.id, r1b.id, r2.id, r2a.id, r1a.id]
        );

        // Flat from a leaf: ancestors are the whole older conversation —
        // sibling branch r2 included — not just the reply chain.
        let (up, down) = thread_flat(&pool, r1a.id).await.unwrap();
        assert_eq!(
            up.iter().map(|s| s.id).collect::<Vec<_>>(),
            [root.id, r1.id, r1b.id, r2.id, r2a.id]
        );
        assert!(down.is_empty());
    }

    /// Flat mode groups by the status' *conversation* (Pleroma's context
    /// grouping), not the reply tree: members of other conversations and
    /// boosts are excluded even when their ids fall within range.
    #[sqlx::test]
    async fn thread_flat_groups_by_conversation(pool: PgPool) {
        use crate::conversation::{self, ContextRefs, EnsureConversation};
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let carol = local_account(&pool, "carol").await;

        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let r1 = create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>r1</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        // A post in a *different* conversation whose id sorts between members.
        let other = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>other</p>", "public", None),
        )
        .await
        .unwrap();
        let r2 = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>r2</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let boost = create_local_reblog(&pool, carol, root.id).await.unwrap();

        // Map the thread into one conversation, exactly as the server action
        // layer does at ingest (the DB-level `create_local` does not).
        let ensure = |s: &Status, is_reply: bool| EnsureConversation {
            status_id: s.id,
            account_id: s.account_id,
            in_reply_to_id: s.in_reply_to_id,
            is_reply,
            refs: ContextRefs::default(),
        };
        let conv = conversation::ensure_for_status(&pool, &ensure(&root, false))
            .await
            .unwrap();
        conversation::ensure_for_status(&pool, &ensure(&r1, true))
            .await
            .unwrap();
        conversation::ensure_for_status(&pool, &ensure(&r2, true))
            .await
            .unwrap();
        let other_conv = conversation::ensure_for_status(&pool, &ensure(&other, false))
            .await
            .unwrap();
        assert_ne!(conv, other_conv, "`other` roots its own conversation");
        // Force the boost into the same conversation to prove it is filtered by
        // `reblog_of_id`, not by conversation membership.
        sqlx::query!(
            "INSERT INTO status_conversations (status_id, conversation_id) VALUES ($1, $2)",
            boost.id,
            conv,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Flat from the root: the two replies, ascending id, split at the
        // focal. `other` (different conversation) and the boost are excluded.
        let (up, down) = thread_flat(&pool, root.id).await.unwrap();
        assert!(up.is_empty());
        assert_eq!(
            down.iter().map(|s| s.id).collect::<Vec<_>>(),
            [r1.id, r2.id]
        );

        // From r1 the split moves: root becomes an ancestor, r2 a descendant.
        let (up, down) = thread_flat(&pool, r1.id).await.unwrap();
        assert_eq!(up.iter().map(|s| s.id).collect::<Vec<_>>(), [root.id]);
        assert_eq!(down.iter().map(|s| s.id).collect::<Vec<_>>(), [r2.id]);
    }

    /// The headline fix: a thread whose middle post we never received still
    /// coheres in flat mode, because membership is the shared context IRI, not
    /// the (broken) reply chain — whereas the Mastodon tree walk drops it.
    #[sqlx::test]
    async fn thread_flat_survives_missing_middle(pool: PgPool) {
        use crate::conversation::{self, ContextRefs, EnsureConversation};
        let bob = remote_account(&pool).await;
        let ctx = "https://remote.example/contexts/9";
        let refs = ContextRefs {
            context_uri: Some(ctx),
            history_uri: None,
        };

        // A remote root and a remote leaf sharing the context IRI; the middle
        // post connecting them was never received, so no in_reply_to resolves.
        let root = upsert_remote(
            &pool,
            remote_note("https://remote.example/s/root", bob, "<p>root</p>"),
        )
        .await
        .unwrap();
        let leaf = upsert_remote(
            &pool,
            remote_note("https://remote.example/s/leaf", bob, "<p>leaf</p>"),
        )
        .await
        .unwrap();
        let conv = conversation::ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: root.id,
                account_id: bob,
                in_reply_to_id: None,
                is_reply: false,
                refs,
            },
        )
        .await
        .unwrap();
        let leaf_conv = conversation::ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: leaf.id,
                account_id: bob,
                in_reply_to_id: None, // parent never received
                is_reply: true,
                refs,
            },
        )
        .await
        .unwrap();
        assert_eq!(conv, leaf_conv, "shared context IRI converges the thread");

        // Flat mode lists the orphaned leaf as a descendant of the root...
        let (up, down) = thread_flat(&pool, root.id).await.unwrap();
        assert!(up.is_empty());
        assert_eq!(down.iter().map(|s| s.id).collect::<Vec<_>>(), [leaf.id]);

        // ...whereas the reply-tree walk (tree mode) can't bridge the gap.
        let tree_down = descendants(&pool, root.id).await.unwrap();
        assert!(
            tree_down.is_empty(),
            "tree mode drops the missing-middle subtree"
        );
    }

    /// GtS-style stub: deleting a middle post keeps a placeholder row so the
    /// tree (Mastodon `descendants`) still reaches the grandchild — the defect
    /// fix. Contrast: a hard delete would SET NULL the child's parent link.
    #[sqlx::test]
    async fn stub_keeps_subtree_in_tree(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let mid = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>mid</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let leaf = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>leaf</p>", "public", Some(mid.id)),
        )
        .await
        .unwrap();

        let stubbed = stub_local(&pool, mid.id, alice)
            .await
            .unwrap()
            .expect("stubbed");
        assert_eq!(stubbed.content, "", "content nulled");
        assert!(is_deleted(&pool, mid.id).await.unwrap());
        // The row survives, so the reply chain stays connected.
        assert!(find_by_id(&pool, mid.id).await.unwrap().is_some());
        let down = descendants(&pool, root.id).await.unwrap();
        assert_eq!(
            down.iter().map(|s| s.id).collect::<Vec<_>>(),
            [mid.id, leaf.id],
            "tree keeps the stub as a connector so the grandchild survives",
        );
    }

    /// Flat mode (Pleroma) *drops* a deleted post from the context entirely,
    /// where tree mode keeps a placeholder — the faithful contrast.
    #[sqlx::test]
    async fn flat_mode_drops_deleted_post(pool: PgPool) {
        use crate::conversation::{self, ContextRefs, EnsureConversation};
        let alice = local_account(&pool, "alice").await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let mid = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>mid</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let leaf = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>leaf</p>", "public", Some(mid.id)),
        )
        .await
        .unwrap();
        let ensure = |s: &Status, is_reply: bool| EnsureConversation {
            status_id: s.id,
            account_id: s.account_id,
            in_reply_to_id: s.in_reply_to_id,
            is_reply,
            refs: ContextRefs::default(),
        };
        conversation::ensure_for_status(&pool, &ensure(&root, false))
            .await
            .unwrap();
        conversation::ensure_for_status(&pool, &ensure(&mid, true))
            .await
            .unwrap();
        conversation::ensure_for_status(&pool, &ensure(&leaf, true))
            .await
            .unwrap();

        stub_local(&pool, mid.id, alice).await.unwrap();

        let (up, down) = thread_flat(&pool, root.id).await.unwrap();
        assert!(up.is_empty());
        assert_eq!(
            down.iter().map(|s| s.id).collect::<Vec<_>>(),
            [leaf.id],
            "flat drops the deleted post but keeps the rest of the conversation",
        );
    }

    /// Stubbing nulls content and strips the engagement/child rows a hard
    /// delete would cascade, but keeps `status_mentions` (visibility math).
    #[sqlx::test]
    async fn stub_nulls_content_strips_children_keeps_mentions(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let post = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>secret</p>", "public", None),
        )
        .await
        .unwrap();
        // A reply makes `post` a middle-of-thread post, so deleting it stubs
        // (a reply-less post would hard-delete instead).
        create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>reply</p>", "public", Some(post.id)),
        )
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO favourites (id, account_id, status_id) VALUES ($1, $2, $3)",
            id::next(),
            bob,
            post.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO status_mentions (status_id, account_id) VALUES ($1, $2)",
            post.id,
            bob,
        )
        .execute(&pool)
        .await
        .unwrap();

        stub_local(&pool, post.id, alice)
            .await
            .unwrap()
            .expect("stubbed");

        let favs = sqlx::query_scalar!(
            r#"SELECT count(*) AS "c!" FROM favourites WHERE status_id = $1"#,
            post.id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(favs, 0, "favourites stripped");
        let mentions = sqlx::query_scalar!(
            r#"SELECT count(*) AS "c!" FROM status_mentions WHERE status_id = $1"#,
            post.id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(mentions, 1, "mentions kept for visibility math");
    }

    /// A stub is excluded from reply counts and the account's status count.
    #[sqlx::test]
    async fn stub_excluded_from_counts(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let r1 = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>r1</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>r2</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        assert_eq!(count_by_account(&pool, alice).await.unwrap(), 3);
        assert_eq!(
            engagement_for(&pool, &[root.id]).await.unwrap()[&root.id].replies,
            2
        );

        stub_local(&pool, r1.id, alice).await.unwrap();

        assert_eq!(
            engagement_for(&pool, &[root.id]).await.unwrap()[&root.id].replies,
            1,
            "stubbed reply not counted",
        );
        assert_eq!(
            count_by_account(&pool, alice).await.unwrap(),
            2,
            "stub excluded from the account status count",
        );
    }

    /// The GC reaps only *leaf* stubs; a stub that still connects a live reply
    /// survives until its child is gone, then is collected on a later run.
    #[sqlx::test]
    async fn prune_leaf_stubs_reaps_only_leaves(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let mid = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>mid</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let leaf = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>leaf</p>", "public", Some(mid.id)),
        )
        .await
        .unwrap();
        // `mid` has a live reply (`leaf`), so deleting it leaves a stub.
        stub_local(&pool, mid.id, alice).await.unwrap();
        assert!(is_deleted(&pool, mid.id).await.unwrap());

        let cutoff = OffsetDateTime::now_utc() + time::Duration::days(1);
        // `mid` still connects the live `leaf`, so it is not yet a leaf stub.
        assert_eq!(prune_leaf_stubs(&pool, cutoff, 100).await.unwrap(), 0);
        assert!(find_by_id(&pool, mid.id).await.unwrap().is_some());

        // Deleting the reply-less `leaf` hard-removes it, making `mid` a leaf.
        stub_local(&pool, leaf.id, alice).await.unwrap();
        assert!(find_by_id(&pool, leaf.id).await.unwrap().is_none());
        assert_eq!(prune_leaf_stubs(&pool, cutoff, 100).await.unwrap(), 1);
        assert!(find_by_id(&pool, mid.id).await.unwrap().is_none());
        // A non-stub is never touched.
        assert!(find_by_id(&pool, root.id).await.unwrap().is_some());
    }

    #[sqlx::test]
    async fn reblogs_are_idempotent_and_cascade(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let carol = local_account(&pool, "carol").await;
        let original = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>boost me</p>", "public", None),
        )
        .await
        .unwrap();

        let boost = create_local_reblog(&pool, carol, original.id)
            .await
            .unwrap();
        assert_eq!(boost.reblog_of_id, Some(original.id));
        let again = create_local_reblog(&pool, carol, original.id)
            .await
            .unwrap();
        assert_eq!(again.id, boost.id);

        // Deleting the original removes the boost too.
        delete_local(&pool, original.id, alice).await.unwrap();
        assert!(find_by_id(&pool, boost.id).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn only_media_looks_through_reblogs_only_when_asked(pool: PgPool) {
        let author = local_account(&pool, "author").await;
        let community = local_account(&pool, "community").await;
        let person = local_account(&pool, "person").await;

        // A post carrying a media attachment.
        let post = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>pic</p>", "public", None),
        )
        .await
        .unwrap();
        let media_id = crate::id::next();
        crate::media::create_local(
            &pool,
            crate::media::NewLocalMedia::new(author, media_id, "pic.png", "image/png"),
        )
        .await
        .unwrap();
        crate::media::attach(&pool, &[media_id], post.id, author)
            .await
            .unwrap();

        // Both a group and a person boost it.
        create_local_reblog(&pool, community, post.id)
            .await
            .unwrap();
        create_local_reblog(&pool, person, post.id).await.unwrap();

        let media = |through| AccountStatusesFilter {
            only_media: true,
            media_through_reblog: through,
            ..Default::default()
        };

        // A group's Media wall (through-reblog) surfaces the boosted media.
        let order = TimelineOrder::default();
        let group_media = by_account(&pool, community, Some(community), &media(true), order, 50)
            .await
            .unwrap();
        assert_eq!(group_media.len(), 1);
        assert_eq!(group_media[0].reblog_of_id, Some(post.id));

        // A person's Media tab (no through-reblog) excludes boosts — a boost
        // row has no attachments of its own.
        let person_media = by_account(&pool, person, Some(person), &media(false), order, 50)
            .await
            .unwrap();
        assert!(person_media.is_empty());

        // The author's own Media tab still shows the original media post.
        let author_media = by_account(&pool, author, Some(author), &media(false), order, 50)
            .await
            .unwrap();
        assert_eq!(author_media.len(), 1);
        assert_eq!(author_media[0].id, post.id);
    }

    #[sqlx::test]
    async fn public_timeline_filters_visibility_locality_and_boosts(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = remote_account(&pool).await;
        let public = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>public</p>", "public", None),
        )
        .await
        .unwrap();
        create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>unlisted</p>", "unlisted", None),
        )
        .await
        .unwrap();
        create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>private</p>", "private", None),
        )
        .await
        .unwrap();
        let local_only = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>local</p>", "local", None),
        )
        .await
        .unwrap();
        create_local_reblog(&pool, alice, public.id).await.unwrap();
        let remote = upsert_remote(
            &pool,
            remote_note(
                "https://remote.example/users/bob/statuses/2",
                bob,
                "<p>remote</p>",
            ),
        )
        .await
        .unwrap();

        let federated = public_timeline_both(&pool, false, None, false, None, 50).await;
        let mut ids: Vec<i64> = federated.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let mut expected = vec![public.id, remote.id];
        expected.sort_unstable();
        assert_eq!(ids, expected, "only public originals appear");

        let local = public_timeline_both(&pool, true, None, false, None, 50).await;
        assert_eq!(local.iter().map(|s| s.id).collect::<Vec<_>>(), [public.id]);

        let local_signed_in = public_timeline_both(&pool, true, Some(alice), false, None, 50).await;
        let ids: Vec<i64> = local_signed_in.iter().map(|s| s.id).collect();
        assert!(
            ids.contains(&public.id) && ids.contains(&local_only.id),
            "authenticated local timeline includes local-only posts"
        );
    }

    /// With `public_timeline_replies` off (the default) the shared
    /// timelines match Mastodon's `PublicFeed` — originals and self-threads.
    /// Runs both localities, and each locality under both orderings via
    /// `public_timeline_both`, since the predicate is pasted into four queries
    /// that must not drift.
    #[sqlx::test]
    async fn public_timelines_hide_replies_to_other_people(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let carol = remote_account(&pool).await;

        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        // Alice continues her own thread: a post, not half a conversation.
        let self_thread = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>and another thing</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        // Bob answers Alice: the row the whole slice exists to hide.
        let stranger_reply = create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>disagree</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        // A remote reply whose parent never arrived: `in_reply_to_id` NULL, so
        // its author is unknowable and it cannot be shown to be a self-thread.
        // Hidden — testing `in_reply_to_id` alone would leak it as a top-level
        // post.
        let mut orphan = remote_note(
            "https://remote.example/users/carol/statuses/1",
            carol,
            "<p>orphan</p>",
        );
        orphan.in_reply_to_uri = Some("https://remote.example/never/fetched");
        let orphan = upsert_remote(&pool, orphan).await.unwrap();

        for local_only in [false, true] {
            let feed = public_timeline_both(&pool, local_only, None, false, None, 50).await;
            let ids: Vec<i64> = feed.iter().map(|s| s.id).collect();
            assert!(
                ids.contains(&root.id) && ids.contains(&self_thread.id),
                "originals and self-threads stay (local_only={local_only})"
            );
            assert!(
                !ids.contains(&stranger_reply.id),
                "a reply to someone else is hidden (local_only={local_only})"
            );
            assert!(
                !ids.contains(&orphan.id),
                "a reply whose parent never arrived is hidden (local_only={local_only})"
            );

            // The knob restores the pre-0031 firehose exactly.
            let all = public_timeline_both(&pool, local_only, None, true, None, 50).await;
            let ids: Vec<i64> = all.iter().map(|s| s.id).collect();
            assert!(
                ids.contains(&stranger_reply.id),
                "the knob brings replies back (local_only={local_only})"
            );
            assert_eq!(
                ids.contains(&orphan.id),
                !local_only,
                "…including orphans, on the feed whose locality carries them"
            );
        }
    }

    /// The reply-column caching installed is what makes the reply predicate a
    /// column test: a reply adopted after its parent arrives must be judged
    /// like any other, which is the difference between the timeline losing a
    /// self-thread forever and merely being late to it.
    #[sqlx::test]
    async fn public_timeline_admits_a_self_thread_once_its_parent_arrives(pool: PgPool) {
        let carol = remote_account(&pool).await;
        let parent_uri = "https://remote.example/users/carol/statuses/1";

        let mut orphan = remote_note(
            "https://remote.example/users/carol/statuses/2",
            carol,
            "<p>…continued</p>",
        );
        orphan.in_reply_to_uri = Some(parent_uri);
        let orphan = upsert_remote(&pool, orphan).await.unwrap();

        let feed = public_timeline_both(&pool, false, None, false, None, 50).await;
        assert!(
            !feed.iter().any(|s| s.id == orphan.id),
            "unjudgeable while the parent is missing"
        );

        let parent = arrives(&pool, remote_note(parent_uri, carol, "<p>a thought</p>")).await;

        let feed = public_timeline_both(&pool, false, None, false, None, 50).await;
        let ids: Vec<i64> = feed.iter().map(|s| s.id).collect();
        assert!(
            ids.contains(&parent.id) && ids.contains(&orphan.id),
            "adoption makes it a self-thread, which the public timeline keeps"
        );
    }

    /// The streaming hub holds a [`Status`], which carries neither
    /// `in_reply_to_uri` nor `in_reply_to_account_id` — it asks this instead,
    /// and its answer must match the timeline predicate row for row.
    #[sqlx::test]
    async fn is_non_self_reply_matches_the_timeline_predicate(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let carol = remote_account(&pool).await;

        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let self_thread = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>more</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let stranger_reply = create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>no</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let mut orphan = remote_note(
            "https://remote.example/users/carol/statuses/1",
            carol,
            "<p>orphan</p>",
        );
        orphan.in_reply_to_uri = Some("https://remote.example/never/fetched");
        let orphan = upsert_remote(&pool, orphan).await.unwrap();

        let timeline: Vec<i64> = public_timeline_both(&pool, false, None, false, None, 50)
            .await
            .iter()
            .map(|s| s.id)
            .collect();
        for id in [root.id, self_thread.id, stranger_reply.id, orphan.id] {
            assert_eq!(
                is_non_self_reply(&pool, id).await.unwrap(),
                !timeline.contains(&id),
                "status {id} disagrees with the timeline"
            );
        }
        // A row that is already gone is not a reply to anybody.
        assert!(!is_non_self_reply(&pool, root.id + 9999).await.unwrap());
    }

    #[sqlx::test]
    async fn public_timelines_hide_blocked_and_muted_authors(pool: PgPool) {
        let viewer = local_account(&pool, "viewer").await;
        let blocked = local_account(&pool, "blocked").await;
        let blocker = local_account(&pool, "blocker").await;
        let muted = local_account(&pool, "muted").await;
        let fine = local_account(&pool, "fine").await;

        for (author, text) in [
            (blocked, "<p>blocked</p>"),
            (blocker, "<p>blocker</p>"),
            (muted, "<p>muted</p>"),
            (fine, "<p>fine</p>"),
            (viewer, "<p>self</p>"),
        ] {
            create_local(&pool, NewLocalStatus::new(author, text, "public", None))
                .await
                .unwrap();
        }
        crate::block::create(&pool, viewer, blocked, None)
            .await
            .unwrap();
        crate::block::create(&pool, blocker, viewer, None)
            .await
            .unwrap();
        crate::mute::upsert(&pool, viewer, muted, false, None)
            .await
            .unwrap();

        // Blocks in either direction and active mutes hide the author from
        // the viewer's public timelines; the viewer's own posts always show.
        for local_only in [false, true] {
            let feed = public_timeline_both(&pool, local_only, Some(viewer), false, None, 50).await;
            let mut authors: Vec<i64> = feed.iter().map(|s| s.account_id).collect();
            authors.sort_unstable();
            let mut expected = vec![fine, viewer];
            expected.sort_unstable();
            assert_eq!(authors, expected, "local_only={local_only}");

            // Anonymous visitors are unaffected.
            let anon = public_timeline_both(&pool, local_only, None, false, None, 50).await;
            assert_eq!(anon.len(), 5, "local_only={local_only}");
        }
    }

    #[sqlx::test]
    async fn public_timeline_hides_suspended_and_silenced(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let spammer = local_account(&pool, "spammer").await;
        let quiet = local_account(&pool, "quiet").await;
        let follower = local_account(&pool, "follower").await;

        let normal = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>hi</p>", "public", None),
        )
        .await
        .unwrap();
        create_local(
            &pool,
            NewLocalStatus::new(spammer, "<p>spam</p>", "public", None),
        )
        .await
        .unwrap();
        let quiet_post = create_local(
            &pool,
            NewLocalStatus::new(quiet, "<p>quiet</p>", "public", None),
        )
        .await
        .unwrap();

        crate::follow::create(&pool, follower, spammer, None)
            .await
            .unwrap();

        crate::account::suspend(&pool, spammer, "local")
            .await
            .unwrap();
        crate::account::silence(&pool, quiet).await.unwrap();

        // Anonymous: both the suspended and the silenced author are hidden.
        let anon = public_timeline_both(&pool, false, None, false, None, 50).await;
        assert_eq!(
            anon.iter().map(|s| s.id).collect::<Vec<_>>(),
            [normal.id],
            "suspended and silenced authors are hidden from the public timeline"
        );

        // Even a follower of the silenced account no longer sees them on the
        // shared public timeline — the post reaches followers through the home
        // timeline instead (Mastodon's `without_silenced`).
        crate::follow::create(&pool, follower, quiet, None)
            .await
            .unwrap();
        let viewed = public_timeline_both(&pool, false, Some(follower), false, None, 50).await;
        assert_eq!(
            viewed.iter().map(|s| s.id).collect::<Vec<_>>(),
            [normal.id],
            "silenced authors are hidden from the public timeline even for followers"
        );
        let _ = quiet_post;

        // The silenced author still reaches their follower's home timeline.
        let home = home_timeline(&pool, follower, TimelineOrder::Received, None, 50)
            .await
            .unwrap();
        assert!(
            home.iter().any(|s| s.account_id == quiet),
            "the follower still receives the silenced author at home"
        );
        assert!(
            home.iter().all(|status| status.account_id != spammer),
            "suspension removes existing posts even from a follower's home timeline"
        );
    }

    #[sqlx::test]
    async fn public_and_tag_timelines_hide_domain_silenced_authors(pool: PgPool) {
        let local = local_account(&pool, "local").await;
        let remote = remote_account(&pool).await;

        let tag_id = crate::tag::ensure(&pool, "hi").await.unwrap();
        let local_post = create_local(
            &pool,
            NewLocalStatus::new(local, "<p>#hi local</p>", "public", None),
        )
        .await
        .unwrap();
        crate::tag::attach(&pool, local_post.id, tag_id)
            .await
            .unwrap();
        let note = remote_note(
            "https://remote.example/users/bob/statuses/1",
            remote,
            "#hi remote",
        );
        let remote_post = upsert_remote(&pool, note).await.unwrap();
        crate::tag::attach(&pool, remote_post.id, tag_id)
            .await
            .unwrap();

        // Before the block, both appear on the federated and hashtag feeds.
        let before = public_timeline_both(&pool, false, None, false, None, 50).await;
        assert_eq!(
            before.len(),
            2,
            "both authors visible before the domain block"
        );

        // Silence the remote author's whole domain.
        crate::instance_policy::create_domain_block(
            &pool,
            crate::instance_policy::NewDomainBlock {
                domain: "remote.example",
                severity: "silence",
                reject_media: false,
                reject_reports: false,
                private_comment: None,
                public_comment: None,
                obfuscate: false,
            },
        )
        .await
        .unwrap();

        let after = public_timeline_both(&pool, false, None, false, None, 50).await;
        assert_eq!(
            after.iter().map(|s| s.id).collect::<Vec<_>>(),
            [local_post.id],
            "the domain-silenced author is hidden from the federated timeline"
        );
        let tagged = crate::tag::timeline(&pool, "hi", None, TimelineOrder::Received, None, 50)
            .await
            .unwrap();
        assert_eq!(
            tagged.iter().map(|s| s.id).collect::<Vec<_>>(),
            [local_post.id],
            "the domain-silenced author is hidden from the hashtag timeline"
        );
        let _ = remote_post;
    }

    #[sqlx::test]
    async fn by_account_hides_silenced_author_from_anonymous_only(pool: PgPool) {
        let quiet = local_account(&pool, "quiet").await;
        let viewer = local_account(&pool, "viewer").await;
        let post = create_local(
            &pool,
            NewLocalStatus::new(quiet, "<p>hello</p>", "public", None),
        )
        .await
        .unwrap();
        crate::account::silence(&pool, quiet).await.unwrap();

        let filter = AccountStatusesFilter {
            exclude_replies: false,
            exclude_reblogs: false,
            only_media: false,
            media_through_reblog: false,
            tagged: None,
            max_id: None,
            since_id: None,
        };
        let order = TimelineOrder::default();
        let anon = by_account(&pool, quiet, None, &filter, order, 50)
            .await
            .unwrap();
        assert!(
            anon.is_empty(),
            "anonymous visitors see none of a silenced author's posts"
        );

        let seen = by_account(&pool, quiet, Some(viewer), &filter, order, 50)
            .await
            .unwrap();
        assert_eq!(
            seen.iter().map(|s| s.id).collect::<Vec<_>>(),
            [post.id],
            "a logged-in viewer still sees the silenced author's posts"
        );
    }

    #[sqlx::test]
    async fn by_account_hides_suspended_author_from_everyone(pool: PgPool) {
        let author = local_account(&pool, "author").await;
        let viewer = local_account(&pool, "viewer").await;
        create_local(
            &pool,
            NewLocalStatus::new(author, "<p>before suspension</p>", "public", None),
        )
        .await
        .unwrap();
        crate::account::suspend(&pool, author, "local")
            .await
            .unwrap();
        let filter = AccountStatusesFilter::default();
        for viewer in [None, Some(viewer), Some(author)] {
            assert!(
                by_account(&pool, author, viewer, &filter, TimelineOrder::Published, 50,)
                    .await
                    .unwrap()
                    .is_empty(),
                "a suspended profile exposes no statuses to {viewer:?}"
            );
        }
    }

    /// The profile listing honors `timeline_order`, like the home/list/tag
    /// timelines: a backfilled remote post (ingested late but published long
    /// ago) leads under ingest order yet sinks to its publish date under the
    /// default publish order. Also guards the two orderings' duplicated WHERE
    /// bodies against drift — both must select the same row set.
    #[sqlx::test]
    async fn by_account_honors_timeline_order(pool: PgPool) {
        let bob = remote_account(&pool).await;
        // Published recently; ingested first, so it takes the lower id.
        let fresh = upsert_remote(
            &pool,
            NewRemoteStatus {
                created_at: datetime!(2026-06-01 12:00 UTC),
                ..remote_note(
                    "https://remote.example/users/bob/statuses/fresh",
                    bob,
                    "<p>now</p>",
                )
            },
        )
        .await
        .unwrap()
        .id;
        // Ingested after `fresh` (higher id) but published long before it.
        let old = upsert_remote(
            &pool,
            NewRemoteStatus {
                created_at: datetime!(2026-01-01 00:00 UTC),
                ..remote_note(
                    "https://remote.example/users/bob/statuses/old",
                    bob,
                    "<p>old</p>",
                )
            },
        )
        .await
        .unwrap()
        .id;

        let filter = AccountStatusesFilter::default();

        // Publish order (the default): newest publish date first.
        let published = by_account(&pool, bob, None, &filter, TimelineOrder::Published, 20)
            .await
            .unwrap();
        assert_eq!(ids_of(&published), [fresh, old]);

        // Ingest order: newest ingested (highest id) first.
        let received = by_account(&pool, bob, None, &filter, TimelineOrder::Received, 20)
            .await
            .unwrap();
        assert_eq!(ids_of(&received), [old, fresh]);
    }

    fn ids_of(statuses: &[Status]) -> Vec<i64> {
        statuses.iter().map(|s| s.id).collect()
    }

    #[sqlx::test]
    async fn published_order_sorts_by_post_date_received_by_ingest(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = remote_account(&pool).await;
        crate::follow::create(&pool, alice, bob, None)
            .await
            .unwrap();

        let first = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>first</p>", "public", None),
        )
        .await
        .unwrap();
        // An old post arrives late — a backfilled or thread-fetched status.
        let mut note = remote_note("https://remote.example/users/bob/statuses/old", bob, "old");
        note.created_at = datetime!(2026-01-01 00:00 UTC);
        let old = upsert_remote(&pool, note).await.unwrap();
        let last = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>last</p>", "public", None),
        )
        .await
        .unwrap();

        let published = home_timeline(&pool, alice, TimelineOrder::Published, None, 20)
            .await
            .unwrap();
        assert_eq!(
            ids_of(&published),
            [last.id, first.id, old.id],
            "post-date order: the late-arriving old post sinks to its date"
        );

        let received = home_timeline(&pool, alice, TimelineOrder::Received, None, 20)
            .await
            .unwrap();
        assert_eq!(
            ids_of(&received),
            [last.id, old.id, first.id],
            "ingest order: the old post sits where it arrived"
        );

        // The public timeline orders the same way.
        let public = public_timeline(
            &pool,
            false,
            None,
            false,
            TimelineOrder::Published,
            None,
            20,
        )
        .await
        .unwrap();
        assert_eq!(ids_of(&public), [last.id, first.id, old.id]);
    }

    #[sqlx::test]
    async fn published_order_clamps_future_dated_posts(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = remote_account(&pool).await;

        let first = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>first</p>", "public", None),
        )
        .await
        .unwrap();
        let mut note = remote_note("https://remote.example/users/bob/statuses/f", bob, "future");
        note.created_at = datetime!(2030-01-01 00:00 UTC);
        let future = upsert_remote(&pool, note).await.unwrap();
        let last = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>last</p>", "public", None),
        )
        .await
        .unwrap();

        let published = public_timeline(
            &pool,
            false,
            None,
            false,
            TimelineOrder::Published,
            None,
            20,
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&published),
            [last.id, future.id, first.id],
            "a future-dated post is clamped to its arrival position, not pinned to the top"
        );
        // The clamp only affects ordering; the claimed publish date still shows.
        assert_eq!(future.created_at, datetime!(2030-01-01 00:00 UTC));
    }

    #[sqlx::test]
    async fn published_order_keyset_paginates_and_survives_a_deleted_anchor(pool: PgPool) {
        let bob = remote_account(&pool).await;
        // Ingest order differs from publish order: day 3, 1, 5, 2, 4.
        let mut by_day = HashMap::new();
        for day in [3, 1, 5, 2, 4] {
            let uri = format!("https://remote.example/users/bob/statuses/{day}");
            let mut note = remote_note(&uri, bob, "hi");
            note.created_at = datetime!(2026-06-01 00:00 UTC) + time::Duration::days(day);
            by_day.insert(day, upsert_remote(&pool, note).await.unwrap().id);
        }

        let page = |max_id| {
            public_timeline(
                &pool,
                false,
                None,
                false,
                TimelineOrder::Published,
                max_id,
                2,
            )
        };
        let first = page(None).await.unwrap();
        assert_eq!(ids_of(&first), [by_day[&5], by_day[&4]]);
        let second = page(Some(by_day[&4])).await.unwrap();
        assert_eq!(ids_of(&second), [by_day[&3], by_day[&2]]);
        let third = page(Some(by_day[&2])).await.unwrap();
        assert_eq!(ids_of(&third), [by_day[&1]]);

        // A deleted anchor must not dead-end pagination: the cursor falls back
        // to the ingest time embedded in the snowflake id, which may re-serve
        // rows already seen but keeps the page ordered and non-empty.
        assert!(
            delete_by_uri(&pool, "https://remote.example/users/bob/statuses/4", bob)
                .await
                .unwrap()
        );
        let after_deleted = public_timeline(
            &pool,
            false,
            None,
            false,
            TimelineOrder::Published,
            Some(by_day[&4]),
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&after_deleted),
            [by_day[&5], by_day[&3], by_day[&2], by_day[&1]],
            "still date-ordered, nothing lost"
        );
    }

    #[sqlx::test]
    async fn remote_reblog_stores_announce_published(pool: PgPool) {
        let bob = remote_account(&pool).await;
        let target = upsert_remote(
            &pool,
            remote_note("https://remote.example/users/bob/statuses/1", bob, "hi"),
        )
        .await
        .unwrap();

        let target2 = upsert_remote(
            &pool,
            remote_note("https://remote.example/users/bob/statuses/2", bob, "yo"),
        )
        .await
        .unwrap();

        let when = datetime!(2026-03-01 12:00 UTC);
        let boost = upsert_remote_reblog(
            &pool,
            "https://remote.example/users/bob/statuses/1/activity",
            bob,
            target.id,
            Some(when),
        )
        .await
        .unwrap()
        .expect("first boost of a target inserts");
        assert_eq!(boost.created_at, when, "the Announce's published time");

        // Without a published time the boost keeps its ingest time (a distinct
        // target — a second boost of the *same* target is deduped, below).
        let plain = upsert_remote_reblog(
            &pool,
            "https://remote.example/users/bob/statuses/2/activity",
            bob,
            target2.id,
            None,
        )
        .await
        .unwrap()
        .expect("first boost of a distinct target inserts");
        assert!(plain.created_at > when);
    }

    #[sqlx::test]
    async fn remote_reblog_dedups_per_account_and_target(pool: PgPool) {
        let bob = remote_account(&pool).await;
        let target = upsert_remote(
            &pool,
            remote_note("https://remote.example/users/bob/statuses/1", bob, "hi"),
        )
        .await
        .unwrap();

        let first = upsert_remote_reblog(
            &pool,
            "https://remote.example/community/announce/1",
            bob,
            target.id,
            None,
        )
        .await
        .unwrap();
        assert!(first.is_some(), "first boost inserts");

        // A second Announce of the same post by the same actor — a redelivery,
        // or a community's Mastodon-compat double-send under a fresh Announce
        // id — must not add a second boost row.
        let dup = upsert_remote_reblog(
            &pool,
            "https://remote.example/community/announce/2-compat",
            bob,
            target.id,
            None,
        )
        .await
        .unwrap();
        assert!(dup.is_none(), "the duplicate boost is deduped away");

        let count = sqlx::query_scalar!(
            "SELECT count(*) FROM statuses WHERE account_id = $1 AND reblog_of_id = $2 -- STUBKEEP: a boost wrapper never stubs",
            bob,
            target.id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, Some(1), "exactly one boost row survives");
    }

    #[sqlx::test]
    async fn cold_reblog_promotes_and_claims_live_effects_once(pool: PgPool) {
        let bob = remote_account(&pool).await;
        let target = upsert_remote(
            &pool,
            remote_note(
                "https://remote.example/users/bob/statuses/target",
                bob,
                "hi",
            ),
        )
        .await
        .unwrap();
        let cold = upsert_remote_reblog_history(
            &pool,
            "https://remote.example/users/bob/announces/history",
            bob,
            target.id,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let (promoted, effects) = upsert_remote_reblog_delivery(
            &pool,
            "https://remote.example/users/bob/announces/live",
            bob,
            target.id,
            None,
        )
        .await
        .unwrap();
        assert_eq!(promoted.id, cold.id);
        assert!(effects);
        let (replay, replay_effects) = upsert_remote_reblog_delivery(
            &pool,
            "https://remote.example/users/bob/announces/replay",
            bob,
            target.id,
            None,
        )
        .await
        .unwrap();
        assert_eq!(replay.id, cold.id);
        assert!(!replay_effects);
        assert_eq!(
            ingest_provenance(&pool, cold.id).await.unwrap().as_deref(),
            Some("delivery")
        );
    }

    #[sqlx::test]
    async fn engagement_and_viewer_flags(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let carol = local_account(&pool, "carol").await;
        let original = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>hi</p>", "public", None),
        )
        .await
        .unwrap();
        create_local(
            &pool,
            NewLocalStatus::new(carol, "<p>re</p>", "public", Some(original.id)),
        )
        .await
        .unwrap();
        create_local_reblog(&pool, carol, original.id)
            .await
            .unwrap();
        crate::favourite::create(&pool, carol, original.id, None)
            .await
            .unwrap();

        let counts = engagement_for(&pool, &[original.id]).await.unwrap();
        let engagement = counts[&original.id];
        assert_eq!(
            (
                engagement.replies,
                engagement.reblogs,
                engagement.favourites
            ),
            (1, 1, 1)
        );

        assert_eq!(
            reblogged_of(&pool, carol, &[original.id]).await.unwrap(),
            [original.id]
        );
        assert!(
            reblogged_of(&pool, alice, &[original.id])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    async fn batch_counts_match_single_lookups(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        // carol posts nothing, so she's absent from both batch maps rather
        // than present with a zero/None.
        let carol = local_account(&pool, "carol").await;
        create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>one</p>", "public", None),
        )
        .await
        .unwrap();
        create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>two</p>", "public", None),
        )
        .await
        .unwrap();
        create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>hello</p>", "public", None),
        )
        .await
        .unwrap();

        let ids = [alice, bob, carol];
        let counts = count_by_account_batch(&pool, &ids).await.unwrap();
        let last_at = last_created_at_batch(&pool, &ids).await.unwrap();
        for id in ids {
            assert_eq!(
                counts.get(&id).copied().unwrap_or(0),
                count_by_account(&pool, id).await.unwrap(),
                "count mismatch for {id}"
            );
            assert_eq!(
                last_at.get(&id).copied(),
                last_created_at(&pool, id).await.unwrap(),
                "last_created_at mismatch for {id}"
            );
        }
        // Carol exists but has no statuses: present with an explicit 0 (the
        // query starts from `accounts` to see synced remote totals).
        assert_eq!(counts.get(&carol), Some(&0));
        assert!(!last_at.contains_key(&carol));

        assert!(count_by_account_batch(&pool, &[]).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn thread_root_walks_to_the_top(pool: PgPool) {
        let author = local_account(&pool, "author").await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let child = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>child</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let grandchild = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>grandchild</p>", "public", Some(child.id)),
        )
        .await
        .unwrap();

        // Every node in the chain resolves to the same root.
        assert_eq!(thread_root(&pool, root.id).await.unwrap(), root.id);
        assert_eq!(thread_root(&pool, child.id).await.unwrap(), root.id);
        assert_eq!(thread_root(&pool, grandchild.id).await.unwrap(), root.id);
        // A missing id falls back to itself rather than erroring.
        assert_eq!(thread_root(&pool, 999_999).await.unwrap(), 999_999);
    }

    #[sqlx::test]
    async fn unresolved_reply_parents_reports_only_unfetched_uri_parents(pool: PgPool) {
        let author = local_account(&pool, "author").await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        // A reply whose parent WAS fetched (carries a local in_reply_to_id).
        let resolved = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>resolved</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        // A reply whose parent was NEVER fetched: only the AP URI is known.
        let orphan = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>orphan</p>", "public", None),
        )
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE statuses SET in_reply_to_uri = $2 WHERE id = $1",
            orphan.id,
            "https://remote.example/comment/42",
        )
        .execute(&pool)
        .await
        .unwrap();

        let map: HashMap<i64, String> =
            unresolved_reply_parents(&pool, &[root.id, resolved.id, orphan.id])
                .await
                .unwrap()
                .into_iter()
                .collect();

        // Only the orphan (null in_reply_to_id, set in_reply_to_uri) is reported.
        assert_eq!(
            map.get(&orphan.id).map(String::as_str),
            Some("https://remote.example/comment/42")
        );
        assert!(!map.contains_key(&root.id)); // not a reply
        assert!(!map.contains_key(&resolved.id)); // parent resolved locally
        // Empty input short-circuits without a query.
        assert!(
            unresolved_reply_parents(&pool, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The denormalized parent author (`FEEDS_DESIGN.md`). Not part of
    /// [`Status`] — it exists for WHERE clauses, not for rendering — so the
    /// tests read it directly.
    async fn reply_author(pool: &PgPool, status_id: i64) -> Option<i64> {
        sqlx::query_scalar!(
            "SELECT in_reply_to_account_id FROM statuses WHERE id = $1 -- STUBKEEP: a stub edge still threads",
            status_id
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn reply_author_tracks_the_linked_parent(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = remote_account(&pool).await;
        let root = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        // A plain post is not a reply: the column stays NULL and the trigger
        // never runs.
        assert_eq!(reply_author(&pool, root.id).await, None);

        // Local reply to someone else's post, and a self-continuation.
        let self_reply = create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>and another thing</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        assert_eq!(reply_author(&pool, self_reply.id).await, Some(alice));

        // Remote reply whose parent resolved: read off the parent row, not off
        // the reply's own author.
        let remote_reply = upsert_remote(
            &pool,
            NewRemoteStatus {
                in_reply_to_id: Some(root.id),
                in_reply_to_uri: Some("https://plamenu.test/@alice/1"),
                ..remote_note(
                    "https://remote.example/users/bob/statuses/9",
                    bob,
                    "<p>replying</p>",
                )
            },
        )
        .await
        .unwrap();
        assert_eq!(reply_author(&pool, remote_reply.id).await, Some(alice));

        // Losing the parent loses the answer: the self-FK's ON DELETE SET NULL
        // nulls `in_reply_to_id`, and the cache must not keep pointing at an
        // author whose post is gone.
        delete_local(&pool, root.id, alice).await.unwrap();
        assert_eq!(reply_author(&pool, self_reply.id).await, None);
        assert_eq!(reply_author(&pool, remote_reply.id).await, None);
    }

    /// What the inbound ingest path does with an arriving remote note: store it,
    /// then join the replies that had been waiting for it (`ingest.rs` runs the
    /// second half once the note has its conversation).
    async fn arrives(pool: &PgPool, new: NewRemoteStatus<'_>) -> Status {
        let stored = upsert_remote(pool, new).await.unwrap();
        if let Some(uri) = stored.uri.clone() {
            let mut conn = pool.acquire().await.unwrap();
            adopt_orphan_replies(&mut conn, stored.id, &uri)
                .await
                .unwrap();
        }
        stored
    }

    #[sqlx::test]
    async fn orphaned_reply_is_adopted_when_its_parent_arrives(pool: PgPool) {
        let bob = remote_account(&pool).await;
        let parent_uri = "https://remote.example/users/bob/statuses/100";
        // A reply ingested before its parent: the IRI is kept, the parent row
        // does not exist yet, so there is nothing to link to.
        let orphan = arrives(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some(parent_uri),
                ..remote_note(
                    "https://remote.example/users/bob/statuses/101",
                    bob,
                    "<p>orphaned reply</p>",
                )
            },
        )
        .await;
        // Its own reply, which found *its* parent and so hangs off the orphan.
        let grandchild = arrives(
            &pool,
            NewRemoteStatus {
                in_reply_to_id: Some(orphan.id),
                in_reply_to_uri: Some("https://remote.example/users/bob/statuses/101"),
                ..remote_note(
                    "https://remote.example/users/bob/statuses/102",
                    bob,
                    "<p>under the orphan</p>",
                )
            },
        )
        .await;
        // A reply into a thread whose root never shows up at all.
        let still_orphaned = arrives(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some("https://elsewhere.example/notes/1"),
                ..remote_note(
                    "https://remote.example/users/bob/statuses/103",
                    bob,
                    "<p>other thread</p>",
                )
            },
        )
        .await;
        assert_eq!(reply_author(&pool, orphan.id).await, None);

        // The parent finally arrives — a later delivery, a boost, a walk down
        // someone else's `replies` collection.
        let parent = arrives(&pool, remote_note(parent_uri, bob, "<p>the root</p>")).await;

        // The thread is repaired: the reply now hangs off the parent, and its own
        // subtree comes with it (the descendants already pointed at the reply).
        let subtree: Vec<i64> = descendants(&pool, parent.id)
            .await
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(subtree, [orphan.id, grandchild.id]);
        // The author cache follows the edge, via the trigger.
        assert_eq!(reply_author(&pool, orphan.id).await, Some(bob));
        // And the "parent was never fetched" notice is gone.
        assert!(
            unresolved_reply_parents(&pool, &[orphan.id])
                .await
                .unwrap()
                .is_empty()
        );
        // An unrelated orphan is untouched, and the parent itself is no reply.
        assert_eq!(reply_author(&pool, still_orphaned.id).await, None);
        assert_eq!(reply_author(&pool, parent.id).await, None);

        // Re-delivery of the parent changes nothing.
        arrives(&pool, remote_note(parent_uri, bob, "<p>changed</p>")).await;
        assert_eq!(reply_author(&pool, orphan.id).await, Some(bob));
    }

    #[sqlx::test]
    async fn orphan_adoption_refuses_to_close_a_cycle(pool: PgPool) {
        let bob = remote_account(&pool).await;
        let first_uri = "https://remote.example/users/bob/statuses/200";
        let second_uri = "https://remote.example/users/bob/statuses/201";
        // Two posts that each claim to answer the other, delivered in an order
        // that leaves the first orphaned.
        let first = arrives(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some(second_uri),
                ..remote_note(first_uri, bob, "<p>first</p>")
            },
        )
        .await;
        let second = arrives(
            &pool,
            NewRemoteStatus {
                in_reply_to_id: Some(first.id),
                in_reply_to_uri: Some(first_uri),
                ..remote_note(second_uri, bob, "<p>second</p>")
            },
        )
        .await;

        // Adopting `first` under `second` would close a loop, so it is refused:
        // the orphan keeps its unresolved parent rather than corrupting the tree.
        assert_eq!(second.in_reply_to_id, Some(first.id));
        let refreshed = find_by_id(&pool, first.id).await.unwrap().unwrap();
        assert_eq!(refreshed.in_reply_to_id, None);
        assert_eq!(reply_author(&pool, first.id).await, None);
        assert_eq!(
            unresolved_reply_parents(&pool, &[first.id])
                .await
                .unwrap()
                .len(),
            1
        );

        // A longer ring is refused just the same — the guard walks the candidate
        // parent's whole ancestry, so it does not matter how many posts the loop
        // takes to come back around. Here A is adopted under B legitimately, and
        // the closing hop (B under C, where C already sits below A) is the one
        // that must be refused.
        let a_uri = "https://remote.example/users/bob/statuses/300";
        let b_uri = "https://remote.example/users/bob/statuses/301";
        let c_uri = "https://remote.example/users/bob/statuses/302";
        let a = arrives(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some(b_uri),
                ..remote_note(a_uri, bob, "<p>a</p>")
            },
        )
        .await;
        let b = arrives(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some(c_uri),
                ..remote_note(b_uri, bob, "<p>b</p>")
            },
        )
        .await;
        // B's arrival adopts A: nothing sits above B yet, so no loop.
        assert_eq!(
            find_by_id(&pool, a.id)
                .await
                .unwrap()
                .unwrap()
                .in_reply_to_id,
            Some(b.id)
        );
        // C answers A, so the chain reads C → A → B. Adopting B under C would
        // close the ring.
        let c = arrives(
            &pool,
            NewRemoteStatus {
                in_reply_to_id: Some(a.id),
                in_reply_to_uri: Some(a_uri),
                ..remote_note(c_uri, bob, "<p>c</p>")
            },
        )
        .await;
        assert_eq!(c.in_reply_to_id, Some(a.id));
        assert_eq!(
            find_by_id(&pool, b.id)
                .await
                .unwrap()
                .unwrap()
                .in_reply_to_id,
            None,
            "the closing hop of a three-post ring is refused"
        );
    }

    // ---- Per-follow `with_replies` -------------------------------------

    /// Runs the home timeline under both orderings, asserts they select the
    /// same rows (the WHERE bodies are duplicated per ordering and must not
    /// drift) and returns the ids.
    async fn home_ids(pool: &PgPool, viewer: i64) -> Vec<i64> {
        let published = home_timeline(pool, viewer, TimelineOrder::Published, None, 50)
            .await
            .unwrap();
        let received = home_timeline(pool, viewer, TimelineOrder::Received, None, 50)
            .await
            .unwrap();
        let mut a = ids_of(&published);
        let mut b = ids_of(&received);
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "both home orderings must select the same rows");
        a
    }

    async fn set_with_replies(pool: &PgPool, viewer: i64, target: i64, on: bool) {
        crate::follow::update_settings(pool, viewer, target, None, Some(on), None, None)
            .await
            .unwrap();
    }

    /// `PeerTube` publishes a Video once from its account Person and announces
    /// the same object from its channel Group. A viewer following both should
    /// get one home item; a viewer following only the channel must still get
    /// the announce. This is deliberately Video-specific so ordinary community
    /// attribution remains a separate feed fact.
    #[sqlx::test]
    async fn home_dedupes_peertube_style_channel_announces(pool: PgPool) {
        let viewer = local_account(&pool, "viewer").await;
        let author = local_account(&pool, "video-author").await;
        let channel = local_account(&pool, "video-channel").await;
        sqlx::query!(
            "UPDATE accounts SET actor_type = 'Group' WHERE id = $1",
            channel
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::follow::create(&pool, viewer, author, None)
            .await
            .unwrap();
        crate::follow::create(&pool, viewer, channel, None)
            .await
            .unwrap();

        let video = create_local(
            &pool,
            NewLocalStatus {
                object_type: Some("Video"),
                ..NewLocalStatus::new(author, "<p>one video</p>", "public", None)
            },
        )
        .await
        .unwrap();
        let channel_video = create_local_reblog(&pool, channel, video.id).await.unwrap();

        let ordinary = create_local(
            &pool,
            NewLocalStatus::new(author, "<p>ordinary post</p>", "public", None),
        )
        .await
        .unwrap();
        let channel_ordinary = create_local_reblog(&pool, channel, ordinary.id)
            .await
            .unwrap();

        let both = home_ids(&pool, viewer).await;
        assert!(
            both.contains(&video.id),
            "the followed author's Video stays"
        );
        assert!(
            !both.contains(&channel_video.id),
            "the redundant channel wrapper is suppressed"
        );
        assert!(both.contains(&ordinary.id));
        assert!(
            both.contains(&channel_ordinary.id),
            "ordinary community announces retain their attribution"
        );

        crate::follow::delete(&pool, viewer, author).await.unwrap();
        let channel_only = home_ids(&pool, viewer).await;
        assert!(!channel_only.contains(&video.id));
        assert!(
            channel_only.contains(&channel_video.id),
            "following only the channel still delivers its Video"
        );
    }

    /// With the flag on, home is unchanged; with it off, the three exemptions
    /// every upstream server agrees on still let a reply through.
    #[sqlx::test]
    async fn home_with_replies_off_keeps_the_three_exemptions(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let carol = local_account(&pool, "carol").await;
        let stranger = local_account(&pool, "stranger").await;
        crate::follow::create(&pool, alice, bob, None)
            .await
            .unwrap();
        crate::follow::create(&pool, alice, carol, None)
            .await
            .unwrap();

        let post = async |author: i64, text: &str, parent: Option<i64>| {
            create_local(&pool, NewLocalStatus::new(author, text, "public", parent))
                .await
                .unwrap()
                .id
        };
        let bob_post = post(bob, "<p>hi</p>", None).await;
        let self_reply = post(bob, "<p>more</p>", Some(bob_post)).await;
        let alice_post = post(alice, "<p>mine</p>", None).await;
        let to_alice = post(bob, "<p>@alice</p>", Some(alice_post)).await;
        let carol_post = post(carol, "<p>carol</p>", None).await;
        let to_carol = post(bob, "<p>@carol</p>", Some(carol_post)).await;
        let stranger_post = post(stranger, "<p>who</p>", None).await;
        let to_stranger = post(bob, "<p>@stranger</p>", Some(stranger_post)).await;
        // The viewer's own reply to a stranger: the self row carries TRUE, so
        // it can never be filtered.
        let alice_to_stranger = post(alice, "<p>@stranger me too</p>", Some(stranger_post)).await;

        // A person follow defaults to on, and on means today's behaviour.
        let page = home_ids(&pool, alice).await;
        for id in [bob_post, self_reply, to_alice, to_carol, to_stranger] {
            assert!(page.contains(&id), "flag on shows every reply");
        }

        set_with_replies(&pool, alice, bob, false).await;
        let page = home_ids(&pool, alice).await;
        assert!(page.contains(&bob_post), "originals are unaffected");
        assert!(page.contains(&self_reply), "a self-thread stays");
        assert!(page.contains(&to_alice), "a reply to me stays");
        assert!(
            page.contains(&to_carol),
            "a reply to someone I follow stays"
        );
        assert!(!page.contains(&to_stranger), "a reply to a stranger goes");
        assert!(page.contains(&alice_to_stranger), "my own replies stay");
        assert!(
            page.contains(&carol_post),
            "the flag is per-follow: carol is unaffected"
        );
    }

    /// A reply whose parent never arrived stays in home. Deliberate, and a
    /// divergence from Mastodon (which drops it) — see `FEEDS_DESIGN`.
    #[sqlx::test]
    async fn home_keeps_replies_whose_parent_never_arrived(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = remote_account(&pool).await;
        crate::follow::create(&pool, alice, bob, None)
            .await
            .unwrap();
        set_with_replies(&pool, alice, bob, false).await;

        let orphan = upsert_remote(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some("https://remote.example/users/nobody/statuses/999"),
                ..remote_note(
                    "https://remote.example/users/bob/statuses/1",
                    bob,
                    "<p>?</p>",
                )
            },
        )
        .await
        .unwrap();

        assert!(
            home_ids(&pool, alice).await.contains(&orphan.id),
            "an unclassifiable reply stays in a follow feed"
        );
    }

    /// With the flag off, an Announce is judged by what it announces —
    /// the reason a community follow filters anything at all, since a
    /// community authors nothing and every wrapper's own `in_reply_to_id` is
    /// NULL.
    #[sqlx::test]
    async fn home_with_replies_off_judges_a_boost_by_its_target(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let community = local_account(&pool, "community").await;
        let stranger = local_account(&pool, "stranger").await;
        sqlx::query!(
            "UPDATE accounts SET actor_type = 'Group' WHERE id = $1",
            community
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::follow::create(&pool, alice, community, None)
            .await
            .unwrap();
        // The follow arrived with replies off, from the target's actor type.
        assert!(
            !crate::follow::find(&pool, alice, community)
                .await
                .unwrap()
                .unwrap()
                .with_replies
        );

        let post = async |author: i64, text: &str, parent: Option<i64>| {
            create_local(&pool, NewLocalStatus::new(author, text, "public", parent))
                .await
                .unwrap()
                .id
        };
        let thread = post(stranger, "<p>thread</p>", None).await;
        let comment = post(stranger, "<p>comment</p>", Some(thread)).await;
        let announced_thread = create_local_reblog(&pool, community, thread)
            .await
            .unwrap()
            .id;
        let announced_comment = create_local_reblog(&pool, community, comment)
            .await
            .unwrap()
            .id;

        let page = home_ids(&pool, alice).await;
        assert!(
            page.contains(&announced_thread),
            "an announced top-level post is kept"
        );
        assert!(
            !page.contains(&announced_comment),
            "an announced comment is dropped"
        );

        // Flag on: an operator who wants the firehose still gets it.
        set_with_replies(&pool, alice, community, true).await;
        let page = home_ids(&pool, alice).await;
        assert!(page.contains(&announced_thread));
        assert!(page.contains(&announced_comment));
    }

    /// The reply-target fold rides the existing `instance_domain_allowed` probe, so
    /// that probe's own effect must survive: a boost of a *top-level* post
    /// from a suspended domain stays hidden, flag either way. And an announce
    /// of a reply whose parent never arrived is kept, matching home's standing
    /// treatment of that state (and not the public timeline's).
    #[sqlx::test]
    async fn boost_target_probe_keeps_its_domain_check_and_orphan_stance(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let booster = local_account(&pool, "booster").await;
        let blocked = remote_account(&pool).await;
        crate::follow::create(&pool, alice, booster, None)
            .await
            .unwrap();
        set_with_replies(&pool, alice, booster, false).await;

        let far_post = upsert_remote(
            &pool,
            remote_note(
                "https://remote.example/users/bob/statuses/1",
                blocked,
                "<p>far</p>",
            ),
        )
        .await
        .unwrap();
        let orphan_reply = upsert_remote(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some("https://remote.example/users/bob/statuses/gone"),
                ..remote_note(
                    "https://remote.example/users/bob/statuses/2",
                    blocked,
                    "<p>orphan</p>",
                )
            },
        )
        .await
        .unwrap();
        let boost_far = create_local_reblog(&pool, booster, far_post.id)
            .await
            .unwrap()
            .id;
        let boost_orphan = create_local_reblog(&pool, booster, orphan_reply.id)
            .await
            .unwrap()
            .id;

        let page = home_ids(&pool, alice).await;
        assert!(page.contains(&boost_far), "boosted original, no policy yet");
        assert!(
            page.contains(&boost_orphan),
            "an announce of an unclassifiable reply is kept"
        );

        crate::instance_policy::create_domain_block(
            &pool,
            crate::instance_policy::NewDomainBlock {
                domain: "remote.example",
                severity: "suspend",
                reject_media: false,
                reject_reports: false,
                private_comment: None,
                public_comment: None,
                obfuscate: false,
            },
        )
        .await
        .unwrap();

        let page = home_ids(&pool, alice).await;
        assert!(
            !page.contains(&boost_far),
            "the domain check the reply-target test was folded into still applies"
        );
        assert!(!page.contains(&boost_orphan));
    }

    /// Arm B is left alone: its rationale is "you follow #tag", not "you
    /// follow them", so a tag-injected reply is not the follow flag's business.
    #[sqlx::test]
    async fn tag_injection_ignores_the_per_follow_reply_flag(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        crate::follow::create(&pool, alice, bob, None)
            .await
            .unwrap();
        set_with_replies(&pool, alice, bob, false).await;
        let tag_id = crate::tag::ensure(&pool, "hi").await.unwrap();
        crate::tag::follow(&pool, alice, tag_id).await.unwrap();

        // bob answers a stranger — the arm-A filter drops this row, but arm B
        // has its own reply rule (parent authored by the viewer, the poster,
        // or someone the viewer follows), and here bob replies to himself.
        let root = create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let tagged_reply = create_local(
            &pool,
            NewLocalStatus::new(bob, "<p>#hi again</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        crate::tag::attach(&pool, tagged_reply.id, tag_id)
            .await
            .unwrap();

        assert!(
            home_ids(&pool, alice).await.contains(&tagged_reply.id),
            "a followed hashtag still injects the post"
        );
    }
}
