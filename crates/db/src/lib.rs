//! `PostgreSQL` persistence layer for Plamenu.
//!
//! Conventions:
//! - IDs are time-ordered snowflakes (see [`id`]) stored as `BIGINT`.
//! - All queries go through `sqlx` compile-time-checked macros; run `cargo sqlx prepare
//!   --workspace` after changing any of them so the committed `.sqlx` metadata stays in sync.

pub mod account;
pub mod account_alias;
pub mod account_domain_block;
pub mod account_media;
pub mod account_migration;
pub mod account_moderation_note;
pub mod account_move_job;
pub mod account_note;
pub mod account_warning;
pub mod actor_key;
pub mod admin_account;
pub mod admin_action_log;
pub mod announcement;
pub mod appeal;
pub mod archive;
pub mod backfill;
pub mod block;
pub mod bookmark;
pub mod bulk_import;
pub mod collection;
pub mod conversation;
pub mod custom_emoji;
pub mod custom_filter;
pub mod discovery;
pub mod dislike;
pub mod domain_severance_job;
pub mod email;
pub mod endorsement;
pub mod export;
pub mod favourite;
pub mod featured_tag;
pub mod follow;
pub mod gateway;
pub mod group;
pub mod id;
pub mod identity_proof;
pub mod instance_policy;
pub mod instance_settings;
pub mod invite;
pub mod job;
pub mod lemmy_emoji;
pub mod lemmy_id;
pub mod lemmy_inbox;
pub mod lemmy_media;
pub mod lemmy_user;
pub mod link_verification;
pub mod list;
pub mod login_activity;
pub mod maintenance;
pub mod marker;
pub mod media;
pub mod media_cleanup;
pub mod media_fetch_failure;
pub mod mention;
pub mod metrics;
pub mod move_replay;
pub mod mute;
pub mod notification;
pub mod notification_policy;
pub mod notification_request;
pub mod oauth;
pub mod pin;
pub mod poll;
pub mod post_read;
pub mod preview_card;
pub mod preview_card_provider;
pub mod preview_card_trend;
pub mod quote;
pub mod quote_verify_job;
pub mod rate_limit;
pub mod reachability;
pub mod reaction;
pub mod relay;
pub mod remote_fetch_failure;
pub mod remote_group;
pub mod remote_history;
pub mod remote_stream_source;
pub mod reply_fetch;
pub mod report;
pub mod report_note;
pub mod retention_sweep;
pub mod role;
pub mod rule;
pub mod scheduled_status;
pub mod signature_prefs;
pub mod single_writer;
pub mod site_upload;
pub mod software_update;
pub mod status;
pub mod status_edit;
pub mod status_event;
pub mod status_participation;
pub mod status_tombstone;
pub mod status_translation;
pub mod status_trend;
pub mod statuses_cleanup;
pub mod streaming;
pub mod suggestion;
pub mod tag;
pub mod tag_trend;
pub mod tagged_object;
pub mod terms_of_service;
pub mod two_factor;
pub mod tz;
pub mod user;
pub mod username_block;
pub mod warning_preset;
pub mod web_push;
pub mod web_setting;
pub mod webauthn_credential;
pub mod webhook;
pub mod webxdc;

pub use sqlx::PgPool;
pub use sqlx::postgres::PgListener;
use sqlx::postgres::PgPoolOptions;
// `PgConnection`/`PgExecutor` are re-exported so the server crate can thread a
// transaction connection through its domain helpers without a direct sqlx
// dependency.
pub use sqlx::{PgConnection, PgExecutor};
use thiserror::Error;

/// Outcome of an idempotent interaction insert: the row id and whether this
/// call inserted it. `inserted == false` means the row already existed — a
/// repeat or redelivered interaction — so callers must skip one-shot side
/// effects (notifications, streaming pushes, federation deliveries).
#[derive(Debug, Clone, Copy)]
pub struct Upserted {
    pub id: i64,
    pub inserted: bool,
}

/// How many times [`upsert_racing`] re-runs an upsert after losing a race to a
/// concurrent insert of the same row. Concurrent thread-ancestor backfills are
/// the practical trigger, and two or three contenders on a single row is
/// already extreme, so a small bound converges while still letting a genuine
/// (non-racing) constraint violation surface promptly.
const UPSERT_RACE_RETRIES: u32 = 3;

/// Runs an `INSERT … ON CONFLICT (arbiter) DO UPDATE` upsert, retrying when a
/// concurrent transaction inserting the *same* row wins the race.
///
/// Postgres only diverts to the `DO UPDATE` branch when the duplicate is
/// detected on the statement's declared arbiter index. When two connections
/// insert the same row at once, the loser's speculative insertion can instead
/// collide on a *different* unique index with a lower OID — e.g.
/// `quotes_status_id_key` while the arbiter is `idx_quotes_status_uri` — and
/// Postgres raises a `23505` unique violation rather than upserting. (Remote
/// `accounts` upserts no longer have such a rival index: since migration 0148
/// their only uniqueness is the arbiter `idx_accounts_uri`, so two actors
/// sharing a handle but not a `uri` both insert cleanly rather than one losing
/// the (username, domain) slot.) By then the rival has committed, so simply
/// re-running the statement lets the arbiter observe the committed row and take
/// the update branch. Bounded so a real, non-racing violation still surfaces.
///
/// Each call runs as its own implicit (autocommit) transaction on the pool, so
/// a failed attempt rolls back cleanly and leaves nothing to reset before the
/// retry — do not use this to wrap a statement inside an explicit transaction,
/// where a failed statement poisons the surrounding transaction.
pub(crate) async fn upsert_racing<T, F, Fut>(mut run: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    let mut attempt = 0;
    loop {
        match run().await {
            Err(sqlx::Error::Database(e))
                if e.is_unique_violation() && attempt < UPSERT_RACE_RETRIES =>
            {
                attempt += 1;
            }
            result => return result,
        }
    }
}

#[derive(Debug, Error)]
pub enum DbError {
    #[error("protocol data is invalid: {0}")]
    Protocol(String),
    #[error("username is already taken")]
    UsernameTaken,
    #[error("email is already registered")]
    EmailTaken,
    #[error("that security key name is already in use")]
    WebauthnNicknameTaken,
    #[error(
        "another Plamenu writer process already holds the single-writer lock; \
         Plamenu does not support running multiple writers against one database"
    )]
    SingleWriterHeld,
    #[error("another Plamenu CLI command is using this database; retry after it completes")]
    CliWriterHeld,
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// Embedded migrations, applied automatically on server start.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Connects a pool and brings the schema up to date. `max_connections` bounds
/// the one pool that every HTTP handler and background worker supervisor shares,
/// so it is operator-configurable (`database_pool_size`): the built-in default
/// suits a single-node deployment, but a busy instance with many concurrent
/// requests and workers can raise it (bounded, in turn, by the database
/// server's own `max_connections`).
pub async fn connect_and_migrate(
    database_url: &str,
    max_connections: u32,
) -> Result<PgPool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await?;
    MIGRATOR.run(&pool).await.map_err(sqlx::Error::from)?;
    Ok(pool)
}

/// Readiness probe for the connection pool: acquires a pooled connection and
/// runs a trivial round-trip, so a dead, unreachable, or exhausted database
/// surfaces as an `Err` instead of the false "healthy" a bare liveness check
/// would report. Used by the `/ready` endpoint; deliberately does not touch
/// application tables, only connectivity.
pub async fn ping(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!(r#"SELECT 1 AS "ok!""#).fetch_one(pool).await?;
    Ok(())
}

#[cfg(test)]
mod schema_contract_tests {
    use std::collections::BTreeSet;

    use super::*;

    /// JSON storage is an explicit exception, never a convenient default.
    /// The remaining columns are wire/client/library-owned opaque
    /// formats. Fixed-shape application data must otherwise use columns and
    /// child tables (profile fields live in `account_fields`; readers get
    /// their legacy JSON shape from `account_fields_json()`).
    #[sqlx::test(migrations = "./migrations")]
    async fn json_columns_are_limited_to_reviewed_opaque_formats(pool: PgPool) {
        let actual: BTreeSet<(String, String)> = sqlx::query!(
            r#"
            SELECT table_name, column_name
            FROM information_schema.columns
            WHERE table_schema = 'public' AND data_type IN ('json', 'jsonb')
            "#,
        )
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.table_name.unwrap(), row.column_name.unwrap()))
        .collect();
        let expected = BTreeSet::from([
            // Preserve complete signed FEP-c390 wire documents, including proof fields.
            ("account_identity_proofs".to_owned(), "documents".to_owned()),
            ("delivery_jobs".to_owned(), "activity".to_owned()),
            ("gateway_actors".to_owned(), "actor".to_owned()),
            ("gateway_collection_items".to_owned(), "object".to_owned()),
            ("gateway_objects".to_owned(), "object".to_owned()),
            (
                "lemmy_user_preferences".to_owned(),
                "preferences".to_owned(),
            ),
            (
                "two_factor_challenges".to_owned(),
                "webauthn_state".to_owned(),
            ),
            ("web_settings".to_owned(), "data".to_owned()),
            ("webauthn_credentials".to_owned(), "credential".to_owned()),
            ("webxdc_updates".to_owned(), "raw_create".to_owned()),
            ("webxdc_updates".to_owned(), "webxdc_update".to_owned()),
        ]);
        assert_eq!(actual, expected);
    }
}

#[cfg(test)]
mod ping_tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn ping_succeeds_against_a_live_pool(pool: PgPool) {
        ping(&pool).await.expect("live pool is ready");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ping_errors_once_the_pool_is_closed(pool: PgPool) {
        // A closed pool stands in for an unreachable database: `ping` must
        // surface the failure so `/ready` can answer 503 instead of a false OK.
        pool.close().await;
        assert!(
            ping(&pool).await.is_err(),
            "a closed pool must not report ready"
        );
    }
}
