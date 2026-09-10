//! Emoji reactions (Pleroma's litepub `EmojiReact`). One row per
//! `(account, status, name)`; reactions are aggregated per emoji at render
//! time into the status' `pleroma.emoji_reactions`. Only inbound (remote)
//! reactions are stored for now — the local-react API is the other half.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::{DbError, Upserted, id};

/// A reaction about to be stored. `name` is the bare emoji (Unicode) or
/// custom-emoji shortcode without colons; `custom_emoji_url` is the federated
/// image for a custom-emoji reaction, `None` for a Unicode one.
#[derive(Debug)]
pub struct NewReaction<'a> {
    pub account_id: i64,
    pub status_id: i64,
    pub name: &'a str,
    pub custom_emoji_url: Option<&'a str>,
    /// The `EmojiReact` activity id, for later `Undo` matching.
    pub uri: Option<&'a str>,
}

/// Records a reaction; idempotent per `(account, status, name)`. A repeat
/// reaction just refreshes the stored image/uri; the result says whether this
/// call inserted the row, so a redelivered `EmojiReact` doesn't re-notify.
pub async fn create<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    data: NewReaction<'_>,
) -> Result<Upserted, DbError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO status_reactions (id, account_id, status_id, name, custom_emoji_url, uri)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (account_id, status_id, name) WHERE custom_emoji_origin_id IS NULL
        DO UPDATE SET custom_emoji_url = EXCLUDED.custom_emoji_url, uri = EXCLUDED.uri
        RETURNING id, (xmax = 0) AS "inserted!"
        "#,
        id::next(),
        data.account_id,
        data.status_id,
        data.name,
        data.custom_emoji_url,
        data.uri,
    )
    .fetch_one(pool)
    .await?;
    Ok(Upserted {
        id: row.id,
        inserted: row.inserted,
    })
}

/// Records a custom-emoji reaction using its stable origin identity.
pub async fn create_custom<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    data: NewReaction<'_>,
    custom_emoji_id: i64,
    origin_id: i64,
) -> Result<Upserted, DbError> {
    let row: (i64, bool) = sqlx::query_as(
        r"INSERT INTO status_reactions
             (id, account_id, status_id, name, custom_emoji_url, uri,
              custom_emoji_id, custom_emoji_origin_id)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           ON CONFLICT (account_id, status_id, custom_emoji_origin_id)
             WHERE custom_emoji_origin_id IS NOT NULL
           DO UPDATE SET name = EXCLUDED.name,
                         custom_emoji_url = EXCLUDED.custom_emoji_url,
                         custom_emoji_id = EXCLUDED.custom_emoji_id,
                         uri = EXCLUDED.uri
           RETURNING id, (xmax = 0)",
    )
    .bind(id::next())
    .bind(data.account_id)
    .bind(data.status_id)
    .bind(data.name)
    .bind(data.custom_emoji_url)
    .bind(data.uri)
    .bind(custom_emoji_id)
    .bind(origin_id)
    .fetch_one(pool)
    .await?;
    Ok(Upserted {
        id: row.0,
        inserted: row.1,
    })
}

/// Removes one reaction; returns its row id if it existed.
pub async fn delete(
    pool: &PgPool,
    account_id: i64,
    status_id: i64,
    name: &str,
) -> Result<Option<i64>, DbError> {
    let row_id = sqlx::query_scalar!(
        "DELETE FROM status_reactions
         WHERE account_id = $1 AND status_id = $2 AND name = $3 RETURNING id",
        account_id,
        status_id,
        name,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row_id)
}

/// What a removed reaction carried — enough to clear its notification and
/// re-render the status.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RemovedReaction {
    pub row_id: i64,
    pub status_id: i64,
    pub name: String,
    pub custom_emoji_url: Option<String>,
}

/// Removes a reaction by its `EmojiReact` activity uri — the reliable
/// `Undo(EmojiReact)` path.
pub async fn delete_by_uri(
    pool: &PgPool,
    account_id: i64,
    uri: &str,
) -> Result<Option<RemovedReaction>, DbError> {
    let row = sqlx::query_as::<_, RemovedReaction>(
        r"DELETE FROM status_reactions
           WHERE account_id = $1 AND uri = $2
           RETURNING id AS row_id, status_id, name, custom_emoji_url",
    )
    .bind(account_id)
    .bind(uri)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Removes a reaction by its `(account, status, name)` — the fallback
/// `Undo(EmojiReact)` path when the activity carries no reusable id. Returns
/// the custom-emoji image it carried (so the notification can be cleared).
pub async fn delete_returning<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
    name: &str,
) -> Result<Option<RemovedReaction>, DbError> {
    let row = sqlx::query_as::<_, RemovedReaction>(
        r"DELETE FROM status_reactions
           WHERE account_id = $1 AND status_id = $2 AND name = $3
           RETURNING id AS row_id, status_id, name, custom_emoji_url",
    )
    .bind(account_id)
    .bind(status_id)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn delete_returning_by_origin<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
    origin_id: i64,
) -> Result<Option<RemovedReaction>, DbError> {
    let row = sqlx::query_as::<_, RemovedReaction>(
        r"DELETE FROM status_reactions
           WHERE account_id = $1 AND status_id = $2 AND custom_emoji_origin_id = $3
           RETURNING id AS row_id, status_id, name, custom_emoji_url",
    )
    .bind(account_id)
    .bind(status_id)
    .bind(origin_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Removes every reaction `account_id` has on `status_id` — how a Misskey
/// `Dislike` withdraws: their model has one reaction per user per note, so
/// the retraction names no emoji.
pub async fn delete_all_for(
    pool: &PgPool,
    account_id: i64,
    status_id: i64,
) -> Result<Vec<RemovedReaction>, DbError> {
    let rows = sqlx::query_as::<_, RemovedReaction>(
        r"DELETE FROM status_reactions
           WHERE account_id = $1 AND status_id = $2
           RETURNING id AS row_id, status_id, name, custom_emoji_url",
    )
    .bind(account_id)
    .bind(status_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One emoji's reactions on a status: the emoji, how many reacted, the
/// optional custom-emoji image, and the reactor account ids (oldest first).
#[derive(Debug, Clone)]
pub struct ReactionGroup {
    pub name: String,
    pub custom_emoji_url: Option<String>,
    pub count: i64,
    pub account_ids: Vec<i64>,
    pub custom_emoji_id: Option<i64>,
    pub custom_emoji_origin_id: Option<i64>,
}

/// Per-status reaction groups for a batch of statuses, each status' groups in
/// first-seen order (the order the emoji were first reacted with), like
/// Pleroma renders its reaction chips. Statuses with no reactions are absent.
pub async fn for_statuses(
    pool: &PgPool,
    status_ids: &[i64],
) -> Result<HashMap<i64, Vec<ReactionGroup>>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT status_id AS "status_id!",
               name AS "name!",
               max(custom_emoji_url) AS custom_emoji_url,
               max(custom_emoji_id) AS custom_emoji_id,
               custom_emoji_origin_id,
               count(*) AS "count!",
               array_agg(account_id ORDER BY id) AS "account_ids!"
        FROM status_reactions
        WHERE status_id = ANY($1)
        GROUP BY status_id, name, custom_emoji_origin_id
        ORDER BY status_id, min(id)
        "#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<i64, Vec<ReactionGroup>> = HashMap::new();
    for row in rows {
        map.entry(row.status_id).or_default().push(ReactionGroup {
            name: row.name,
            custom_emoji_url: row.custom_emoji_url,
            count: row.count,
            account_ids: row.account_ids,
            custom_emoji_id: row.custom_emoji_id,
            custom_emoji_origin_id: row.custom_emoji_origin_id,
        });
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status;

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
    async fn reactions_aggregate_in_first_seen_order(pool: PgPool) {
        let author = local(&pool, "author").await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(author, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();

        // 😀 first (alice, then bob), then a custom :blobcat: from alice.
        create(
            &pool,
            NewReaction {
                account_id: alice,
                status_id: post.id,
                name: "😀",
                custom_emoji_url: None,
                uri: Some("https://r/1"),
            },
        )
        .await
        .unwrap();
        create(
            &pool,
            NewReaction {
                account_id: bob,
                status_id: post.id,
                name: "😀",
                custom_emoji_url: None,
                uri: Some("https://r/2"),
            },
        )
        .await
        .unwrap();
        create(
            &pool,
            NewReaction {
                account_id: alice,
                status_id: post.id,
                name: "blobcat",
                custom_emoji_url: Some("https://r/blobcat.png"),
                uri: Some("https://r/3"),
            },
        )
        .await
        .unwrap();

        let groups = for_statuses(&pool, &[post.id]).await.unwrap();
        let reactions = &groups[&post.id];
        assert_eq!(reactions.len(), 2);
        assert_eq!(reactions[0].name, "😀");
        assert_eq!(reactions[0].count, 2);
        assert_eq!(reactions[0].account_ids, [alice, bob]);
        assert!(reactions[0].custom_emoji_url.is_none());
        assert_eq!(reactions[1].name, "blobcat");
        assert_eq!(reactions[1].count, 1);
        assert_eq!(
            reactions[1].custom_emoji_url.as_deref(),
            Some("https://r/blobcat.png")
        );
    }

    #[sqlx::test]
    async fn same_shortcode_different_personal_origins_do_not_merge(pool: PgPool) {
        let author = local(&pool, "origin_author").await;
        let alice = local(&pool, "origin_alice").await;
        let bob = local(&pool, "origin_bob").await;
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(author, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();
        let crate::custom_emoji::PersonalCreateOutcome::Created(a_id) =
            crate::custom_emoji::create_personal_upload(
                &pool,
                alice,
                "same",
                "a.png",
                "image/png",
                1,
                None,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        let crate::custom_emoji::PersonalCreateOutcome::Created(b_id) =
            crate::custom_emoji::create_personal_upload(
                &pool,
                bob,
                "same",
                "b.png",
                "image/png",
                1,
                None,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        let a = crate::custom_emoji::find_managed_by_id(&pool, a_id)
            .await
            .unwrap()
            .unwrap();
        let b = crate::custom_emoji::find_managed_by_id(&pool, b_id)
            .await
            .unwrap()
            .unwrap();
        create_custom(
            &pool,
            NewReaction {
                account_id: alice,
                status_id: post.id,
                name: "same",
                custom_emoji_url: Some("https://local/a.png"),
                uri: None,
            },
            a.id,
            a.origin_id,
        )
        .await
        .unwrap();
        create_custom(
            &pool,
            NewReaction {
                account_id: bob,
                status_id: post.id,
                name: "same",
                custom_emoji_url: Some("https://local/b.png"),
                uri: None,
            },
            b.id,
            b.origin_id,
        )
        .await
        .unwrap();
        let groups = for_statuses(&pool, &[post.id]).await.unwrap();
        assert_eq!(groups[&post.id].len(), 2);
        assert_ne!(
            groups[&post.id][0].custom_emoji_origin_id,
            groups[&post.id][1].custom_emoji_origin_id
        );
    }

    #[sqlx::test]
    async fn reaction_is_idempotent_and_removable(pool: PgPool) {
        let author = local(&pool, "author").await;
        let alice = local(&pool, "alice").await;
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(author, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();

        let first = create(
            &pool,
            NewReaction {
                account_id: alice,
                status_id: post.id,
                name: "😀",
                custom_emoji_url: None,
                uri: Some("https://r/1"),
            },
        )
        .await
        .unwrap();
        let second = create(
            &pool,
            NewReaction {
                account_id: alice,
                status_id: post.id,
                name: "😀",
                custom_emoji_url: None,
                uri: Some("https://r/1b"),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            first.id, second.id,
            "idempotent per (account, status, name)"
        );
        assert!(first.inserted);
        assert!(!second.inserted, "repeat is flagged as such");

        // Undo by uri returns what it removed.
        let removed = delete_by_uri(&pool, alice, "https://r/1b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(removed.status_id, post.id);
        assert_eq!(removed.name, "😀");
        assert!(removed.custom_emoji_url.is_none());
        assert!(for_statuses(&pool, &[post.id]).await.unwrap().is_empty());
        assert!(delete(&pool, alice, post.id, "😀").await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn delete_all_for_sweeps_only_the_named_pair(pool: PgPool) {
        let author = local(&pool, "author").await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(author, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();

        for (who, name, url) in [
            (alice, "😀", None),
            (alice, "blobcat", Some("https://r.example/blobcat.png")),
            (bob, "😀", None),
        ] {
            create(
                &pool,
                NewReaction {
                    account_id: who,
                    status_id: post.id,
                    name,
                    custom_emoji_url: url,
                    uri: None,
                },
            )
            .await
            .unwrap();
        }

        let removed = delete_all_for(&pool, alice, post.id).await.unwrap();
        let mut names: Vec<&str> = removed.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["blobcat", "😀"]);

        // bob's reaction survives; a repeat sweep removes nothing.
        let groups = for_statuses(&pool, &[post.id]).await.unwrap();
        assert_eq!(groups[&post.id].len(), 1);
        assert_eq!(groups[&post.id][0].account_ids, [bob]);
        assert!(
            delete_all_for(&pool, alice, post.id)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
