//! Hashtags.

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool};
use time::{Date, OffsetDateTime};

use crate::status::Status;
use crate::user::TimelineOrder;
use crate::{DbError, id};

/// Finds or creates a tag (case-insensitively), returning its id.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn ensure<'e, E: PgExecutor<'e>>(executor: E, name: &str) -> Result<i64, DbError> {
    let tag_id = sqlx::query_scalar!(
        r#"
        INSERT INTO tags (id, name)
        VALUES ($1, $2)
        ON CONFLICT (lower(name)) DO UPDATE SET name = tags.name
        RETURNING id
        "#,
        id::next(),
        name,
    )
    .fetch_one(executor)
    .await?;
    Ok(tag_id)
}

pub async fn attach<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
    tag_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"INSERT INTO status_tags (status_id, tag_id, sort_at)
           VALUES ($1, $2, (SELECT sort_at FROM statuses WHERE id = $1))
           ON CONFLICT DO NOTHING"#,
        status_id,
        tag_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// The set-based [`ensure`] + [`attach`]: upserts every hashtag name and links
/// the set to one status in two statements instead of two per tag. Names that
/// collide case-insensitively are deduplicated in the statement itself —
/// `DISTINCT ON (lower(name))` keeps the first spelling, as sequential
/// [`ensure`] calls would, and uses the same `lower()` the unique index does,
/// so two same-statement upserts can never hit one row.
pub async fn ensure_and_attach_many(
    conn: &mut sqlx::PgConnection,
    status_id: i64,
    names: &[&str],
) -> Result<(), DbError> {
    if names.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = names.iter().map(|_| id::next()).collect();
    let tag_ids = sqlx::query_scalar!(
        r#"
        INSERT INTO tags (id, name)
        SELECT DISTINCT ON (lower(v.name)) v.id, v.name
        FROM unnest($1::bigint[], $2::text[]) WITH ORDINALITY AS v(id, name, ord)
        ORDER BY lower(v.name), v.ord
        ON CONFLICT (lower(name)) DO UPDATE SET name = tags.name
        RETURNING id
        "#,
        &ids,
        &names as &[&str],
    )
    .fetch_all(&mut *conn)
    .await?;
    sqlx::query!(
        r#"INSERT INTO status_tags (status_id, tag_id, sort_at)
           SELECT $1, t.tag_id, (SELECT sort_at FROM statuses WHERE id = $1)
           FROM unnest($2::bigint[]) AS t(tag_id)
           ON CONFLICT DO NOTHING"#,
        status_id,
        &tag_ids,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Tag names per status, for a batch of statuses.
/// Removes all tag links of a status (edits rebuild them).
pub async fn detach_all<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM status_tags WHERE status_id = $1", status_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn names_for_statuses<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<HashMap<i64, Vec<String>>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT st.status_id AS "status_id!", t.name AS "name!"
        FROM status_tags st
        JOIN tags t ON t.id = st.tag_id
        WHERE st.status_id = ANY($1)
        ORDER BY t.name
        "#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        map.entry(row.status_id).or_default().push(row.name);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Usage history — the Postgres equivalent of Mastodon's Redis trend history.
// ---------------------------------------------------------------------------

/// Records `status`'s usable-tag usage into the daily rollup for `day` (a UTC
/// calendar day), mirroring Mastodon's `Trends.tags.register`: skipped for
/// reblogs, non-public statuses, silenced authors and non-usable tags. Each
/// call bumps `uses` by one per tag and, on the account's first use of a tag
/// that day, adds it to that day's distinct-account set (a fresh row). Runs off
/// the already-attached `status_tags`, so call it after `attach`.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn record_uses<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
    day: Date,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO tag_usages (tag_id, day, account_id, uses)
        SELECT st.tag_id, $2, s.account_id, 1
        FROM statuses s
        JOIN status_tags st ON st.status_id = s.id
        JOIN tags t ON t.id = st.tag_id
        JOIN accounts a ON a.id = s.account_id
        WHERE s.id = $1
          AND s.reblog_of_id IS NULL
          AND s.visibility = 'public'
          AND NOT account_silenced(a.id)
          AND t.usable IS NOT FALSE
        ON CONFLICT (tag_id, day, account_id)
        DO UPDATE SET uses = tag_usages.uses + 1
        "#,
        status_id,
        day,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// One day's usage of a tag: total `uses` and distinct `accounts`.
#[derive(Debug, Clone)]
pub struct DayCount {
    pub tag_id: i64,
    pub day: Date,
    pub uses: i64,
    pub accounts: i64,
}

/// A tag's per-day usage on or after `from` (typically the start of the 7-day
/// window). Only non-empty days are returned; the caller fills the gaps.
pub async fn history(pool: &PgPool, tag_id: i64, from: Date) -> Result<Vec<DayCount>, DbError> {
    let rows = sqlx::query_as!(
        DayCount,
        r#"
        SELECT tag_id AS "tag_id!", day AS "day!",
               COALESCE(sum(uses), 0)::bigint AS "uses!",
               count(*)::bigint AS "accounts!"
        FROM tag_usages
        WHERE tag_id = $1 AND day >= $2
        GROUP BY tag_id, day
        "#,
        tag_id,
        from,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Per-day usage for a batch of tags (search results, followed lists), keyed by
/// tag id. Same semantics as [`history`].
pub async fn history_batch(
    pool: &PgPool,
    tag_ids: &[i64],
    from: Date,
) -> Result<HashMap<i64, Vec<DayCount>>, DbError> {
    let rows = sqlx::query_as!(
        DayCount,
        r#"
        SELECT tag_id AS "tag_id!", day AS "day!",
               COALESCE(sum(uses), 0)::bigint AS "uses!",
               count(*)::bigint AS "accounts!"
        FROM tag_usages
        WHERE tag_id = ANY($1) AND day >= $2
        GROUP BY tag_id, day
        "#,
        tag_ids,
        from,
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<i64, Vec<DayCount>> = HashMap::new();
    for row in rows {
        map.entry(row.tag_id).or_default().push(row);
    }
    Ok(map)
}

/// A stored hashtag. `name` is the (case-insensitively unique) matching form
/// and drives the `/tags/<name>` URL; `display_name` is the optional cased
/// override a moderator can set, shown in the Tag entity's `name` field.
#[derive(Debug, Clone)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
}

impl Tag {
    /// The casing shown to clients — the `display_name` override, else `name`.
    #[must_use]
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// A tag with its moderation registry, for the `Admin::Tag` serializer. The
/// three booleans are `None` when unset (meaning the default: usable/listable →
/// true, trendable → the operator's `trendable_by_default`).
#[derive(Debug, Clone)]
pub struct AdminTag {
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
    pub usable: Option<bool>,
    pub listable: Option<bool>,
    pub trendable: Option<bool>,
    pub reviewed_at: Option<OffsetDateTime>,
}

impl AdminTag {
    /// The casing shown to clients — the `display_name` override, else `name`.
    #[must_use]
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// Still awaits moderator review, matching Mastodon's reviewable
    /// semantics — never reviewed.
    #[must_use]
    pub fn requires_review(&self) -> bool {
        self.reviewed_at.is_none()
    }
}

/// All tags, newest id first, keyset-paginated by id (Mastodon's admin
/// `Tag.all.to_a_paginated_by_id`).
pub async fn admin_list(
    pool: &PgPool,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<AdminTag>, DbError> {
    if let Some(min_id) = min_id {
        let mut rows = sqlx::query_as!(
            AdminTag,
            r#"
            SELECT id, name, display_name, usable, listable, trendable, reviewed_at
            FROM tags
            WHERE id > $1 AND ($2::bigint IS NULL OR id < $2)
            ORDER BY id ASC
            LIMIT $3
            "#,
            min_id,
            max_id,
            limit,
        )
        .fetch_all(pool)
        .await?;
        rows.reverse();
        return Ok(rows);
    }
    let rows = sqlx::query_as!(
        AdminTag,
        r#"
        SELECT id, name, display_name, usable, listable, trendable, reviewed_at
        FROM tags
        WHERE ($1::bigint IS NULL OR id < $1) AND ($2::bigint IS NULL OR id > $2)
        ORDER BY id DESC
        LIMIT $3
        "#,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A single tag's admin view by id.
pub async fn admin_find(pool: &PgPool, id: i64) -> Result<Option<AdminTag>, DbError> {
    let row = sqlx::query_as!(
        AdminTag,
        r#"
        SELECT id, name, display_name, usable, listable, trendable, reviewed_at
        FROM tags WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Applies a moderator's registry edit (any subset of the fields; `None` leaves
/// a field unchanged) and stamps `reviewed_at`, like Mastodon's admin `update!`.
/// Returns the updated row, or `None` if the id is unknown. Both the
/// `admin/tags` update and the `admin/trends/tags` approve/reject funnel here.
#[allow(clippy::too_many_arguments)]
pub async fn admin_update(
    pool: &PgPool,
    id: i64,
    display_name: Option<&str>,
    usable: Option<bool>,
    listable: Option<bool>,
    trendable: Option<bool>,
    reviewed_at: OffsetDateTime,
) -> Result<Option<AdminTag>, DbError> {
    let row = sqlx::query_as!(
        AdminTag,
        r#"
        UPDATE tags SET
            display_name = COALESCE($2, display_name),
            usable       = COALESCE($3, usable),
            listable     = COALESCE($4, listable),
            trendable    = COALESCE($5, trendable),
            reviewed_at  = $6
        WHERE id = $1
        RETURNING id, name, display_name, usable, listable, trendable, reviewed_at
        "#,
        id,
        display_name,
        usable,
        listable,
        trendable,
        reviewed_at,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Prefix search over tag names, shortest (= closest) matches first, like
/// Mastodon's database-backed `Tag.search_for`. An exact match therefore
/// always sorts first.
pub async fn search(
    pool: &PgPool,
    term: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<Tag>, DbError> {
    // `term` is user input: escape LIKE metacharacters so they match
    // literally.
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let tags = sqlx::query_as!(
        Tag,
        r#"
        SELECT id, name, display_name FROM tags
        WHERE lower(name) LIKE lower($1) || '%'
        ORDER BY length(name), lower(name)
        LIMIT $2 OFFSET $3
        "#,
        escaped,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(tags)
}

/// Public statuses carrying a tag, newest first, keyset-paginated (the cursor
/// is a status id under either ordering), minus authors hidden from the
/// (optional) viewer.
pub async fn timeline(
    pool: &PgPool,
    name: &str,
    viewer: Option<i64>,
    order: TimelineOrder,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    // Resolve the tag once (`lower(name)` is unique) so both orderings can
    // drive the status_tags indexes with a bare tag id. A never-seen tag has
    // no memberships: empty page.
    let Some(tag) = find_by_name(pool, name).await? else {
        return Ok(Vec::new());
    };
    match order {
        TimelineOrder::Published => {
            let anchor = crate::status::sort_anchor(pool, max_id).await?;
            timeline_published(pool, tag.id, viewer, anchor, limit).await
        }
        TimelineOrder::Received => timeline_received(pool, tag.id, viewer, max_id, limit).await,
    }
}

// The two orderings are separate compile-checked queries whose status-side
// filters must stay in sync; they differ in the keyset predicate, the ORDER
// BY, and which status_tags index drives the scan (`received` nested-loops
// from idx_status_tags_tag, `published` from idx_status_tags_tag_sort_at —
// the denormalized-membership index from migration 0131).

async fn timeline_received(
    pool: &PgPool,
    tag_id: i64,
    viewer: Option<i64>,
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
        JOIN status_tags st ON st.status_id = s.id
        JOIN accounts a ON a.id = s.account_id
        WHERE st.tag_id = $1
          AND s.visibility = 'public' AND s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.ingest_provenance = 'delivery'
          AND NOT account_hidden($2, s.account_id)
          AND NOT account_silenced(s.account_id)
          AND (a.portable OR instance_domain_allowed(a.domain))
          AND ($3::bigint IS NULL OR s.id < $3)
        ORDER BY s.id DESC
        LIMIT $4
        "#,
        tag_id,
        viewer,
        max_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

async fn timeline_published(
    pool: &PgPool,
    tag_id: i64,
    viewer: Option<i64>,
    anchor: Option<(OffsetDateTime, i64)>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let (anchor_sort_at, anchor_id) = anchor.unzip();
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id AS "id!", s.uri, s.account_id AS "account_id!", s.content AS "content!",
               s.created_at AS "created_at!", s.updated_at AS "updated_at!",
               s.visibility AS "visibility!", s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text AS "spoiler_text!", s.sensitive AS "sensitive!", s.language, s.url,
               s.quote_approval_policy AS "quote_approval_policy!", s.application_id,
               s.title, s.object_type, s.external_url
        FROM (
            -- Drive the tag's feed-ordered membership and stop at LIMIT.
            -- `status_tags.sort_at` is the status' immutable sort_at
            -- denormalized by migration 0131, so the keyset lands inside the
            -- idx_status_tags_tag_sort_at scan. OFFSET 0 is an intentional
            -- planner barrier: it keeps that ordered membership as the nested
            -- loop's outer path instead of flattening into a gather-then-sort
            -- join (the shape that cost ~66ms; see the home timeline's arm B).
            SELECT status_id, sort_at
            FROM status_tags
            WHERE tag_id = $1
              AND ($3::timestamptz IS NULL OR (sort_at, status_id) < ($3, $4::bigint))
            ORDER BY sort_at DESC, status_id DESC
            OFFSET 0
        ) st
        JOIN statuses s ON s.id = st.status_id
        JOIN accounts a ON a.id = s.account_id
        WHERE s.visibility = 'public' AND s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.ingest_provenance = 'delivery'
          AND NOT account_hidden($2, s.account_id)
          AND NOT account_silenced(s.account_id)
          AND (a.portable OR instance_domain_allowed(a.domain))
        ORDER BY st.sort_at DESC, st.status_id DESC
        LIMIT $5
        "#,
        tag_id,
        viewer,
        anchor_sort_at,
        anchor_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// A tag by name (case-insensitive), or `None` if it has never been seen.
pub async fn find_by_name(pool: &PgPool, name: &str) -> Result<Option<Tag>, DbError> {
    let tag = sqlx::query_as!(
        Tag,
        "SELECT id, name, display_name FROM tags WHERE lower(name) = lower($1)",
        name,
    )
    .fetch_optional(pool)
    .await?;
    Ok(tag)
}

/// The display name of a tag by id, or `None` if it does not exist.
pub async fn name_of<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    tag_id: i64,
) -> Result<Option<String>, DbError> {
    let name = sqlx::query_scalar!("SELECT name FROM tags WHERE id = $1", tag_id)
        .fetch_optional(pool)
        .await?;
    Ok(name)
}

/// [`name_of`] over a set of ids in one query, keyed by tag id. Unknown ids
/// are simply absent.
pub async fn names_of<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    tag_ids: &[i64],
) -> Result<std::collections::HashMap<i64, String>, DbError> {
    if tag_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query!("SELECT id, name FROM tags WHERE id = ANY($1)", tag_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|row| (row.id, row.name)).collect())
}

/// Records `account` following `tag` (idempotent, like Mastodon's
/// `find_or_create_by!`).
pub async fn follow(pool: &PgPool, account_id: i64, tag_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO tag_follows (id, account_id, tag_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, tag_id) DO NOTHING
        "#,
        id::next(),
        account_id,
        tag_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Drops a tag follow, returning whether one existed.
pub async fn unfollow(pool: &PgPool, account_id: i64, tag_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM tag_follows WHERE account_id = $1 AND tag_id = $2",
        account_id,
        tag_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Whether `account` follows `tag`.
pub async fn is_following(pool: &PgPool, account_id: i64, tag_id: i64) -> Result<bool, DbError> {
    let following = sqlx::query_scalar!(
        r#"SELECT EXISTS (
            SELECT 1 FROM tag_follows WHERE account_id = $1 AND tag_id = $2
        ) AS "following!""#,
        account_id,
        tag_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(following)
}

/// Of `tag_ids`, which ones `account` follows — for rendering the `following`
/// flag on a batch of `Tag` entities (e.g. search results).
pub async fn followed_ids(
    pool: &PgPool,
    account_id: i64,
    tag_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT tag_id AS "tag_id!" FROM tag_follows
           WHERE account_id = $1 AND tag_id = ANY($2)"#,
        account_id,
        tag_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The followed-hashtag names on each given status, keyed by status id, for the
/// home hashtag-source banner — but returned **only** for statuses that reached
/// the feed *solely* via a followed tag (`status::home_timeline` arm B,
/// Mastodon's `deliver_to_hashtag_followers!`): an original (not a boost) whose
/// author is neither the viewer nor an account the viewer follows. A post the
/// viewer would see anyway — their own, a followed account's, or a boost — is
/// omitted, as is a post carrying only tags the viewer does not follow, so every
/// returned id warrants the "in your feed because you follow #tag" banner.
///
/// This is the typed, single-query provenance the web home page reads instead of
/// reconstructing the reason from rendered entity JSON (QC #5): it replaces the
/// prior followed-names lookup, the separate follow-membership probe, and the
/// in-Rust tag intersection with one round trip, and matching by `tag_id` makes
/// it exact where the old stored-name intersection was heuristic. Names are
/// sorted so the banner order is stable; an empty `status_ids` returns nothing.
pub async fn followed_tag_sources(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<Vec<(i64, Vec<String>)>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT s.id AS "status_id!",
               array_agg(t.name ORDER BY t.name) AS "names!"
        FROM statuses s
        JOIN status_tags st ON st.status_id = s.id
        JOIN tag_follows tf ON tf.tag_id = st.tag_id AND tf.account_id = $1
        JOIN tags t ON t.id = st.tag_id
        WHERE s.id = ANY($2)
          AND s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.account_id <> $1
          AND NOT EXISTS (
              SELECT 1 FROM follows f
              WHERE f.account_id = $1
                AND f.target_account_id = s.account_id
                AND NOT f.pending)
        GROUP BY s.id
        "#,
        account_id,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.status_id, r.names)).collect())
}

/// One row of the `followed_tags` listing: the tag plus the follow row id
/// that keyset-paginates it (Mastodon's `Link` ids are `TagFollow` ids).
#[derive(Debug, Clone)]
pub struct FollowedTag {
    pub follow_id: i64,
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
}

/// An account's followed tags, newest follow first, keyset-paginated by the
/// follow row id like Mastodon's `to_a_paginated_by_id`.
pub async fn followed(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<FollowedTag>, DbError> {
    if let Some(min_id) = min_id {
        let mut rows = sqlx::query_as!(
            FollowedTag,
            r#"
            SELECT tf.id AS "follow_id!", t.id AS "id!", t.name AS "name!",
                   t.display_name
            FROM tag_follows tf
            JOIN tags t ON t.id = tf.tag_id
            WHERE tf.account_id = $1 AND tf.id > $2
              AND ($3::bigint IS NULL OR tf.id < $3)
            ORDER BY tf.id ASC
            LIMIT $4
            "#,
            account_id,
            min_id,
            max_id,
            limit,
        )
        .fetch_all(pool)
        .await?;
        rows.reverse();
        return Ok(rows);
    }
    let rows = sqlx::query_as!(
        FollowedTag,
        r#"
        SELECT tf.id AS "follow_id!", t.id AS "id!", t.name AS "name!",
               t.display_name
        FROM tag_follows tf
        JOIN tags t ON t.id = tf.tag_id
        WHERE tf.account_id = $1
          AND ($2::bigint IS NULL OR tf.id < $2)
          AND ($3::bigint IS NULL OR tf.id > $3)
        ORDER BY tf.id DESC
        LIMIT $4
        "#,
        account_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status;

    #[sqlx::test]
    async fn tags_dedupe_case_insensitively_and_drive_the_timeline(pool: PgPool) {
        let account = account::create_local(
            &pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let public = status::create_local(
            &pool,
            status::NewLocalStatus::new(account.id, "<p>a</p>", "public", None),
        )
        .await
        .unwrap();
        let unlisted = status::create_local(
            &pool,
            status::NewLocalStatus::new(account.id, "<p>b</p>", "unlisted", None),
        )
        .await
        .unwrap();

        let first = ensure(&pool, "Rust").await.unwrap();
        let second = ensure(&pool, "rust").await.unwrap();
        assert_eq!(first, second, "tags are case-insensitive");

        attach(&pool, public.id, first).await.unwrap();
        attach(&pool, public.id, first).await.unwrap(); // idempotent
        attach(&pool, unlisted.id, first).await.unwrap();

        let names = names_for_statuses(&pool, &[public.id]).await.unwrap();
        assert_eq!(names[&public.id], ["Rust"]);

        // Only the public status appears on the tag timeline.
        let items = timeline(&pool, "RUST", None, TimelineOrder::Published, None, 20)
            .await
            .unwrap();
        assert_eq!(items.iter().map(|s| s.id).collect::<Vec<_>>(), [public.id]);
    }

    async fn local(pool: &PgPool, username: &str) -> account::Account {
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
    }

    /// A late-arriving old post sinks to its publish date under the default
    /// ordering but leads under ingest order — same rows either way (the two
    /// timeline queries are duplicated per ordering and must not drift).
    #[sqlx::test]
    async fn timeline_orderings_disagree_on_backfilled_posts(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let rust = ensure(&pool, "rust").await.unwrap();

        let fresh = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, "<p>now</p>", "public", None),
        )
        .await
        .unwrap();
        attach(&pool, fresh.id, rust).await.unwrap();
        // Ingested after `fresh` but published long before it.
        let old = status::upsert_remote(
            &pool,
            status::NewRemoteStatus {
                title: None,
                object_type: None,
                external_url: None,
                uri: "https://remote.example/users/bob/statuses/old",
                account_id: bob.id,
                content: "<p>old</p>",
                created_at: time::macros::datetime!(2026-01-01 00:00 UTC),
                visibility: "public",
                in_reply_to_id: None,
                in_reply_to_uri: None,
                spoiler_text: "",
                sensitive: false,
                language: None,
                url: None,
                quote_approval_policy: 0,
            },
        )
        .await
        .unwrap();
        attach(&pool, old.id, rust).await.unwrap();

        let published = timeline(&pool, "rust", None, TimelineOrder::Published, None, 20)
            .await
            .unwrap();
        assert_eq!(
            published.iter().map(|s| s.id).collect::<Vec<_>>(),
            [fresh.id, old.id]
        );
        let received = timeline(&pool, "rust", None, TimelineOrder::Received, None, 20)
            .await
            .unwrap();
        assert_eq!(
            received.iter().map(|s| s.id).collect::<Vec<_>>(),
            [old.id, fresh.id]
        );
    }

    #[sqlx::test]
    async fn follow_is_idempotent_and_listed(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let rust = ensure(&pool, "Rust").await.unwrap();
        let go = ensure(&pool, "Go").await.unwrap();

        assert!(!is_following(&pool, alice.id, rust).await.unwrap());
        follow(&pool, alice.id, rust).await.unwrap();
        follow(&pool, alice.id, rust).await.unwrap(); // idempotent
        follow(&pool, alice.id, go).await.unwrap();
        assert!(is_following(&pool, alice.id, rust).await.unwrap());

        let followed_set = followed_ids(&pool, alice.id, &[rust, go, 999])
            .await
            .unwrap();
        assert_eq!(followed_set.len(), 2);
        assert!(followed_set.contains(&rust) && followed_set.contains(&go));

        // Newest follow (Go) first, paginated by follow row id.
        let listing = followed(&pool, alice.id, None, None, None, 20)
            .await
            .unwrap();
        assert_eq!(listing.iter().map(|t| t.id).collect::<Vec<_>>(), [go, rust]);

        assert!(unfollow(&pool, alice.id, rust).await.unwrap());
        assert!(!unfollow(&pool, alice.id, rust).await.unwrap()); // already gone
        assert!(!is_following(&pool, alice.id, rust).await.unwrap());
    }

    #[sqlx::test]
    async fn followed_tag_injects_public_originals_into_home(pool: PgPool) {
        let alice = local(&pool, "alice").await; // the hashtag follower
        let bob = local(&pool, "bob").await; // a stranger alice does not follow

        let tagged = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, "<p>hi</p>", "public", None),
        )
        .await
        .unwrap();
        let untagged = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, "<p>off-topic</p>", "public", None),
        )
        .await
        .unwrap();
        let unlisted = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, "<p>quiet</p>", "unlisted", None),
        )
        .await
        .unwrap();
        let rust = ensure(&pool, "rust").await.unwrap();
        let systems = ensure(&pool, "systems").await.unwrap();
        attach(&pool, tagged.id, rust).await.unwrap();
        // A status carrying two followed tags must still occur only once after
        // the per-tag lateral candidates are merged.
        attach(&pool, tagged.id, systems).await.unwrap();
        attach(&pool, unlisted.id, rust).await.unwrap();

        // Before following, nothing from bob is on alice's home.
        let before = status::home_timeline(&pool, alice.id, TimelineOrder::Published, None, 20)
            .await
            .unwrap();
        assert!(before.is_empty());

        follow(&pool, alice.id, rust).await.unwrap();
        follow(&pool, alice.id, systems).await.unwrap();
        for order in [TimelineOrder::Published, TimelineOrder::Received] {
            let home = status::home_timeline(&pool, alice.id, order, None, 20)
                .await
                .unwrap();
            let ids: Vec<i64> = home.iter().map(|s| s.id).collect();
            // Only one copy of the public tagged original — not the untagged
            // post, and not the unlisted one (Mastodon injects public only).
            assert_eq!(ids, [tagged.id], "order={order:?}");
            assert!(!ids.contains(&untagged.id));
            assert!(!ids.contains(&unlisted.id));
        }

        unfollow(&pool, alice.id, rust).await.unwrap();
        unfollow(&pool, alice.id, systems).await.unwrap();
        let after = status::home_timeline(&pool, alice.id, TimelineOrder::Published, None, 20)
            .await
            .unwrap();
        assert!(after.is_empty());
    }

    #[sqlx::test]
    async fn followed_tag_filters_replies_to_strangers(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await; // a stranger to alice

        let rust = ensure(&pool, "rust").await.unwrap();
        follow(&pool, alice.id, rust).await.unwrap();

        let carol_root = status::create_local(
            &pool,
            status::NewLocalStatus::new(carol.id, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        // Bob replies to carol (alice follows neither) with the followed tag.
        let reply_to_stranger = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, "<p>re</p>", "public", Some(carol_root.id)),
        )
        .await
        .unwrap();
        attach(&pool, reply_to_stranger.id, rust).await.unwrap();
        // Bob's self-reply with the tag is allowed.
        let bob_root = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, "<p>thread</p>", "public", None),
        )
        .await
        .unwrap();
        let self_reply = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, "<p>more</p>", "public", Some(bob_root.id)),
        )
        .await
        .unwrap();
        attach(&pool, self_reply.id, rust).await.unwrap();

        let home = status::home_timeline(&pool, alice.id, TimelineOrder::Published, None, 20)
            .await
            .unwrap();
        let ids: Vec<i64> = home.iter().map(|s| s.id).collect();
        assert!(ids.contains(&self_reply.id), "self-reply injected");
        assert!(
            !ids.contains(&reply_to_stranger.id),
            "reply to a non-followed account is filtered"
        );
    }

    #[sqlx::test]
    async fn usage_history_counts_uses_and_distinct_accounts(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let today = OffsetDateTime::now_utc().date();
        let rust = ensure(&pool, "rust").await.unwrap();

        // Alice uses #rust on two public posts, bob on one.
        for (author, vis) in [
            (alice.id, "public"),
            (alice.id, "public"),
            (bob.id, "public"),
            // A private post with the tag is not registered.
            (alice.id, "private"),
        ] {
            let s = status::create_local(
                &pool,
                status::NewLocalStatus::new(author, "<p>#rust</p>", vis, None),
            )
            .await
            .unwrap();
            attach(&pool, s.id, rust).await.unwrap();
            record_uses(&pool, s.id, today).await.unwrap();
        }

        let hist = history(&pool, rust, today - time::Duration::days(6))
            .await
            .unwrap();
        assert_eq!(hist.len(), 1, "only today has usage");
        assert_eq!(hist[0].day, today);
        assert_eq!(
            hist[0].uses, 3,
            "three public uses; the private one skipped"
        );
        assert_eq!(hist[0].accounts, 2, "two distinct accounts");

        // A non-usable tag is skipped entirely by `record_uses`.
        let spam = ensure(&pool, "spam").await.unwrap();
        sqlx::query!("UPDATE tags SET usable = false WHERE id = $1", spam)
            .execute(&pool)
            .await
            .unwrap();
        let s = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, "<p>#spam</p>", "public", None),
        )
        .await
        .unwrap();
        attach(&pool, s.id, spam).await.unwrap();
        record_uses(&pool, s.id, today).await.unwrap();
        assert!(
            history(&pool, spam, today - time::Duration::days(6))
                .await
                .unwrap()
                .is_empty(),
            "non-usable tag records nothing"
        );

        // The batch variant keys by tag id.
        let batch = history_batch(&pool, &[rust, spam], today - time::Duration::days(6))
            .await
            .unwrap();
        assert_eq!(batch[&rust][0].uses, 3);
        assert!(!batch.contains_key(&spam));
    }

    #[sqlx::test]
    async fn search_prefix_matches_shortest_first(pool: PgPool) {
        for name in ["rustlang", "Rust", "rusty", "crust"] {
            ensure(&pool, name).await.unwrap();
        }

        let hits = search(&pool, "rus", 10, 0).await.unwrap();
        let names: Vec<&str> = hits.iter().map(|t| t.name.as_str()).collect();
        // Prefix-only (no "crust"), exact-length matches first.
        assert_eq!(names, ["Rust", "rusty", "rustlang"]);

        let paged = search(&pool, "rus", 10, 2).await.unwrap();
        assert_eq!(paged.len(), 1);

        // LIKE metacharacters match literally, not as wildcards.
        assert!(search(&pool, "%", 10, 0).await.unwrap().is_empty());
        assert!(search(&pool, "ru_t", 10, 0).await.unwrap().is_empty());
    }

    /// The typed provenance behind the home hashtag-source banner: given a page
    /// of status ids, only the ones injected *solely* by a followed tag come
    /// back, each with the followed tag names on it (sorted). A post the viewer
    /// authored, a followed account's post, and a post carrying only unfollowed
    /// tags are all omitted even when they carry a followed tag, and an
    /// unfollowed tag never contributes a name.
    #[sqlx::test]
    async fn followed_tag_sources_flags_only_tag_injected_posts(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await; // a stranger to alice
        let carol = local(&pool, "carol").await; // alice follows carol
        crate::follow::create(&pool, alice.id, carol.id, None)
            .await
            .unwrap();

        let rust = ensure(&pool, "rust").await.unwrap();
        let go = ensure(&pool, "go").await.unwrap();
        let cpp = ensure(&pool, "cpp").await.unwrap(); // alice does not follow this
        follow(&pool, alice.id, rust).await.unwrap();
        follow(&pool, alice.id, go).await.unwrap();

        // The stranger's post carries two followed tags and one unfollowed one:
        // it is a tag injection, and only the followed tags name it (sorted).
        let injected = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();
        attach(&pool, injected.id, rust).await.unwrap();
        attach(&pool, injected.id, go).await.unwrap();
        attach(&pool, injected.id, cpp).await.unwrap();
        // A followed account's tagged post is in the feed via the follow.
        let followed_author = status::create_local(
            &pool,
            status::NewLocalStatus::new(carol.id, "<p>y</p>", "public", None),
        )
        .await
        .unwrap();
        attach(&pool, followed_author.id, rust).await.unwrap();
        // The viewer's own tagged post.
        let own = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, "<p>z</p>", "public", None),
        )
        .await
        .unwrap();
        attach(&pool, own.id, rust).await.unwrap();
        // A stranger's post with only an unfollowed tag has no followed source.
        let unfollowed_only = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, "<p>w</p>", "public", None),
        )
        .await
        .unwrap();
        attach(&pool, unfollowed_only.id, cpp).await.unwrap();

        let ids = [injected.id, followed_author.id, own.id, unfollowed_only.id];
        let sources = followed_tag_sources(&pool, alice.id, &ids).await.unwrap();
        assert_eq!(
            sources,
            vec![(injected.id, vec!["go".to_owned(), "rust".to_owned()])],
            "only the stranger's injection, followed tags sorted, unfollowed excluded",
        );

        // The provenance is per-viewer: bob follows nothing, so nothing on the
        // same page is a tag injection for him; an empty page returns nothing.
        assert!(
            followed_tag_sources(&pool, bob.id, &ids)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            followed_tag_sources(&pool, alice.id, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }
}
