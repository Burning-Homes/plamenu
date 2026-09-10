//! Community facts mirrored from a *remote* Group actor: Lemmy's `sensitive`
//! and `postingRestrictedToMods` flags, and the FEP-1b12 `attributedTo`
//! moderator roster.
//!
//! Deliberately separate from [`crate::group`], which is the sidecar of a group
//! we host — `group::find` returning `Some` is the "our policy, our
//! enforcement" test throughout the server. Nothing here is authoritative: the
//! rows are the origin's claims, refreshed on every actor refresh, and being
//! listed as a moderator of a remote community grants no local power.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;
use crate::group::PostingPolicy;

/// The mirrored `remote_groups` row of a remote Group account.
#[derive(Debug, Clone)]
pub struct RemoteGroup {
    pub account_id: i64,
    pub sensitive: bool,
    /// Who the origin says may start threads, as the same tri-state a hosted
    /// group uses. Read through [`Self::posting_policy`].
    pub posting_policy: String,
    /// The moderator collection to dereference, empty when the actor
    /// advertises none.
    pub moderators_uri: String,
    pub updated_at: OffsetDateTime,
}

impl RemoteGroup {
    #[must_use]
    pub fn posting_policy(&self) -> PostingPolicy {
        PostingPolicy::parse(&self.posting_policy)
    }
}

/// The community flags carried by one actor document.
#[derive(Debug, Clone, Copy)]
pub struct GroupFacts<'a> {
    pub sensitive: bool,
    pub posting_policy: &'a str,
    pub moderators_uri: &'a str,
}

impl Default for GroupFacts<'_> {
    /// What an actor that states nothing means: a safe-for-work community
    /// anyone may post to, publishing no moderators.
    fn default() -> Self {
        Self {
            sensitive: false,
            posting_policy: PostingPolicy::Anyone.as_str(),
            moderators_uri: "",
        }
    }
}

/// Records what a remote Group actor says about itself. Idempotent per account:
/// every actor refresh overwrites the row, so a community that turns its NSFW
/// or mods-only flag back off stops being marked here on the next refresh.
pub async fn upsert(pool: &PgPool, account_id: i64, facts: GroupFacts<'_>) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO remote_groups
            (account_id, sensitive, posting_policy, moderators_uri)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id) DO UPDATE SET
            sensitive = excluded.sensitive,
            posting_policy = excluded.posting_policy,
            moderators_uri = excluded.moderators_uri,
            updated_at = now()
        "#,
        account_id,
        facts.sensitive,
        facts.posting_policy,
        facts.moderators_uri,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The mirrored facts of `account_id`, `None` for an account we never saw a
/// Group document for.
pub async fn find(pool: &PgPool, account_id: i64) -> Result<Option<RemoteGroup>, DbError> {
    let row = sqlx::query_as!(
        RemoteGroup,
        r#"
        SELECT account_id, sensitive, posting_policy, moderators_uri, updated_at
        FROM remote_groups
        WHERE account_id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Replaces a remote community's mirrored moderator roster with `account_ids`,
/// in collection order. The collection is authoritative for its own community,
/// so anyone it no longer lists is dropped; an empty list clears the roster.
pub async fn set_moderators(
    pool: &PgPool,
    group_account_id: i64,
    account_ids: &[i64],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "DELETE FROM remote_group_moderators
         WHERE group_account_id = $1 AND NOT (account_id = ANY($2))",
        group_account_id,
        account_ids,
    )
    .execute(&mut *tx)
    .await?;
    for (index, account_id) in account_ids.iter().enumerate() {
        let ordinal = i32::try_from(index).unwrap_or(i32::MAX);
        sqlx::query!(
            r#"
            INSERT INTO remote_group_moderators (group_account_id, account_id, ordinal)
            VALUES ($1, $2, $3)
            ON CONFLICT (group_account_id, account_id) DO UPDATE SET ordinal = excluded.ordinal
            "#,
            group_account_id,
            account_id,
            ordinal,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The mirrored moderators of `group_account_id`, in the order the origin
/// listed them.
pub async fn moderator_ids(pool: &PgPool, group_account_id: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"SELECT account_id AS "account_id!" FROM remote_group_moderators
           WHERE group_account_id = $1
           ORDER BY ordinal, account_id"#,
        group_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Whether the origin lists `account_id` among `group_account_id`'s moderators.
pub async fn is_moderator(
    pool: &PgPool,
    group_account_id: i64,
    account_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT 1 AS "one!" FROM remote_group_moderators
           WHERE group_account_id = $1 AND account_id = $2"#,
        group_account_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, RemoteAccountData, upsert_remote};

    async fn remote(pool: &PgPool, username: &str, kind: &str) -> i64 {
        upsert_remote(
            pool,
            RemoteAccountData {
                username,
                domain: "remote.test",
                uri: &format!("https://remote.test/{username}"),
                display_name: "",
                note: "",
                inbox_url: &format!("https://remote.test/{username}/inbox"),
                shared_inbox_url: "https://remote.test/inbox",
                public_key_pem: "pub",
                public_key_id: &format!("https://remote.test/{username}#main-key"),
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
                actor_type: Some(kind),
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn facts_round_trip(pool: PgPool) {
        let group = remote(&pool, "community", "Group").await;
        assert!(find(&pool, group).await.unwrap().is_none());

        upsert(
            &pool,
            group,
            GroupFacts {
                sensitive: true,
                posting_policy: "mods",
                moderators_uri: "https://remote.test/c/community/moderators",
            },
        )
        .await
        .unwrap();
        let stored = find(&pool, group).await.unwrap().unwrap();
        assert!(stored.sensitive);
        assert_eq!(stored.posting_policy(), PostingPolicy::Mods);
        assert_eq!(
            stored.moderators_uri,
            "https://remote.test/c/community/moderators"
        );

        // A refresh that no longer advertises the flags clears them.
        upsert(&pool, group, GroupFacts::default()).await.unwrap();
        let stored = find(&pool, group).await.unwrap().unwrap();
        assert!(!stored.sensitive);
        assert_eq!(stored.posting_policy(), PostingPolicy::Anyone);
        assert!(stored.moderators_uri.is_empty());
    }

    #[sqlx::test]
    async fn roster_is_replaced_in_collection_order(pool: PgPool) {
        let group = remote(&pool, "community", "Group").await;
        let alice = remote(&pool, "alice", "Person").await;
        let bob = remote(&pool, "bob", "Person").await;
        let carol = remote(&pool, "carol", "Person").await;

        set_moderators(&pool, group, &[bob, alice]).await.unwrap();
        assert_eq!(moderator_ids(&pool, group).await.unwrap(), vec![bob, alice]);
        assert!(is_moderator(&pool, group, alice).await.unwrap());
        assert!(!is_moderator(&pool, group, carol).await.unwrap());

        // The collection is authoritative: a dropped moderator goes away, and
        // a kept one takes its new position.
        set_moderators(&pool, group, &[carol, alice]).await.unwrap();
        assert_eq!(
            moderator_ids(&pool, group).await.unwrap(),
            vec![carol, alice]
        );
        assert!(!is_moderator(&pool, group, bob).await.unwrap());

        set_moderators(&pool, group, &[]).await.unwrap();
        assert!(moderator_ids(&pool, group).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn deleting_an_account_clears_its_rows(pool: PgPool) {
        let group = remote(&pool, "community", "Group").await;
        let alice = remote(&pool, "alice", "Person").await;
        upsert(&pool, group, GroupFacts::default()).await.unwrap();
        set_moderators(&pool, group, &[alice]).await.unwrap();

        account::delete_by_id(&pool, alice).await.unwrap();
        assert!(moderator_ids(&pool, group).await.unwrap().is_empty());

        account::delete_by_id(&pool, group).await.unwrap();
        assert!(find(&pool, group).await.unwrap().is_none());
    }
}
