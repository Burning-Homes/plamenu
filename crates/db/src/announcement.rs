//! Announcements and their reactions/mutes (Mastodon's `Announcement` /
//! `AnnouncementReaction` / `AnnouncementMute`).
//!
//! Announcements are server-wide notices. `GET /api/v1/announcements` serves
//! only the published ones in chronological order ([`published_chronological`]),
//! while management (create/publish/destroy) is admin-only — the `plamenu
//! announcement` CLI for now, the web dashboard later (Mastodon has no REST
//! surface for it). An announcement auto-publishes on create unless its
//! `scheduled_at` is still in the future (Mastodon's `set_published`).
//!
//! A reaction is an emoji a logged-in user attaches: a Unicode emoji
//! (`custom_emoji_id` NULL) or a LOCAL custom emoji. Reactions are aggregated
//! per emoji at render time into the entity's `reactions`. A mute is the
//! per-account read marker behind the entity's `read` flag and the
//! `POST .../dismiss` endpoint.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Announcement {
    pub id: i64,
    pub text: String,
    pub published: bool,
    pub all_day: bool,
    pub scheduled_at: Option<OffsetDateTime>,
    pub starts_at: Option<OffsetDateTime>,
    pub ends_at: Option<OffsetDateTime>,
    pub published_at: Option<OffsetDateTime>,
    pub notification_sent_at: Option<OffsetDateTime>,
    pub status_ids: Option<Vec<i64>>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// A new announcement. `scheduled_at` in the future keeps it unpublished until
/// a scheduler (or the admin) publishes it; otherwise it publishes on create.
#[derive(Debug, Default)]
pub struct NewAnnouncement<'a> {
    pub text: &'a str,
    pub scheduled_at: Option<OffsetDateTime>,
    pub starts_at: Option<OffsetDateTime>,
    pub ends_at: Option<OffsetDateTime>,
    pub all_day: bool,
    pub status_ids: Option<&'a [i64]>,
}

/// Fields editable from the admin dashboard. Editing does not implicitly
/// publish or unpublish the announcement; the explicit publish verbs own that
/// state transition.
#[derive(Debug)]
pub struct AnnouncementUpdate<'a> {
    pub text: &'a str,
    pub scheduled_at: Option<OffsetDateTime>,
    pub starts_at: Option<OffsetDateTime>,
    pub ends_at: Option<OffsetDateTime>,
    pub all_day: bool,
    pub status_ids: Option<&'a [i64]>,
}

/// The published announcements in chronological display order — Mastodon's
/// `Announcement.published.chronological` (ordered by the first set of
/// `starts_at`/`scheduled_at`/`published_at`/`created_at`).
pub async fn published_chronological(pool: &PgPool) -> Result<Vec<Announcement>, DbError> {
    let rows = sqlx::query_as!(
        Announcement,
        r#"
        SELECT id, text, published, all_day, scheduled_at, starts_at, ends_at,
               published_at, notification_sent_at, status_ids, created_at, updated_at
        FROM announcements
        WHERE published
        ORDER BY COALESCE(starts_at, scheduled_at, published_at, created_at), id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// All announcements, newest first — the admin/CLI listing (includes the
/// not-yet-published ones).
pub async fn list_all(pool: &PgPool) -> Result<Vec<Announcement>, DbError> {
    let rows = sqlx::query_as!(
        Announcement,
        r#"
        SELECT id, text, published, all_day, scheduled_at, starts_at, ends_at,
               published_at, notification_sent_at, status_ids, created_at, updated_at
        FROM announcements
        ORDER BY COALESCE(starts_at, scheduled_at, published_at, created_at) DESC, id DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A single published announcement (the API only exposes published ones).
pub async fn find_published(pool: &PgPool, id: i64) -> Result<Option<Announcement>, DbError> {
    let row = sqlx::query_as!(
        Announcement,
        r#"
        SELECT id, text, published, all_day, scheduled_at, starts_at, ends_at,
               published_at, notification_sent_at, status_ids, created_at, updated_at
        FROM announcements
        WHERE id = $1 AND published
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Any announcement by id, published or not — for management commands.
pub async fn find_by_id(pool: &PgPool, id: i64) -> Result<Option<Announcement>, DbError> {
    let row = sqlx::query_as!(
        Announcement,
        r#"
        SELECT id, text, published, all_day, scheduled_at, starts_at, ends_at,
               published_at, notification_sent_at, status_ids, created_at, updated_at
        FROM announcements
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Creates an announcement, auto-publishing it unless it is scheduled for the
/// future (Mastodon's `before_validation :set_published`).
pub async fn create(pool: &PgPool, data: NewAnnouncement<'_>) -> Result<Announcement, DbError> {
    let now = OffsetDateTime::now_utc();
    let published = data.scheduled_at.is_none_or(|at| at <= now);
    let published_at = published.then_some(now);
    let row = sqlx::query_as!(
        Announcement,
        r#"
        INSERT INTO announcements
            (id, text, published, all_day, scheduled_at, starts_at, ends_at,
             published_at, status_ids)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id, text, published, all_day, scheduled_at, starts_at, ends_at,
                  published_at, notification_sent_at, status_ids, created_at, updated_at
        "#,
        id::next(),
        data.text,
        published,
        data.all_day,
        data.scheduled_at,
        data.starts_at,
        data.ends_at,
        published_at,
        data.status_ids,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Updates editable announcement fields in place; returns `None` for an
/// unknown id.
pub async fn update(
    pool: &PgPool,
    id: i64,
    data: AnnouncementUpdate<'_>,
) -> Result<Option<Announcement>, DbError> {
    let row = sqlx::query_as!(
        Announcement,
        r#"
        UPDATE announcements
        SET text = $2,
            scheduled_at = $3,
            starts_at = $4,
            ends_at = $5,
            all_day = $6,
            status_ids = $7,
            updated_at = now()
        WHERE id = $1
        RETURNING id, text, published, all_day, scheduled_at, starts_at, ends_at,
                  published_at, notification_sent_at, status_ids, created_at, updated_at
        "#,
        id,
        data.text,
        data.scheduled_at,
        data.starts_at,
        data.ends_at,
        data.all_day,
        data.status_ids,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Publishes an announcement now (Mastodon's `publish!`); returns `None` for an
/// unknown id.
pub async fn publish(pool: &PgPool, id: i64) -> Result<Option<Announcement>, DbError> {
    let row = sqlx::query_as!(
        Announcement,
        r#"
        UPDATE announcements
        SET published = TRUE, published_at = now(), scheduled_at = NULL, updated_at = now()
        WHERE id = $1
        RETURNING id, text, published, all_day, scheduled_at, starts_at, ends_at,
                  published_at, notification_sent_at, status_ids, created_at, updated_at
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Unpublishes an announcement (Mastodon's `unpublish!`); returns `None` for an
/// unknown id.
pub async fn unpublish(pool: &PgPool, id: i64) -> Result<Option<Announcement>, DbError> {
    let row = sqlx::query_as!(
        Announcement,
        r#"
        UPDATE announcements
        SET published = FALSE, scheduled_at = NULL, updated_at = now()
        WHERE id = $1
        RETURNING id, text, published, all_day, scheduled_at, starts_at, ends_at,
                  published_at, notification_sent_at, status_ids, created_at, updated_at
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Deletes an announcement (cascading its reactions and mutes). Returns whether
/// a row existed.
pub async fn delete(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query!("DELETE FROM announcements WHERE id = $1", id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// One emoji's reactions on an announcement: the emoji name, its optional local
/// custom-emoji id, how many reacted, and whether the viewer is among them.
#[derive(Debug, Clone)]
pub struct ReactionGroup {
    pub name: String,
    pub custom_emoji_id: Option<i64>,
    pub count: i64,
    pub me: bool,
}

/// Per-announcement reaction groups for a batch, each in first-reacted order
/// (Mastodon groups by `(name, custom_emoji_id)` ordered by `MIN(created_at)`).
/// `me` is whether `viewer` reacted with that emoji; `false` for an anonymous
/// or absent viewer. Announcements with no reactions are absent from the map.
pub async fn reactions_for(
    pool: &PgPool,
    announcement_ids: &[i64],
    viewer: Option<i64>,
) -> Result<HashMap<i64, Vec<ReactionGroup>>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT announcement_id AS "announcement_id!",
               name AS "name!",
               custom_emoji_id,
               count(*) AS "count!",
               COALESCE(bool_or(account_id = $2), FALSE) AS "me!"
        FROM announcement_reactions
        WHERE announcement_id = ANY($1)
        GROUP BY announcement_id, name, custom_emoji_id
        ORDER BY announcement_id, min(created_at), min(id)
        "#,
        announcement_ids,
        viewer,
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<i64, Vec<ReactionGroup>> = HashMap::new();
    for row in rows {
        map.entry(row.announcement_id)
            .or_default()
            .push(ReactionGroup {
                name: row.name,
                custom_emoji_id: row.custom_emoji_id,
                count: row.count,
                me: row.me,
            });
    }
    Ok(map)
}

/// The `(viewer, announcement, name, custom emoji)` reaction rows of a set of
/// viewers — what flips [`ReactionGroup::me`] per recipient when one shared
/// [`reactions_for`] pass (anonymous, `me` all false) serves a whole streaming
/// fan-out.
pub async fn viewer_reactions(
    pool: &PgPool,
    announcement_ids: &[i64],
    viewer_ids: &[i64],
) -> Result<HashSet<(i64, i64, String, Option<i64>)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id, announcement_id, name, custom_emoji_id
           FROM announcement_reactions
           WHERE announcement_id = ANY($1) AND account_id = ANY($2)"#,
        announcement_ids,
        viewer_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.account_id,
                row.announcement_id,
                row.name,
                row.custom_emoji_id,
            )
        })
        .collect())
}

/// How many people reacted with `name` on an announcement — for the
/// `announcement.reaction` streaming payload (0 once the last one is removed).
pub async fn reaction_count(
    pool: &PgPool,
    announcement_id: i64,
    name: &str,
) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM announcement_reactions
           WHERE announcement_id = $1 AND name = $2"#,
        announcement_id,
        name,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Distinct emoji already reacted with on an announcement, ignoring `name` —
/// the count Mastodon's `ReactionValidator` caps at 8 before allowing a *new*
/// emoji.
pub async fn distinct_reaction_names_excluding(
    pool: &PgPool,
    announcement_id: i64,
    name: &str,
) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(DISTINCT name) AS "count!" FROM announcement_reactions
           WHERE announcement_id = $1 AND name <> $2"#,
        announcement_id,
        name,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Records a reaction; idempotent per `(account, announcement, name)`. Returns
/// `true` when it inserted a new row (so callers only broadcast/limit-check on
/// a real change).
pub async fn create_reaction(
    pool: &PgPool,
    account_id: i64,
    announcement_id: i64,
    name: &str,
    custom_emoji_id: Option<i64>,
) -> Result<bool, DbError> {
    let row_id = sqlx::query_scalar!(
        r#"
        INSERT INTO announcement_reactions (id, account_id, announcement_id, name, custom_emoji_id)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (account_id, announcement_id, name) DO NOTHING
        RETURNING id
        "#,
        id::next(),
        account_id,
        announcement_id,
        name,
        custom_emoji_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row_id.is_some())
}

/// Removes one reaction; returns whether it existed.
pub async fn delete_reaction(
    pool: &PgPool,
    account_id: i64,
    announcement_id: i64,
    name: &str,
) -> Result<bool, DbError> {
    let affected = sqlx::query!(
        r#"DELETE FROM announcement_reactions
           WHERE account_id = $1 AND announcement_id = $2 AND name = $3"#,
        account_id,
        announcement_id,
        name,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

pub async fn delete_reaction_by_emoji_id(
    pool: &PgPool,
    account_id: i64,
    announcement_id: i64,
    custom_emoji_id: i64,
) -> Result<bool, DbError> {
    let affected = sqlx::query!(
        r#"DELETE FROM announcement_reactions
           WHERE account_id = $1 AND announcement_id = $2 AND custom_emoji_id = $3"#,
        account_id,
        announcement_id,
        custom_emoji_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Marks an announcement read for an account (Mastodon's `dismiss`); idempotent.
pub async fn mute(pool: &PgPool, account_id: i64, announcement_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO announcement_mutes (id, account_id, announcement_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, announcement_id) DO NOTHING
        "#,
        id::next(),
        account_id,
        announcement_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Which of `announcement_ids` `account_id` has dismissed — backs the entity's
/// per-user `read` flag.
pub async fn muted_announcement_ids(
    pool: &PgPool,
    account_id: i64,
    announcement_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"SELECT announcement_id FROM announcement_mutes
           WHERE account_id = $1 AND announcement_id = ANY($2)"#,
        account_id,
        announcement_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// [`muted_announcement_ids`] across a set of viewers in one query —
/// `(viewer, announcement)` pairs the viewer has dismissed.
pub async fn muted_for_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    announcement_ids: &[i64],
) -> Result<HashSet<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id, announcement_id FROM announcement_mutes
           WHERE account_id = ANY($1) AND announcement_id = ANY($2)"#,
        viewer_ids,
        announcement_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.account_id, row.announcement_id))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn local(pool: &PgPool, username: &str) -> i64 {
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
    async fn create_publishes_unless_scheduled_ahead(pool: PgPool) {
        let now = OffsetDateTime::now_utc();
        let live = create(
            &pool,
            NewAnnouncement {
                text: "Hello",
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(live.published);
        assert!(live.published_at.is_some());

        let future = create(
            &pool,
            NewAnnouncement {
                text: "Later",
                scheduled_at: Some(now + time::Duration::hours(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!future.published);
        assert!(future.published_at.is_none());

        // Only the published one is listed by the public query.
        let published = published_chronological(&pool).await.unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].id, live.id);

        // Publishing clears the schedule and stamps published_at.
        let published_future = publish(&pool, future.id).await.unwrap().unwrap();
        assert!(published_future.published);
        assert!(published_future.scheduled_at.is_none());
        assert_eq!(published_chronological(&pool).await.unwrap().len(), 2);
    }

    #[sqlx::test]
    async fn reactions_group_in_first_seen_order_with_me(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let ann = create(
            &pool,
            NewAnnouncement {
                text: "React!",
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(
            create_reaction(&pool, alice, ann.id, "😀", None)
                .await
                .unwrap()
        );
        // Idempotent for the same (account, announcement, name).
        assert!(
            !create_reaction(&pool, alice, ann.id, "😀", None)
                .await
                .unwrap()
        );
        assert!(
            create_reaction(&pool, bob, ann.id, "😀", None)
                .await
                .unwrap()
        );
        assert!(
            create_reaction(&pool, bob, ann.id, "🎉", None)
                .await
                .unwrap()
        );

        let groups = reactions_for(&pool, &[ann.id], Some(alice)).await.unwrap();
        let groups = &groups[&ann.id];
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "😀");
        assert_eq!(groups[0].count, 2);
        assert!(groups[0].me, "alice reacted with 😀");
        assert_eq!(groups[1].name, "🎉");
        assert_eq!(groups[1].count, 1);
        assert!(!groups[1].me, "alice did not react with 🎉");

        // me is false for an anonymous viewer.
        let anon = reactions_for(&pool, &[ann.id], None).await.unwrap();
        assert!(!anon[&ann.id][0].me);

        assert_eq!(reaction_count(&pool, ann.id, "😀").await.unwrap(), 2);
        assert!(delete_reaction(&pool, alice, ann.id, "😀").await.unwrap());
        assert!(!delete_reaction(&pool, alice, ann.id, "😀").await.unwrap());
        assert_eq!(reaction_count(&pool, ann.id, "😀").await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn distinct_name_limit_and_mutes(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let ann = create(
            &pool,
            NewAnnouncement {
                text: "x",
                ..Default::default()
            },
        )
        .await
        .unwrap();
        create_reaction(&pool, alice, ann.id, "😀", None)
            .await
            .unwrap();
        create_reaction(&pool, alice, ann.id, "🎉", None)
            .await
            .unwrap();

        // Adding 🎉 again counts the other distinct names (just 😀).
        assert_eq!(
            distinct_reaction_names_excluding(&pool, ann.id, "🎉")
                .await
                .unwrap(),
            1
        );

        assert!(
            muted_announcement_ids(&pool, alice, &[ann.id])
                .await
                .unwrap()
                .is_empty()
        );
        mute(&pool, alice, ann.id).await.unwrap();
        mute(&pool, alice, ann.id).await.unwrap(); // idempotent
        assert_eq!(
            muted_announcement_ids(&pool, alice, &[ann.id])
                .await
                .unwrap(),
            vec![ann.id]
        );
    }
}
