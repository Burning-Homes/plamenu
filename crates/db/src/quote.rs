//! Quote relationships (FEP-044f consent handshake state).

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::DbError;
#[cfg(test)]
use crate::id;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Quote {
    pub id: i64,
    pub status_id: Option<i64>,
    pub status_uri: String,
    pub account_id: i64,
    pub quoted_status_id: Option<i64>,
    pub quoted_account_id: Option<i64>,
    /// `pending` | `accepted` | `rejected` | `revoked`.
    pub state: String,
    pub activity_uri: Option<String>,
    pub approval_uri: Option<String>,
    /// The quoted post's URI, kept even while that post is unresolved so a
    /// later Update or verification retry can still fetch it.
    pub quoted_uri: Option<String>,
    /// Arrived without the FEP-044f `quote` property (Misskey-style): no
    /// consent handshake exists, so a non-accepted legacy quote is omitted
    /// from rendering (Mastodon likewise renders them only once accepted).
    pub legacy: bool,
    pub created_at: OffsetDateTime,
}

const COLS: &str = "id, status_id, status_uri, account_id, quoted_status_id, \
                    quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at";
const _: &str = COLS;

#[derive(Debug)]
pub struct NewQuote<'a> {
    /// Pre-generated row id (authorization URLs embed it).
    pub quote_id: i64,
    pub status_id: Option<i64>,
    pub status_uri: &'a str,
    pub account_id: i64,
    pub quoted_status_id: Option<i64>,
    pub quoted_account_id: Option<i64>,
    pub state: &'a str,
    pub activity_uri: Option<&'a str>,
    pub approval_uri: Option<&'a str>,
    pub quoted_uri: Option<&'a str>,
    pub legacy: bool,
}

/// Inserts a quote row; idempotent on the quoting post's URI (one quote per
/// post). Redelivery keeps established state, but a pending row first seen as
/// an inbound `Create` can later be upgraded by the authoritative `QuoteRequest`
/// or authorization stamp.
pub async fn create(pool: &PgPool, new: NewQuote<'_>) -> Result<Quote, DbError> {
    let quote = crate::upsert_racing(|| insert_quote(pool, &new)).await?;
    Ok(quote)
}

/// [`create`] within a caller-provided transaction, so a local quote row commits
/// atomically with the status it belongs to and its outbox. A
/// unique-violation retry would poison the surrounding transaction, so this runs
/// the insert exactly once — safe because a brand-new local `status_uri` (a fresh
/// snowflake) cannot race a concurrent insert, unlike the inbound-quote path
/// [`create`] guards.
pub async fn create_in_tx<'e, E: PgExecutor<'e>>(
    executor: E,
    new: NewQuote<'_>,
) -> Result<Quote, DbError> {
    Ok(insert_quote(executor, &new).await?)
}

async fn insert_quote<'e, E: PgExecutor<'e>>(
    executor: E,
    new: &NewQuote<'_>,
) -> Result<Quote, sqlx::Error> {
    sqlx::query_as!(
        Quote,
        r#"
        INSERT INTO quotes (id, status_id, status_uri, account_id, quoted_status_id,
                            quoted_account_id, state, activity_uri, approval_uri, quoted_uri,
                            legacy)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (status_uri) DO UPDATE SET
            status_id = COALESCE(quotes.status_id, EXCLUDED.status_id),
            quoted_status_id = COALESCE(quotes.quoted_status_id, EXCLUDED.quoted_status_id),
            quoted_account_id = COALESCE(quotes.quoted_account_id, EXCLUDED.quoted_account_id),
            activity_uri = COALESCE(quotes.activity_uri, EXCLUDED.activity_uri),
            approval_uri = COALESCE(quotes.approval_uri, EXCLUDED.approval_uri),
            quoted_uri = COALESCE(quotes.quoted_uri, EXCLUDED.quoted_uri),
            legacy = EXCLUDED.legacy,
            state = CASE
                WHEN quotes.state = 'pending'
                 AND EXCLUDED.state <> 'pending'
                 AND (EXCLUDED.activity_uri IS NOT NULL OR EXCLUDED.approval_uri IS NOT NULL)
                THEN EXCLUDED.state
                ELSE quotes.state
            END
        RETURNING id, status_id, status_uri, account_id, quoted_status_id,
                  quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at
        "#,
        new.quote_id,
        new.status_id,
        new.status_uri,
        new.account_id,
        new.quoted_status_id,
        new.quoted_account_id,
        new.state,
        new.activity_uri,
        new.approval_uri,
        new.quoted_uri,
        new.legacy,
    )
    .fetch_one(executor)
    .await
}

/// A local quoter of a status: who quoted it and with which post — the
/// audience of `quoted_update` notifications when the quoted post is edited.
#[derive(Debug, Clone, Copy)]
pub struct LocalQuoter {
    pub account_id: i64,
    /// The quoting (local) status.
    pub status_id: i64,
}

/// Accepted quotes of a status held by local accounts, ascending by quote id.
pub async fn accepted_local_quoters_of(
    pool: &PgPool,
    quoted_status_id: i64,
) -> Result<Vec<LocalQuoter>, DbError> {
    let rows = sqlx::query_as!(
        LocalQuoter,
        r#"
        SELECT q.account_id AS "account_id!", q.status_id AS "status_id!"
        FROM quotes q
        JOIN accounts a ON a.id = q.account_id
        WHERE q.quoted_status_id = $1 AND q.state = 'accepted'
          AND q.status_id IS NOT NULL AND a.domain IS NULL
        ORDER BY q.id
        "#,
        quoted_status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Resolves the state of a quote (Accept/Reject handling). The expected
/// quoted account guards against forged responses.
pub async fn set_state_by_activity_uri(
    pool: &PgPool,
    activity_uri: &str,
    quoted_account_id: i64,
    state: &str,
    approval_uri: Option<&str>,
) -> Result<Option<Quote>, DbError> {
    let quote = sqlx::query_as!(
        Quote,
        r#"
        UPDATE quotes SET state = $3, approval_uri = COALESCE($4, approval_uri)
        WHERE activity_uri = $1 AND quoted_account_id = $2
        RETURNING id, status_id, status_uri, account_id, quoted_status_id,
                  quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at
        "#,
        activity_uri,
        quoted_account_id,
        state,
        approval_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(quote)
}

/// Revokes a previously-accepted quote of a status: the quoted author (the
/// owner of `quoted_account_id`) withdraws consent. Scoped to that account so a
/// caller can only revoke quotes of their own posts. Returns the updated row,
/// or `None` if it was not an accepted quote owned by them.
pub async fn revoke<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    quote_id: i64,
    quoted_account_id: i64,
) -> Result<Option<Quote>, DbError> {
    let quote = sqlx::query_as!(
        Quote,
        r#"
        UPDATE quotes SET state = 'revoked', approval_uri = NULL
        WHERE id = $1 AND quoted_account_id = $2 AND state = 'accepted'
        RETURNING id, status_id, status_uri, account_id, quoted_status_id,
                  quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at
        "#,
        quote_id,
        quoted_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(quote)
}

/// The live (pending or accepted) quote carrying an authorization stamp
/// issued by `quoted_account_id` — whether a forwarded stamp deletion names
/// anything worth confirming at its origin.
pub async fn find_live_by_approval_uri(
    pool: &PgPool,
    approval_uri: &str,
    quoted_account_id: i64,
) -> Result<Option<Quote>, DbError> {
    let quote = sqlx::query_as!(
        Quote,
        r#"
        SELECT id, status_id, status_uri, account_id, quoted_status_id,
               quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at
        FROM quotes
        WHERE approval_uri = $1 AND quoted_account_id = $2
          AND state IN ('pending', 'accepted')
        "#,
        approval_uri,
        quoted_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(quote)
}

/// Inbound `Delete(QuoteAuthorization)`: the quoted author (the owner of
/// `quoted_account_id`) deletes the stamp it issued. An accepted quote becomes
/// `revoked`, a pending one `rejected` (matching Mastodon's rejection
/// semantics), and the
/// stamp URI is dropped either way. `None` when no live quote carries the
/// stamp — including when it belongs to someone else.
pub async fn revoke_by_approval_uri(
    pool: &PgPool,
    approval_uri: &str,
    quoted_account_id: i64,
) -> Result<Option<Quote>, DbError> {
    let quote = sqlx::query_as!(
        Quote,
        r#"
        UPDATE quotes
        SET state = CASE WHEN state = 'accepted' THEN 'revoked' ELSE 'rejected' END,
            approval_uri = NULL
        WHERE approval_uri = $1 AND quoted_account_id = $2
          AND state IN ('pending', 'accepted')
        RETURNING id, status_id, status_uri, account_id, quoted_status_id,
                  quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at
        "#,
        approval_uri,
        quoted_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(quote)
}

/// Accepts a pending quote once its authorization stamp verified, recording
/// the stamp (a later `Delete(QuoteAuthorization)` revokes by this URI).
pub async fn accept_with_stamp(
    pool: &PgPool,
    quote_id: i64,
    approval_uri: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE quotes SET state = 'accepted', approval_uri = $2
        WHERE id = $1 AND state = 'pending'
        "#,
        quote_id,
        approval_uri,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a fresh authorization stamp on a still-pending quote (an Update
/// delivered a `quoteAuthorization` we have not verified yet).
pub async fn set_pending_stamp(
    pool: &PgPool,
    quote_id: i64,
    approval_uri: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE quotes SET approval_uri = $2
        WHERE id = $1 AND state = 'pending'
        "#,
        quote_id,
        approval_uri,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Links the quoted post once it resolves (it may have been unfetchable when
/// the quoting post arrived).
pub async fn link_quoted_target(
    pool: &PgPool,
    quote_id: i64,
    quoted_status_id: i64,
    quoted_account_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE quotes SET quoted_status_id = $2, quoted_account_id = $3
        WHERE id = $1
        "#,
        quote_id,
        quoted_status_id,
        quoted_account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Records whether the quote is (still) a legacy one — an edit can convert a
/// legacy quote into a FEP-044f one and vice versa (Mastodon updates the flag
/// in `update_quote_approval!`).
pub async fn set_legacy(pool: &PgPool, quote_id: i64, legacy: bool) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE quotes SET legacy = $2 WHERE id = $1",
        quote_id,
        legacy,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Backfills the quoted post's URI on a row created before it was recorded.
pub async fn set_quoted_uri(pool: &PgPool, quote_id: i64, quoted_uri: &str) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE quotes SET quoted_uri = $2 WHERE id = $1 AND quoted_uri IS NULL",
        quote_id,
        quoted_uri,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes a quote relationship (an edit dropped the quote — Mastodon's
/// `update_quote!` destroy branch).
pub async fn delete_by_id(pool: &PgPool, quote_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM quotes WHERE id = $1", quote_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Marks an existing quote row's state (verification outcomes).
pub async fn set_state(pool: &PgPool, quote_id: i64, state: &str) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE quotes SET state = $2 WHERE id = $1",
        quote_id,
        state
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Links the quoting status row once it is known (inlined instrument or a
/// Create arriving after the `QuoteRequest`).
pub async fn link_status(pool: &PgPool, quote_id: i64, status_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE quotes SET status_id = $2 WHERE id = $1 AND status_id IS NULL",
        quote_id,
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn find_by_id(pool: &PgPool, quote_id: i64) -> Result<Option<Quote>, DbError> {
    let quote = sqlx::query_as!(
        Quote,
        r#"
        SELECT id, status_id, status_uri, account_id, quoted_status_id,
               quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at
        FROM quotes WHERE id = $1
        "#,
        quote_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(quote)
}

/// The quote attached to a quoting post, by the post's URI.
pub async fn find_by_status_uri(pool: &PgPool, status_uri: &str) -> Result<Option<Quote>, DbError> {
    let quote = sqlx::query_as!(
        Quote,
        r#"
        SELECT id, status_id, status_uri, account_id, quoted_status_id,
               quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at
        FROM quotes WHERE status_uri = $1
        "#,
        status_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(quote)
}

/// An accepted quote of a status: the quote row id (the pagination cursor of
/// `GET /api/v1/statuses/:id/quotes`) and the quoting status' id.
#[derive(Debug, Clone, Copy)]
pub struct QuoteOfStatus {
    pub quote_id: i64,
    pub status_id: i64,
}

/// Accepted, materialised quotes of a status, newest first, keyset-paginated
/// on the quote row id — the audience of `GET /api/v1/statuses/:id/quotes`
/// (`max_id`/`since_id` are quote ids, like Mastodon's `paginate_by_max_id`).
pub async fn accepted_quotes_of(
    pool: &PgPool,
    quoted_status_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<QuoteOfStatus>, DbError> {
    let rows = sqlx::query_as!(
        QuoteOfStatus,
        r#"
        SELECT q.id AS "quote_id!", q.status_id AS "status_id!"
        FROM quotes q
        WHERE q.quoted_status_id = $1 AND q.state = 'accepted'
          AND q.status_id IS NOT NULL
          AND ($2::bigint IS NULL OR q.id < $2)
          AND ($3::bigint IS NULL OR q.id > $3)
        ORDER BY q.id DESC
        LIMIT $4
        "#,
        quoted_status_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Quotes for a batch of (quoting) statuses, keyed by status id.
pub async fn for_statuses<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<HashMap<i64, Quote>, DbError> {
    let rows = sqlx::query_as!(
        Quote,
        r#"
        SELECT id, status_id, status_uri, account_id, quoted_status_id,
               quoted_account_id, state, activity_uri, approval_uri, quoted_uri, legacy, created_at
        FROM quotes WHERE status_id = ANY($1)
        "#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|q| q.status_id.map(|sid| (sid, q)))
        .collect())
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
    async fn quote_lifecycle(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let quoted = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice, "<p>original</p>", "public", None),
        )
        .await
        .unwrap();
        let quoting = status::create_local(
            &pool,
            status::NewLocalStatus::new(carol, "<p>look at this</p>", "public", None),
        )
        .await
        .unwrap();

        let quote_id = id::next();
        let created = create(
            &pool,
            NewQuote {
                quote_id,
                status_id: Some(quoting.id),
                status_uri: "https://plamenu.test/users/carol/statuses/x",
                account_id: carol,
                quoted_status_id: Some(quoted.id),
                quoted_account_id: Some(alice),
                state: "pending",
                activity_uri: Some("https://plamenu.test/users/carol#quote_requests/1"),
                approval_uri: None,
                quoted_uri: None,
                legacy: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(created.state, "pending");

        // Re-creating for the same status uri keeps the original row.
        let again = create(
            &pool,
            NewQuote {
                quote_id: id::next(),
                status_id: Some(quoting.id),
                status_uri: "https://plamenu.test/users/carol/statuses/x",
                account_id: carol,
                quoted_status_id: Some(quoted.id),
                quoted_account_id: Some(alice),
                state: "accepted",
                activity_uri: None,
                approval_uri: None,
                quoted_uri: None,
                legacy: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(again.id, created.id);
        assert_eq!(again.state, "pending", "state is not clobbered");

        // Accept resolves by activity uri + quoted account.
        let wrong = set_state_by_activity_uri(
            &pool,
            "https://plamenu.test/users/carol#quote_requests/1",
            carol, // not the quoted account
            "accepted",
            Some("https://x/approval"),
        )
        .await
        .unwrap();
        assert!(wrong.is_none());
        let accepted = set_state_by_activity_uri(
            &pool,
            "https://plamenu.test/users/carol#quote_requests/1",
            alice,
            "accepted",
            Some("https://x/approval"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(accepted.state, "accepted");
        assert_eq!(accepted.approval_uri.as_deref(), Some("https://x/approval"));

        let map = for_statuses(&pool, &[quoting.id]).await.unwrap();
        assert_eq!(map[&quoting.id].id, created.id);
    }

    #[sqlx::test]
    async fn stamp_deletion_revokes_accepted_and_rejects_pending(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let quoted = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice, "<p>original</p>", "public", None),
        )
        .await
        .unwrap();

        let accepted = create(
            &pool,
            NewQuote {
                quote_id: id::next(),
                status_id: None,
                status_uri: "https://remote.example/users/carol/statuses/1",
                account_id: carol,
                quoted_status_id: Some(quoted.id),
                quoted_account_id: Some(alice),
                state: "accepted",
                activity_uri: Some("https://remote.example/users/carol#quote_requests/1"),
                approval_uri: Some("https://x/stamp1"),
                quoted_uri: None,
                legacy: false,
            },
        )
        .await
        .unwrap();

        // Only the quoted author's stamp counts.
        assert!(
            find_live_by_approval_uri(&pool, "https://x/stamp1", carol)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            revoke_by_approval_uri(&pool, "https://x/stamp1", carol)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            find_live_by_approval_uri(&pool, "https://x/stamp1", alice)
                .await
                .unwrap()
                .unwrap()
                .id,
            accepted.id
        );

        let revoked = revoke_by_approval_uri(&pool, "https://x/stamp1", alice)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(revoked.id, accepted.id);
        assert_eq!(revoked.state, "revoked");
        assert!(revoked.approval_uri.is_none());
        // The stamp is spent: nothing live carries it any more.
        assert!(
            find_live_by_approval_uri(&pool, "https://x/stamp1", alice)
                .await
                .unwrap()
                .is_none()
        );

        // A pending quote is rejected rather than revoked.
        create(
            &pool,
            NewQuote {
                quote_id: id::next(),
                status_id: None,
                status_uri: "https://remote.example/users/carol/statuses/2",
                account_id: carol,
                quoted_status_id: Some(quoted.id),
                quoted_account_id: Some(alice),
                state: "pending",
                activity_uri: None,
                approval_uri: Some("https://x/stamp2"),
                quoted_uri: None,
                legacy: false,
            },
        )
        .await
        .unwrap();
        let rejected = revoke_by_approval_uri(&pool, "https://x/stamp2", alice)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rejected.state, "rejected");
    }

    #[sqlx::test]
    async fn quote_request_upgrades_pending_row_created_by_inbound_status(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let quoted = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice, "<p>original</p>", "public", None),
        )
        .await
        .unwrap();
        let quoting = status::upsert_remote(
            &pool,
            status::NewRemoteStatus {
                title: None,
                object_type: None,
                external_url: None,
                account_id: carol,
                uri: "https://remote.example/users/carol/statuses/1",
                content: "<p>quote</p>",
                created_at: OffsetDateTime::now_utc(),
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

        let from_status = create(
            &pool,
            NewQuote {
                quote_id: id::next(),
                status_id: Some(quoting.id),
                status_uri: "https://remote.example/users/carol/statuses/1",
                account_id: carol,
                quoted_status_id: Some(quoted.id),
                quoted_account_id: Some(alice),
                state: "pending",
                activity_uri: None,
                approval_uri: None,
                quoted_uri: None,
                legacy: false,
            },
        )
        .await
        .unwrap();

        let from_request = create(
            &pool,
            NewQuote {
                quote_id: id::next(),
                status_id: None,
                status_uri: "https://remote.example/users/carol/statuses/1",
                account_id: carol,
                quoted_status_id: Some(quoted.id),
                quoted_account_id: Some(alice),
                state: "accepted",
                activity_uri: Some("https://remote.example/users/carol#quote_requests/1"),
                approval_uri: None,
                quoted_uri: None,
                legacy: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(from_request.id, from_status.id);
        assert_eq!(from_request.status_id, Some(quoting.id));
        assert_eq!(from_request.state, "accepted");
        assert_eq!(
            from_request.activity_uri.as_deref(),
            Some("https://remote.example/users/carol#quote_requests/1")
        );
    }

    #[sqlx::test]
    async fn create_survives_concurrent_first_insert(pool: PgPool) {
        // Two backfills of the same thread ingest the same quoting status at
        // once, each calling `create` for the same quote. The ON CONFLICT
        // arbiter is `idx_quotes_status_uri`, but the loser's speculative
        // insert collides first on the lower-OID `quotes_status_id_key`
        // (`status_id UNIQUE`) — which used to surface as a spurious 500.
        // Every concurrent contender must succeed and converge on one row.
        const CONCURRENCY: usize = 5; // the sqlx::test pool connection cap
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        for round in 0..12 {
            // A fresh quoting status per round so each race starts from empty.
            let quoting = status::create_local(
                &pool,
                status::NewLocalStatus::new(carol, "<p>look</p>", "public", None),
            )
            .await
            .unwrap();
            let quoted = status::create_local(
                &pool,
                status::NewLocalStatus::new(alice, "<p>orig</p>", "public", None),
            )
            .await
            .unwrap();
            let status_uri =
                std::sync::Arc::new(format!("https://plamenu.test/users/carol/statuses/{round}"));

            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CONCURRENCY));
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..CONCURRENCY {
                let pool = pool.clone();
                let barrier = barrier.clone();
                let status_uri = status_uri.clone();
                tasks.spawn(async move {
                    // Each ingest mints its own candidate row id, as in prod.
                    let quote_id = id::next();
                    barrier.wait().await;
                    create(
                        &pool,
                        NewQuote {
                            quote_id,
                            status_id: Some(quoting.id),
                            status_uri: &status_uri,
                            account_id: carol,
                            quoted_status_id: Some(quoted.id),
                            quoted_account_id: Some(alice),
                            state: "pending",
                            activity_uri: Some("https://plamenu.test/users/carol#qr/1"),
                            approval_uri: None,
                            quoted_uri: None,
                            legacy: false,
                        },
                    )
                    .await
                });
            }

            let mut ids = Vec::new();
            while let Some(joined) = tasks.join_next().await {
                let quote = joined
                    .expect("task panicked")
                    .expect("quote insert raced to a 500");
                ids.push(quote.id);
            }
            assert!(ids.iter().all(|&id| id == ids[0]), "diverged ids: {ids:?}");
            let rows = sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM quotes WHERE status_id = $1"#,
                quoting.id,
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                rows, 1,
                "expected exactly one quote row for status {}",
                quoting.id
            );
        }
    }
}
