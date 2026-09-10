//! Status mentions.

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool};

use crate::DbError;
use crate::account::Account;

/// Links `account_id` to `status_id`. A `silent` mention grants access
/// and delivers but doesn't notify or show in the public `mentions` list; a
/// non-silent one is a normal tag/text mention. A conflict keeps the existing
/// row's flag — a real (non-silent) mention always outranks a later silent one,
/// and the audience pass runs after tags so it never downgrades.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn attach<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
    account_id: i64,
    silent: bool,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO status_mentions (status_id, account_id, silent) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
        status_id,
        account_id,
        silent,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Attaches many mentions to one status in a single statement — the set-based
/// form of [`attach`], so an inbound Note's whole `tag` + `to`/`cc`/`audience`
/// mention set is written in one round trip instead of one per recipient (QC
/// audit #33). Each `(account_id, silent)` pair follows the same
/// `ON CONFLICT DO NOTHING` rule as [`attach`]; the caller deduplicates the
/// input (preferring a non-silent mention) so a real mention is never demoted
/// to silent by a duplicate. Executor-generic so it can run inside the
/// reconciliation transaction alongside [`detach_all`].
pub async fn attach_many<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
    mentions: &[(i64, bool)],
) -> Result<(), DbError> {
    if mentions.is_empty() {
        return Ok(());
    }
    let account_ids: Vec<i64> = mentions.iter().map(|(id, _)| *id).collect();
    let silents: Vec<bool> = mentions.iter().map(|(_, silent)| *silent).collect();
    sqlx::query!(
        "INSERT INTO status_mentions (status_id, account_id, silent)
         SELECT $1, m.account_id, m.silent
         FROM unnest($2::bigint[], $3::boolean[]) AS m(account_id, silent)
         ON CONFLICT DO NOTHING",
        status_id,
        &account_ids,
        &silents,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Whether `account_id` is mentioned in `status_id` (private-post access).
/// Removes all mention links of a status (edits rebuild them), returning
/// the previously-mentioned account ids. Executor-generic so an edit can clear
/// and rebuild the mention set in one transaction.
pub async fn detach_all<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        "DELETE FROM status_mentions WHERE status_id = $1 RETURNING account_id",
        status_id,
    )
    .fetch_all(executor)
    .await?;
    Ok(rows)
}

/// Removes one account's mention link from a status — a group detaching a
/// submission it rejected, keeping every other recipient intact.
pub async fn detach<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
    account_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM status_mentions WHERE status_id = $1 AND account_id = $2",
        status_id,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn exists(pool: &PgPool, status_id: i64, account_id: i64) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT 1 AS "one" FROM status_mentions WHERE status_id = $1 AND account_id = $2"#,
        status_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Mentioned accounts per status, for a batch of statuses. `active_only`
/// excludes silent mentions — the REST entity's `mentions` list shows
/// active ones only (Mastodon's `active_mentions`), while delivery, DM access
/// and conversation membership pass `false` to include silent recipients too.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn for_statuses<'e, E: PgExecutor<'e>>(
    executor: E,
    status_ids: &[i64],
    active_only: bool,
) -> Result<HashMap<i64, Vec<Account>>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT sm.status_id AS "status_id!",
               a.id, a.username, a.domain, a.display_name, a.note,
               a.public_key, a.created_at, a.updated_at, a.uri, a.inbox_url,
               a.shared_inbox_url, a.public_key_id, a.avatar_file_name,
               a.header_file_name, a.avatar_remote_url, a.header_remote_url,
               account_fields_json(a.id) AS "fields!", a.note_source, a.locked, a.also_known_as, a.moved_to_uri,
               a.url, a.discoverable, a.feature_approval_policy,
               a.is_bot, a.indexable, a.hide_collections,
               a.avatar_description, a.header_description,
               a.suspended_at, a.silenced_at, a.sensitized_at, a.suspension_origin,
               a.show_media, a.show_media_replies, a.show_featured, a.memorial,
               a.actor_type
        FROM status_mentions sm
        JOIN accounts a ON a.id = sm.account_id
        WHERE sm.status_id = ANY($1)
          AND (NOT $2 OR NOT sm.silent)
        ORDER BY a.id
        "#,
        status_ids,
        active_only,
    )
    .fetch_all(executor)
    .await?;
    let mut map: HashMap<i64, Vec<Account>> = HashMap::new();
    for row in rows {
        map.entry(row.status_id).or_default().push(Account {
            id: row.id,
            username: row.username,
            domain: row.domain,
            display_name: row.display_name,
            note: row.note,
            public_key: row.public_key,
            created_at: row.created_at,
            updated_at: row.updated_at,
            uri: row.uri,
            inbox_url: row.inbox_url,
            shared_inbox_url: row.shared_inbox_url,
            public_key_id: row.public_key_id,
            avatar_file_name: row.avatar_file_name,
            header_file_name: row.header_file_name,
            avatar_remote_url: row.avatar_remote_url,
            header_remote_url: row.header_remote_url,
            fields: row.fields,
            note_source: row.note_source,
            locked: row.locked,
            also_known_as: row.also_known_as,
            moved_to_uri: row.moved_to_uri,
            url: row.url,
            discoverable: row.discoverable,
            feature_approval_policy: row.feature_approval_policy,
            is_bot: row.is_bot,
            indexable: row.indexable,
            hide_collections: row.hide_collections,
            avatar_description: row.avatar_description,
            header_description: row.header_description,
            suspended_at: row.suspended_at,
            silenced_at: row.silenced_at,
            sensitized_at: row.sensitized_at,
            suspension_origin: row.suspension_origin,
            show_media: row.show_media,
            show_media_replies: row.show_media_replies,
            show_featured: row.show_featured,
            memorial: row.memorial,
            actor_type: row.actor_type,
        });
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status;

    #[sqlx::test]
    async fn mentions_attach_and_batch_load(pool: PgPool) {
        let alice = account::create_local(
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
        let carol = account::create_local(
            &pool,
            NewLocalAccount {
                username: "carol",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();

        attach(&pool, post.id, carol.id, false).await.unwrap();
        attach(&pool, post.id, carol.id, false).await.unwrap(); // idempotent
        assert!(exists(&pool, post.id, carol.id).await.unwrap());
        assert!(!exists(&pool, post.id, alice.id).await.unwrap());

        let map = for_statuses(&pool, &[post.id], false).await.unwrap();
        assert_eq!(map[&post.id].len(), 1);
        assert_eq!(map[&post.id][0].username, "carol");

        // A silent mention grants access but is hidden from the active list.
        let dave = crate::account::create_local(
            &pool,
            crate::account::NewLocalAccount {
                username: "dave",
                display_name: "",
                note: "",
                public_key_pem: "k",
            },
        )
        .await
        .unwrap();
        attach(&pool, post.id, dave.id, true).await.unwrap();
        assert!(exists(&pool, post.id, dave.id).await.unwrap());
        // All mentions include the silent one; active-only excludes it.
        assert_eq!(
            for_statuses(&pool, &[post.id], false).await.unwrap()[&post.id].len(),
            2
        );
        assert_eq!(
            for_statuses(&pool, &[post.id], true).await.unwrap()[&post.id].len(),
            1
        );
    }

    #[sqlx::test]
    async fn attach_many_is_set_based_and_conflict_safe(pool: PgPool) {
        let mut ids = Vec::new();
        for name in ["alice", "bob", "carol"] {
            ids.push(
                account::create_local(
                    &pool,
                    NewLocalAccount {
                        username: name,
                        display_name: "",
                        note: "",
                        public_key_pem: "pub",
                    },
                )
                .await
                .unwrap()
                .id,
            );
        }
        let (alice, bob, carol) = (ids[0], ids[1], ids[2]);
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();

        // A single call writes the whole set. Bob is a real (non-silent)
        // mention; carol is silent.
        attach_many(&pool, post.id, &[(bob, false), (carol, true)])
            .await
            .unwrap();
        let all = for_statuses(&pool, &[post.id], false).await.unwrap();
        assert_eq!(all[&post.id].len(), 2);
        // Only bob is active (non-silent).
        let active = for_statuses(&pool, &[post.id], true).await.unwrap();
        assert_eq!(active[&post.id].len(), 1);
        assert_eq!(active[&post.id][0].id, bob);

        // Re-attaching (including a would-be silent downgrade of bob) is a no-op:
        // ON CONFLICT keeps bob non-silent and adds nothing new.
        attach_many(&pool, post.id, &[(bob, true), (carol, true)])
            .await
            .unwrap();
        assert_eq!(
            for_statuses(&pool, &[post.id], true).await.unwrap()[&post.id].len(),
            1,
            "bob stays active after a silent re-attach"
        );
        assert_eq!(
            for_statuses(&pool, &[post.id], false).await.unwrap()[&post.id].len(),
            2,
            "no duplicate rows"
        );

        // An empty set is a no-op.
        attach_many(&pool, post.id, &[]).await.unwrap();
    }
}
