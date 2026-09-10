//! Timeline read positions (Mastodon's markers): one row per
//! (user, timeline) recording the newest id the user has read.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// The timelines Mastodon accepts markers for.
pub const TIMELINES: [&str; 2] = ["home", "notifications"];

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Marker {
    pub user_id: i64,
    pub timeline: String,
    pub last_read_id: i64,
    /// Mastodon's `lock_version`: bumped on every change, exposed to clients
    /// so they can detect concurrent updates.
    pub version: i32,
    pub updated_at: OffsetDateTime,
}

/// A user's markers for the given timelines (unknown names simply match
/// nothing).
pub async fn list(
    pool: &PgPool,
    user_id: i64,
    timelines: &[String],
) -> Result<Vec<Marker>, DbError> {
    let markers = sqlx::query_as!(
        Marker,
        r#"
        SELECT user_id, timeline, last_read_id, version, updated_at
        FROM markers
        WHERE user_id = $1 AND timeline = ANY($2)
        "#,
        user_id,
        timelines,
    )
    .fetch_all(pool)
    .await?;
    Ok(markers)
}

pub async fn find(pool: &PgPool, user_id: i64, timeline: &str) -> Result<Option<Marker>, DbError> {
    let marker = sqlx::query_as!(
        Marker,
        r#"
        SELECT user_id, timeline, last_read_id, version, updated_at
        FROM markers
        WHERE user_id = $1 AND timeline = $2
        "#,
        user_id,
        timeline,
    )
    .fetch_optional(pool)
    .await?;
    Ok(marker)
}

/// Saves a marker the way Mastodon's `find_or_create_by` + `update!` does:
/// a fresh marker lands at version 1, a changed `last_read_id` bumps the
/// version, and re-submitting the current value touches nothing. With no
/// `last_read_id` the row is only ensured to exist (id 0, version 0).
pub async fn upsert(
    pool: &PgPool,
    user_id: i64,
    timeline: &str,
    last_read_id: Option<i64>,
) -> Result<Marker, DbError> {
    let Some(last_read_id) = last_read_id else {
        sqlx::query!(
            "INSERT INTO markers (user_id, timeline) VALUES ($1, $2)
             ON CONFLICT (user_id, timeline) DO NOTHING",
            user_id,
            timeline,
        )
        .execute(pool)
        .await?;
        return Ok(find(pool, user_id, timeline)
            .await?
            .expect("marker row just ensured"));
    };
    let updated = sqlx::query_as!(
        Marker,
        r#"
        INSERT INTO markers (user_id, timeline, last_read_id, version)
        VALUES ($1, $2, $3, 1)
        ON CONFLICT (user_id, timeline) DO UPDATE
        SET last_read_id = EXCLUDED.last_read_id,
            version = markers.version + 1,
            updated_at = now()
        WHERE markers.last_read_id IS DISTINCT FROM EXCLUDED.last_read_id
        RETURNING user_id, timeline, last_read_id, version, updated_at
        "#,
        user_id,
        timeline,
        last_read_id,
    )
    .fetch_optional(pool)
    .await?;
    match updated {
        Some(marker) => Ok(marker),
        // The no-change case: the conditional upsert touched nothing.
        None => Ok(find(pool, user_id, timeline)
            .await?
            .expect("marker row exists when the upsert is a no-op")),
    }
}

/// Advance-only save: moves the marker forward when `last_read_id` is newer
/// than the stored position and touches nothing otherwise. Used by the web
/// notifications page, which reads a whole page at once and must never rewind
/// a position an API client already pushed further.
pub async fn advance(
    pool: &PgPool,
    user_id: i64,
    timeline: &str,
    last_read_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO markers (user_id, timeline, last_read_id, version)
        VALUES ($1, $2, $3, 1)
        ON CONFLICT (user_id, timeline) DO UPDATE
        SET last_read_id = EXCLUDED.last_read_id,
            version = markers.version + 1,
            updated_at = now()
        WHERE markers.last_read_id < EXCLUDED.last_read_id
        "#,
        user_id,
        timeline,
        last_read_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::user;

    async fn local_user(pool: &PgPool, username: &str) -> i64 {
        let account = account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        user::create(
            pool,
            account.id,
            Some(&format!("{username}@example.test")),
            "hash",
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn upsert_versions_like_mastodon(pool: PgPool) {
        let user_id = local_user(&pool, "alice").await;

        // Fresh marker: created then updated, so version 1.
        let marker = upsert(&pool, user_id, "home", Some(10)).await.unwrap();
        assert_eq!(marker.last_read_id, 10);
        assert_eq!(marker.version, 1);

        // A new value bumps the version.
        let marker = upsert(&pool, user_id, "home", Some(20)).await.unwrap();
        assert_eq!(marker.last_read_id, 20);
        assert_eq!(marker.version, 2);
        let first_updated_at = marker.updated_at;

        // Re-submitting the same value changes nothing.
        let marker = upsert(&pool, user_id, "home", Some(20)).await.unwrap();
        assert_eq!(marker.version, 2);
        assert_eq!(marker.updated_at, first_updated_at);

        // No last_read_id only ensures the row.
        let marker = upsert(&pool, user_id, "notifications", None).await.unwrap();
        assert_eq!(marker.last_read_id, 0);
        assert_eq!(marker.version, 0);
        let marker = upsert(&pool, user_id, "home", None).await.unwrap();
        assert_eq!(marker.last_read_id, 20);
        assert_eq!(marker.version, 2);
    }

    #[sqlx::test]
    async fn advance_only_moves_forward(pool: PgPool) {
        let user_id = local_user(&pool, "alice").await;

        // No marker yet: advance creates one.
        advance(&pool, user_id, "notifications", 10).await.unwrap();
        let marker = find(&pool, user_id, "notifications")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(marker.last_read_id, 10);
        assert_eq!(marker.version, 1);

        // A newer id moves it and bumps the version, like an upsert would.
        advance(&pool, user_id, "notifications", 20).await.unwrap();
        let marker = find(&pool, user_id, "notifications")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(marker.last_read_id, 20);
        assert_eq!(marker.version, 2);

        // An older or equal id changes nothing — no rewind, no version bump.
        advance(&pool, user_id, "notifications", 5).await.unwrap();
        advance(&pool, user_id, "notifications", 20).await.unwrap();
        let marker = find(&pool, user_id, "notifications")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(marker.last_read_id, 20);
        assert_eq!(marker.version, 2);
    }

    #[sqlx::test]
    async fn list_and_find_scope_to_user_and_timeline(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let carol = local_user(&pool, "carol").await;
        upsert(&pool, alice, "home", Some(5)).await.unwrap();
        upsert(&pool, alice, "notifications", Some(7))
            .await
            .unwrap();
        upsert(&pool, carol, "home", Some(9)).await.unwrap();

        let both = list(
            &pool,
            alice,
            &["home".to_owned(), "notifications".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(both.len(), 2);

        let home_only = list(&pool, alice, &["home".to_owned()]).await.unwrap();
        assert_eq!(home_only.len(), 1);
        assert_eq!(home_only[0].last_read_id, 5);

        assert!(
            list(&pool, alice, &["bogus".to_owned()])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(list(&pool, alice, &[]).await.unwrap().is_empty());

        let found = find(&pool, carol, "home").await.unwrap().unwrap();
        assert_eq!(found.last_read_id, 9);
        assert!(find(&pool, carol, "notifications").await.unwrap().is_none());
    }
}
