//! Web Push storage: per-token subscriptions, the VAPID keypair and the
//! push delivery queue (filled by the `web_push_fanout` trigger on every
//! notification insert, drained by the server's push worker).

use serde_json::Value;
use sqlx::PgPool;

use crate::{DbError, id};

/// How long a claimed push job stays invisible before a crashed worker's
/// batch becomes due again.
const LEASE_SECONDS: f64 = 300.0;

/// Give up on a push after this many attempts (Mastodon's Sidekiq
/// `retry: 5`).
pub const MAX_ATTEMPTS: i32 = 5;

/// The most Web Push subscriptions one user may hold at once (finding #66).
/// Every notification fans one delivery job out per matching subscription (the
/// `web_push_fanout` trigger) and a user can mint a fresh token — hence a fresh
/// subscription slot — on every login or OAuth grant, and those tokens are only
/// soft-revoked, so without a ceiling one account's accumulated sessions
/// multiply every notification's queue fanout and outbound POSTs without bound.
/// Ten is generous for honest simultaneous multi-device / multi-client use
/// (several browsers, a phone, a couple of third-party apps) while capping the
/// amplification; a new registration past the cap evicts the user's oldest
/// subscription rather than being refused, so the newest client always works.
pub const MAX_SUBSCRIPTIONS_PER_USER: i64 = 10;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Subscription {
    pub id: i64,
    pub user_id: i64,
    pub access_token_id: i64,
    /// The subscription's bearer token in the clear — carried inside every
    /// encrypted push payload, like Mastodon's.
    pub access_token: String,
    pub endpoint: String,
    pub key_p256dh: String,
    pub key_auth: String,
    /// RFC 8291 `aes128gcm` when true, the legacy `aesgcm` draft otherwise.
    pub standard: bool,
    pub policy: String,
    /// Ordered parallel arrays loaded from `web_push_alerts`.
    pub alert_kinds: Vec<String>,
    pub alert_values: Vec<bool>,
}

const COLS: &str = "id, user_id, access_token_id, access_token, endpoint, \
                    key_p256dh, key_auth, standard, policy, alert_kinds, alert_values";
const _: &str = COLS; // documentation: every query selects exactly these

pub struct NewSubscription<'a> {
    pub user_id: i64,
    pub access_token_id: i64,
    pub access_token: &'a str,
    pub endpoint: &'a str,
    pub key_p256dh: &'a str,
    pub key_auth: &'a str,
    pub standard: bool,
    pub data: &'a Value,
}

/// Replaces the token's subscription (Mastodon destroys-then-creates under
/// a per-user lock; the transaction gives the same effect) and enforces the
/// per-user subscription ceiling (finding #66).
///
/// The whole body runs under a user-scoped `pg_advisory_xact_lock` so a burst
/// of first-time registrations on distinct new tokens cannot each observe a
/// below-cap count under READ COMMITTED (their not-yet-committed rows are
/// invisible to one another) and all insert past the ceiling — the same
/// check-then-insert race [`crate::bulk_import::create_with_rows_capped`] and
/// [`crate::archive::create_if_none_within`] close. The advisory key is the
/// user id, sharing the single-int advisory keyspace with those account-scoped
/// gates; a rare id collision only briefly serializes two unrelated short
/// transactions, and distinct users never contend.
pub async fn replace_for_token(
    pool: &PgPool,
    new: NewSubscription<'_>,
) -> Result<Subscription, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", new.user_id)
        .execute(&mut *tx)
        .await?;
    // Drop this token's own prior subscription first, so re-registering an
    // existing token is net-zero on the user's total and never trips the cap.
    sqlx::query!(
        "DELETE FROM web_push_subscriptions WHERE access_token_id = $1",
        new.access_token_id,
    )
    .execute(&mut *tx)
    .await?;
    // Make room for the insert below: keep at most `MAX - 1` existing
    // subscriptions so the total after inserting one stays within the cap.
    evict_oldest_over(&mut tx, new.user_id, MAX_SUBSCRIPTIONS_PER_USER - 1).await?;
    let row = sqlx::query!(
        r#"
        INSERT INTO web_push_subscriptions
            (id, user_id, access_token_id, access_token, endpoint,
             key_p256dh, key_auth, standard, policy)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id
        "#,
        id::next(),
        new.user_id,
        new.access_token_id,
        new.access_token,
        new.endpoint,
        new.key_p256dh,
        new.key_auth,
        new.standard,
        policy_of(new.data),
    )
    .fetch_one(&mut *tx)
    .await?;
    replace_alerts(&mut tx, row.id, new.data).await?;
    tx.commit().await?;
    find_by_id(pool, row.id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound.into())
}

/// Deletes a user's oldest push subscriptions until at most `keep` remain,
/// so a following insert leaves the user within the cap (finding #66). Ordered
/// newest-first by `(created_at, id)` — both monotonic — so live sessions
/// survive and the least-recently-created (most likely abandoned) rows go
/// first. Cascades remove each evicted subscription's alerts and pending jobs.
/// Runs inside the caller's per-user advisory-locked transaction.
async fn evict_oldest_over(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
    keep: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        DELETE FROM web_push_subscriptions
        WHERE id IN (
            SELECT id FROM web_push_subscriptions
            WHERE user_id = $1
            ORDER BY created_at DESC, id DESC
            OFFSET $2
        )
        "#,
        user_id,
        keep,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn find_for_token(
    pool: &PgPool,
    access_token_id: i64,
) -> Result<Option<Subscription>, DbError> {
    let subscription = sqlx::query_as!(
        Subscription,
        r#"
        SELECT id, user_id, access_token_id, access_token, endpoint,
               key_p256dh, key_auth, standard, policy,
               ARRAY(SELECT a.kind FROM web_push_alerts a
                     WHERE a.subscription_id = s.id ORDER BY a.kind) AS "alert_kinds!",
               ARRAY(SELECT a.enabled FROM web_push_alerts a
                     WHERE a.subscription_id = s.id ORDER BY a.kind) AS "alert_values!"
        FROM web_push_subscriptions s
        WHERE s.access_token_id = $1
        "#,
        access_token_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(subscription)
}

pub async fn find_by_id(pool: &PgPool, id: i64) -> Result<Option<Subscription>, DbError> {
    let subscription = sqlx::query_as!(
        Subscription,
        r#"
        SELECT id, user_id, access_token_id, access_token, endpoint,
               key_p256dh, key_auth, standard, policy,
               ARRAY(SELECT a.kind FROM web_push_alerts a
                     WHERE a.subscription_id = s.id ORDER BY a.kind) AS "alert_kinds!",
               ARRAY(SELECT a.enabled FROM web_push_alerts a
                     WHERE a.subscription_id = s.id ORDER BY a.kind) AS "alert_values!"
        FROM web_push_subscriptions s
        WHERE s.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(subscription)
}

pub async fn find_for_user_by_id(
    pool: &PgPool,
    user_id: i64,
    id: i64,
) -> Result<Option<Subscription>, DbError> {
    let subscription = sqlx::query_as!(
        Subscription,
        r#"
        SELECT id, user_id, access_token_id, access_token, endpoint,
               key_p256dh, key_auth, standard, policy,
               ARRAY(SELECT a.kind FROM web_push_alerts a
                     WHERE a.subscription_id = s.id ORDER BY a.kind) AS "alert_kinds!",
               ARRAY(SELECT a.enabled FROM web_push_alerts a
                     WHERE a.subscription_id = s.id ORDER BY a.kind) AS "alert_values!"
        FROM web_push_subscriptions s
        WHERE s.user_id = $1 AND s.id = $2
        "#,
        user_id,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(subscription)
}

/// Replaces the subscription's `data` (alerts + policy), returning the
/// updated row — `None` when the token has no subscription.
pub async fn update_data(
    pool: &PgPool,
    access_token_id: i64,
    data: &Value,
) -> Result<Option<Subscription>, DbError> {
    let mut tx = pool.begin().await?;
    let id = sqlx::query_scalar!(
        r#"
        UPDATE web_push_subscriptions
        SET policy = $2, updated_at = now()
        WHERE access_token_id = $1
        RETURNING id
        "#,
        access_token_id,
        policy_of(data),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        tx.commit().await?;
        return Ok(None);
    };
    replace_alerts(&mut tx, id, data).await?;
    tx.commit().await?;
    find_by_id(pool, id).await
}

/// Replaces a web-client subscription's `data` by row id, scoped to the
/// session user. The web API addresses subscriptions by id rather than by the
/// current OAuth token's singleton slot.
pub async fn update_data_for_user_by_id(
    pool: &PgPool,
    user_id: i64,
    id: i64,
    data: &Value,
) -> Result<Option<Subscription>, DbError> {
    let mut tx = pool.begin().await?;
    let found = sqlx::query_scalar!(
        r#"
        UPDATE web_push_subscriptions
        SET policy = $3, updated_at = now()
        WHERE user_id = $1 AND id = $2
        RETURNING id
        "#,
        user_id,
        id,
        policy_of(data),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = found else {
        tx.commit().await?;
        return Ok(None);
    };
    replace_alerts(&mut tx, id, data).await?;
    tx.commit().await?;
    find_by_id(pool, id).await
}

fn policy_of(data: &Value) -> &str {
    data.get("policy")
        .and_then(Value::as_str)
        .filter(|policy| !policy.is_empty())
        .unwrap_or("all")
}

async fn replace_alerts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    subscription_id: i64,
    data: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "DELETE FROM web_push_alerts WHERE subscription_id = $1",
        subscription_id,
    )
    .execute(&mut **tx)
    .await?;
    let Some(alerts) = data.get("alerts").and_then(Value::as_object) else {
        return Ok(());
    };
    let pairs: Vec<(&str, bool)> = alerts
        .iter()
        .filter_map(|(kind, value)| value.as_bool().map(|enabled| (kind.as_str(), enabled)))
        .collect();
    if pairs.is_empty() {
        return Ok(());
    }
    let kinds: Vec<String> = pairs.iter().map(|(kind, _)| (*kind).to_owned()).collect();
    let values: Vec<bool> = pairs.iter().map(|(_, enabled)| *enabled).collect();
    sqlx::query!(
        r#"
        INSERT INTO web_push_alerts (subscription_id, kind, enabled)
        SELECT $1, input.kind, input.enabled
        FROM unnest($2::text[], $3::boolean[]) AS input(kind, enabled)
        "#,
        subscription_id,
        &kinds,
        &values,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn delete_for_token(pool: &PgPool, access_token_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM web_push_subscriptions WHERE access_token_id = $1",
        access_token_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_for_user_by_id(pool: &PgPool, user_id: i64, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM web_push_subscriptions WHERE user_id = $1 AND id = $2",
        user_id,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Removes a dead subscription (the push service answered a permanent 4xx);
/// its queued jobs cascade away with it.
pub async fn delete(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM web_push_subscriptions WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Highest-priority contributing reason enabled for this subscription. A
/// mention wins over a quote, which wins over notify-on-post; other
/// notifications carry their sole kind as their sole reason.
#[must_use]
pub fn preferred_alert_kind<'a>(
    subscription: &Subscription,
    reasons: &'a [String],
) -> Option<&'a str> {
    let enabled = |reason: &str| {
        subscription
            .alert_kinds
            .iter()
            .zip(&subscription.alert_values)
            .any(|(stored_kind, enabled)| stored_kind == reason && *enabled)
    };
    ["mention", "quote", "status"]
        .into_iter()
        .find(|kind| reasons.iter().any(|reason| reason == kind) && enabled(kind))
        .or_else(|| {
            reasons
                .iter()
                .find(|reason| enabled(reason))
                .map(String::as_str)
        })
}

/// Mastodon's `pushable?` re-check at delivery time: at least one contributing
/// reason's alert is on, and the subscription policy allows the sender.
pub async fn pushable(
    pool: &PgPool,
    subscription: &Subscription,
    recipient_account_id: i64,
    from_account_id: i64,
    reasons: &[String],
) -> Result<bool, DbError> {
    if preferred_alert_kind(subscription, reasons).is_none() {
        return Ok(false);
    }
    let (follower, target) = match subscription.policy.as_str() {
        "all" => return Ok(true),
        "followed" => (recipient_account_id, from_account_id),
        "follower" => (from_account_id, recipient_account_id),
        // Mastodon's case has no else arm: 'none' and anything unknown
        // allow nothing.
        _ => return Ok(false),
    };
    let found = sqlx::query_scalar!(
        r#"
        SELECT 1 AS "one" FROM follows
        WHERE account_id = $1 AND target_account_id = $2 AND NOT pending
        "#,
        follower,
        target,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PushJob {
    pub id: i64,
    pub subscription_id: i64,
    pub notification_id: i64,
    /// Attempts so far, including the one being made now.
    pub attempts: i32,
}

/// Claims up to `limit` due push jobs, bumping their attempt counter and
/// leasing them for [`LEASE_SECONDS`].
///
/// Jobs are picked round-robin across recipient *users* rather
/// than strictly oldest-first: one abusive account with a large accumulated
/// backlog can no longer fill an entire batch and starve every other user's
/// pushes. The window ranks each user's due jobs oldest-first, then the batch
/// takes every user's first job before any user's second, and so on. The
/// single push worker (guaranteed by the single-writer invariant) is the only
/// claimer, so the lease — pushing `run_at` into the future — rather than
/// `SKIP LOCKED` is what makes a crashed batch reclaimable.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<PushJob>, DbError> {
    let jobs = sqlx::query_as!(
        PushJob,
        r#"
        UPDATE push_delivery_jobs j SET
            attempts = j.attempts + 1,
            run_at = now() + make_interval(secs => $2)
        FROM (
            SELECT ranked.id
            FROM (
                SELECT dj.id,
                       dj.run_at AS due_at,
                       row_number() OVER (
                           PARTITION BY s.user_id ORDER BY dj.run_at, dj.id
                       ) AS user_rank
                FROM push_delivery_jobs dj
                JOIN web_push_subscriptions s ON s.id = dj.subscription_id
                WHERE dj.run_at <= now()
            ) ranked
            ORDER BY ranked.user_rank, ranked.due_at, ranked.id
            LIMIT $1
        ) picked
        WHERE j.id = picked.id
        RETURNING j.id, j.subscription_id, j.notification_id, j.attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a delivered (or permanently failed) push job.
pub async fn complete(pool: &PgPool, job_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM push_delivery_jobs WHERE id = $1", job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Reschedules a failed push with exponential backoff (60s, 4m, 16m, ~1h).
pub async fn retry_later(pool: &PgPool, job_id: i64, attempts: i32) -> Result<(), DbError> {
    let exp = u32::try_from(attempts.clamp(1, 9)).unwrap_or(1) - 1;
    let delay = f64::from((60u32 * 4u32.saturating_pow(exp)).min(21_600));
    sqlx::query!(
        "UPDATE push_delivery_jobs SET run_at = now() + make_interval(secs => $2) WHERE id = $1",
        job_id,
        delay,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Number of pushes not yet delivered (diagnostics / tests).
pub async fn pending_count(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM push_delivery_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Test hook: makes every push job due now.
pub async fn make_all_due(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!("UPDATE push_delivery_jobs SET run_at = now()")
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct VapidKeys {
    /// P-256, PKCS#8 PEM.
    pub private_key: String,
    /// Uncompressed SEC1 point, base64url (padded).
    pub public_key: String,
}

/// The stored VAPID keypair, if one has been generated.
pub async fn vapid_get(pool: &PgPool) -> Result<Option<VapidKeys>, DbError> {
    let row = sqlx::query!("SELECT private_key, public_key FROM vapid_keys WHERE id = 1")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| VapidKeys {
        private_key: r.private_key,
        public_key: r.public_key,
    }))
}

/// Stores a freshly generated VAPID keypair unless one already exists,
/// returning the winning row either way (first-boot races are harmless).
pub async fn vapid_create_if_missing(
    pool: &PgPool,
    private_key_pem: &str,
    public_key_b64: &str,
) -> Result<VapidKeys, DbError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO vapid_keys (id, private_key, public_key)
        VALUES (1, $1, $2)
        ON CONFLICT (id) DO UPDATE SET id = vapid_keys.id
        RETURNING private_key, public_key
        "#,
        private_key_pem,
        public_key_b64,
    )
    .fetch_one(pool)
    .await?;
    Ok(VapidKeys {
        private_key: row.private_key,
        public_key: row.public_key,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::account::{self, NewLocalAccount};

    #[test]
    fn coalesced_push_uses_highest_enabled_reason() {
        let subscription = Subscription {
            id: 1,
            user_id: 1,
            access_token_id: 1,
            access_token: String::new(),
            endpoint: String::new(),
            key_p256dh: String::new(),
            key_auth: String::new(),
            standard: true,
            policy: "all".to_owned(),
            alert_kinds: vec![
                "mention".to_owned(),
                "quote".to_owned(),
                "status".to_owned(),
            ],
            alert_values: vec![false, true, true],
        };
        let reasons = vec![
            "mention".to_owned(),
            "quote".to_owned(),
            "status".to_owned(),
        ];

        assert_eq!(preferred_alert_kind(&subscription, &reasons), Some("quote"));
    }
    use crate::{notification, oauth, user};

    /// A local account with a user and an access token; returns
    /// (`account_id`, `user_id`, `token_id`).
    async fn local_user(pool: &PgPool, username: &str) -> (i64, i64, i64) {
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
        let user = user::create(
            pool,
            account.id,
            Some(&format!("{username}@example.test")),
            "hash",
        )
        .await
        .unwrap();
        let app = oauth::create_app(
            pool,
            oauth::NewApp {
                name: "test",
                website: None,
                client_id: username,
                client_secret_hash: "secret",
                redirect_uris: &[],
                scopes: "push",
            },
        )
        .await
        .unwrap();
        let token = oauth::create_token(
            pool,
            &format!("{username}-token-hash"),
            app.id,
            Some(user.id),
            "push",
        )
        .await
        .unwrap();
        (account.id, user.id, token.id)
    }

    fn subscription(user_id: i64, token_id: i64, data: &Value) -> NewSubscription<'_> {
        NewSubscription {
            user_id,
            access_token_id: token_id,
            access_token: "the-bearer-token",
            endpoint: "https://push.example/endpoint",
            key_p256dh: "p256dh",
            key_auth: "auth",
            standard: false,
            data,
        }
    }

    /// Mints an extra access token (under its own app) for an existing user,
    /// so a test can register more than one push subscription for one user.
    async fn mint_token(pool: &PgPool, user_id: i64, tag: &str) -> i64 {
        let app = oauth::create_app(
            pool,
            oauth::NewApp {
                name: "test",
                website: None,
                client_id: tag,
                client_secret_hash: "secret",
                redirect_uris: &[],
                scopes: "push",
            },
        )
        .await
        .unwrap();
        oauth::create_token(pool, &format!("{tag}-hash"), app.id, Some(user_id), "push")
            .await
            .unwrap()
            .id
    }

    async fn subscription_count(pool: &PgPool, user_id: i64) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM web_push_subscriptions WHERE user_id = $1"#,
            user_id,
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn coalesced_reasons_enqueue_one_push_job(pool: PgPool) {
        let (recipient, user_id, token_id) = local_user(&pool, "alice").await;
        let (sender, _, _) = local_user(&pool, "bob").await;
        replace_for_token(
            &pool,
            subscription(
                user_id,
                token_id,
                &json!({
                    "alerts": {"mention": true, "quote": true, "status": true},
                    "policy": "all"
                }),
            ),
        )
        .await
        .unwrap();
        let post = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(sender, "<p>hello</p>", "public", None),
        )
        .await
        .unwrap();

        notification::create_post_notifications_many(
            &pool,
            &[notification::PostNotification {
                account_id: recipient,
                mention: true,
                quote: true,
                status: true,
            }],
            sender,
            post.id,
        )
        .await
        .unwrap();

        assert_eq!(pending_count(&pool).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn replace_is_per_token(pool: PgPool) {
        let (_, user_id, token_id) = local_user(&pool, "alice").await;
        let first = replace_for_token(&pool, subscription(user_id, token_id, &json!({})))
            .await
            .unwrap();
        let second = replace_for_token(&pool, subscription(user_id, token_id, &json!({})))
            .await
            .unwrap();
        assert_ne!(first.id, second.id);
        let found = find_for_token(&pool, token_id).await.unwrap().unwrap();
        assert_eq!(found.id, second.id);
    }

    #[sqlx::test]
    async fn registration_caps_subscriptions_per_user_evicting_oldest(pool: PgPool) {
        let (_, user_id, first_token) = local_user(&pool, "alice").await;
        // A second user must be untouched by alice's cap.
        let (_, other_user, other_token) = local_user(&pool, "bob").await;
        replace_for_token(&pool, subscription(other_user, other_token, &json!({})))
            .await
            .unwrap();

        // Register on the first token plus enough fresh tokens to exceed the cap.
        let cap = MAX_SUBSCRIPTIONS_PER_USER;
        let mut tokens = vec![first_token];
        for i in 0..(cap + 2) {
            tokens.push(mint_token(&pool, user_id, &format!("alice-{i}")).await);
        }
        for token in &tokens {
            replace_for_token(&pool, subscription(user_id, *token, &json!({})))
                .await
                .unwrap();
        }

        // Total is pinned at the cap, and the oldest registrations were evicted
        // while the newest `cap` survived.
        assert_eq!(subscription_count(&pool, user_id).await, cap);
        let evicted = tokens.len() - usize::try_from(cap).unwrap();
        for token in &tokens[..evicted] {
            assert!(find_for_token(&pool, *token).await.unwrap().is_none());
        }
        for token in &tokens[evicted..] {
            assert!(find_for_token(&pool, *token).await.unwrap().is_some());
        }
        // The other user's subscription is unaffected.
        assert_eq!(subscription_count(&pool, other_user).await, 1);
    }

    #[sqlx::test]
    async fn reregistering_an_existing_token_at_the_cap_evicts_nothing(pool: PgPool) {
        let (_, user_id, first_token) = local_user(&pool, "alice").await;
        let cap = usize::try_from(MAX_SUBSCRIPTIONS_PER_USER).unwrap();
        let mut tokens = vec![first_token];
        for i in 0..(cap - 1) {
            tokens.push(mint_token(&pool, user_id, &format!("alice-{i}")).await);
        }
        for token in &tokens {
            replace_for_token(&pool, subscription(user_id, *token, &json!({})))
                .await
                .unwrap();
        }
        assert_eq!(
            subscription_count(&pool, user_id).await,
            MAX_SUBSCRIPTIONS_PER_USER
        );

        // Re-registering the oldest token refreshes it in place: still at the
        // cap, and every other token's subscription survives.
        replace_for_token(&pool, subscription(user_id, first_token, &json!({})))
            .await
            .unwrap();
        assert_eq!(
            subscription_count(&pool, user_id).await,
            MAX_SUBSCRIPTIONS_PER_USER
        );
        for token in &tokens {
            assert!(find_for_token(&pool, *token).await.unwrap().is_some());
        }
    }

    #[sqlx::test]
    async fn concurrent_registrations_cannot_exceed_the_cap(pool: PgPool) {
        let (_, user_id, first_token) = local_user(&pool, "alice").await;
        let mut tokens = vec![first_token];
        for i in 0..=MAX_SUBSCRIPTIONS_PER_USER {
            tokens.push(mint_token(&pool, user_id, &format!("alice-{i}")).await);
        }
        // Fire every first-time registration at once. Without the per-user
        // advisory lock they could each observe a below-cap count and all
        // insert past the ceiling.
        let mut handles = Vec::new();
        for token in tokens {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                replace_for_token(&pool, subscription(user_id, token, &json!({})))
                    .await
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(
            subscription_count(&pool, user_id).await,
            MAX_SUBSCRIPTIONS_PER_USER
        );
    }

    #[sqlx::test]
    async fn one_notification_fans_out_at_most_the_cap_of_jobs(pool: PgPool) {
        let (account_id, user_id, first_token) = local_user(&pool, "alice").await;
        let (sender_id, _, _) = local_user(&pool, "bob").await;

        // Accumulate well past the cap: distinct tokens, each subscribed with
        // the favourite alert on.
        let data = json!({"alerts": {"favourite": true}, "policy": "all"});
        let mut tokens = vec![first_token];
        for i in 0..(MAX_SUBSCRIPTIONS_PER_USER + 5) {
            tokens.push(mint_token(&pool, user_id, &format!("alice-{i}")).await);
        }
        for token in &tokens {
            replace_for_token(&pool, subscription(user_id, *token, &data))
                .await
                .unwrap();
        }
        assert_eq!(
            subscription_count(&pool, user_id).await,
            MAX_SUBSCRIPTIONS_PER_USER
        );

        // One notification enqueues one job per surviving subscription — bounded
        // by the cap, not the number of tokens ever registered (finding #66).
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(
            pending_count(&pool).await.unwrap(),
            u64::try_from(MAX_SUBSCRIPTIONS_PER_USER).unwrap()
        );
    }

    #[sqlx::test]
    async fn revoking_a_token_drops_its_subscription_and_queued_jobs(pool: PgPool) {
        let (account_id, user_id, token_id) = local_user(&pool, "alice").await;
        let (sender_id, _, _) = local_user(&pool, "bob").await;
        replace_for_token(
            &pool,
            subscription(
                user_id,
                token_id,
                &json!({"alerts": {"favourite": true}, "policy": "all"}),
            ),
        )
        .await
        .unwrap();
        // A notification queues a push for the live subscription.
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(subscription_count(&pool, user_id).await, 1);
        assert_eq!(pending_count(&pool).await.unwrap(), 1);

        // Revoking the session's token — here through the real "sign out
        // everywhere" path, but the trigger fires for any revocation entry
        // point — deletes the subscription and cascades its queued job away, so
        // a discarded session can no longer keep multiplying pushes (finding
        // #66).
        let revoked = oauth::revoke_all_for_user(&pool, user_id).await.unwrap();
        assert_eq!(revoked, 1);
        assert_eq!(subscription_count(&pool, user_id).await, 0);
        assert_eq!(pending_count(&pool).await.unwrap(), 0);

        // A later notification for the same account fans out to nothing.
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn claim_due_is_fair_across_users(pool: PgPool) {
        let (alice_acct, alice_user, alice_token) = local_user(&pool, "alice").await;
        let (bob_acct, bob_user, bob_token) = local_user(&pool, "bob").await;
        let (sender, _, _) = local_user(&pool, "sender").await;

        let data = json!({"alerts": {"favourite": true}, "policy": "all"});
        replace_for_token(&pool, subscription(alice_user, alice_token, &data))
            .await
            .unwrap();
        let bob_sub = replace_for_token(&pool, subscription(bob_user, bob_token, &data))
            .await
            .unwrap();

        // Alice floods her own push queue; bob has only two pending pushes.
        // Under a strictly oldest-first claim, alice's older backlog would fill
        // the whole batch and starve bob. The fair round-robin claim (finding
        // #66) takes one push from each user in turn, so bob's pushes ride in
        // the same small batch instead of waiting behind alice's flood.
        for _ in 0..10 {
            notification::create(&pool, alice_acct, sender, "favourite", None)
                .await
                .unwrap();
        }
        for _ in 0..2 {
            notification::create(&pool, bob_acct, sender, "favourite", None)
                .await
                .unwrap();
        }
        assert_eq!(pending_count(&pool).await.unwrap(), 12);

        let batch = claim_due(&pool, 4).await.unwrap();
        assert_eq!(batch.len(), 4);
        let bob_jobs = batch
            .iter()
            .filter(|job| job.subscription_id == bob_sub.id)
            .count();
        assert_eq!(
            bob_jobs, 2,
            "both of bob's pushes are claimed despite alice's larger backlog"
        );
    }

    #[sqlx::test]
    async fn fanout_trigger_respects_alerts_and_policy(pool: PgPool) {
        let (account_id, user_id, token_id) = local_user(&pool, "alice").await;
        let (sender_id, _, _) = local_user(&pool, "bob").await;

        // No subscription yet: no jobs.
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0);

        // Alert on: enqueued.
        replace_for_token(
            &pool,
            subscription(
                user_id,
                token_id,
                &json!({"alerts": {"favourite": true}, "policy": "all"}),
            ),
        )
        .await
        .unwrap();
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 1);

        // Alert for another kind only: not enqueued.
        notification::create(&pool, account_id, sender_id, "follow", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 1);

        // policy=followed without a follow: not enqueued.
        replace_for_token(
            &pool,
            subscription(
                user_id,
                token_id,
                &json!({"alerts": {"favourite": true}, "policy": "followed"}),
            ),
        )
        .await
        .unwrap();
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0); // old job cascaded with the replace

        // policy=followed with the follow in place: enqueued.
        crate::follow::create(&pool, account_id, sender_id, None)
            .await
            .unwrap();
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 1);

        // policy=none: nothing.
        replace_for_token(
            &pool,
            subscription(
                user_id,
                token_id,
                &json!({"alerts": {"favourite": true}, "policy": "none"}),
            ),
        )
        .await
        .unwrap();
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn fanout_trigger_skips_policy_filtered_notifications(pool: PgPool) {
        let (account_id, user_id, token_id) = local_user(&pool, "alice").await;
        let (sender_id, _, _) = local_user(&pool, "bob").await;
        replace_for_token(
            &pool,
            subscription(
                user_id,
                token_id,
                &json!({"alerts": {"favourite": true}, "policy": "all"}),
            ),
        )
        .await
        .unwrap();

        // Filter senders the recipient doesn't follow: the notification is
        // stored hidden and must not reach the phone.
        let policy = crate::notification_policy::Policy {
            for_not_following: crate::notification_policy::Disposition::Filter,
            ..Default::default()
        };
        crate::notification_policy::upsert(&pool, account_id, policy)
            .await
            .unwrap();
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0);

        // The same notification from a followed (accepted) sender pushes.
        crate::follow::create(&pool, account_id, sender_id, None)
            .await
            .unwrap();
        notification::create(&pool, account_id, sender_id, "favourite", None)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn job_queue_lifecycle(pool: PgPool) {
        let (account_id, user_id, token_id) = local_user(&pool, "alice").await;
        let (sender_id, _, _) = local_user(&pool, "bob").await;
        let sub = replace_for_token(
            &pool,
            subscription(user_id, token_id, &json!({"alerts": {"follow": true}})),
        )
        .await
        .unwrap();
        notification::create(&pool, account_id, sender_id, "follow", None)
            .await
            .unwrap();

        let jobs = claim_due(&pool, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].subscription_id, sub.id);
        assert_eq!(jobs[0].attempts, 1);
        // Leased: nothing else due.
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());

        retry_later(&pool, jobs[0].id, jobs[0].attempts)
            .await
            .unwrap();
        make_all_due(&pool).await.unwrap();
        let again = claim_due(&pool, 10).await.unwrap();
        assert_eq!(again[0].attempts, 2);

        // Deleting the subscription cascades its jobs away.
        delete(&pool, sub.id).await.unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn vapid_keys_are_create_once(pool: PgPool) {
        assert!(vapid_get(&pool).await.unwrap().is_none());
        let first = vapid_create_if_missing(&pool, "pem-a", "pub-a")
            .await
            .unwrap();
        let second = vapid_create_if_missing(&pool, "pem-b", "pub-b")
            .await
            .unwrap();
        assert_eq!(first.private_key, "pem-a");
        assert_eq!(second.private_key, "pem-a");
        assert_eq!(vapid_get(&pool).await.unwrap().unwrap().public_key, "pub-a");
    }
}
