//! Per-host HTTP-signature dialect memory (migration 0138) — the state
//! behind RFC 9421 emission.
//!
//! Learned from a *positive capability signal* only, never from a bare
//! outbound `200` (upstream Pleroma answers 200 to a 9421 inbox POST then
//! asynchronously drops it — the black hole). Two signals record
//! `rfc9421 = true` for a host:
//! - **Primary — FEP-844e `implements`:** a fetched actor that advertises the
//!   rfc9421 capability declares its host can verify our 9421 deliveries
//!   (recorded in `store_remote_actor`). This is what keeps 9421 alive with
//!   the peers that support it (Mitra, other FEP-521a software) even though
//!   they *emit* draft-cavage.
//! - **Secondary — inbound observation:** a peer that itself signs us a
//!   verified RFC 9421 request demonstrably speaks the dialect (recorded in
//!   the inbox handler).
//!
//! Outbound delivery never records `true`; its only write is a *downgrade* to
//! `false` when a known-positive host hard-refuses a 9421 knock and falls back.
//! Absence of a `true` row means draft-cavage (universal, lossless here).

use time::OffsetDateTime;

use crate::{DbError, PgPool};

/// How long a positive RFC 9421 verdict is trusted before the host is treated
/// as unknown again (re-proven by the next RFC 9421 inbound it sends).
pub const RE_KNOCK_AFTER_DAYS: i64 = 30;

#[derive(Debug, Clone)]
pub struct HostSignaturePref {
    pub rfc9421: bool,
    pub updated_at: OffsetDateTime,
}

pub async fn find(pool: &PgPool, host: &str) -> Result<Option<HostSignaturePref>, DbError> {
    Ok(sqlx::query_as!(
        HostSignaturePref,
        "SELECT rfc9421, updated_at FROM host_signature_prefs WHERE host = $1",
        host,
    )
    .fetch_optional(pool)
    .await?)
}

/// Records an observed dialect for `host`. Called from the inbound path with
/// `true` when a peer signs us a valid RFC 9421 request. Refreshes
/// `updated_at` only when the verdict changed or at most daily — a busy host
/// must not turn every inbound into a row write.
pub async fn record(pool: &PgPool, host: &str, rfc9421: bool) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO host_signature_prefs (host, rfc9421)
         VALUES ($1, $2)
         ON CONFLICT (host) DO UPDATE
             SET rfc9421 = EXCLUDED.rfc9421, updated_at = now()
             WHERE host_signature_prefs.rfc9421 IS DISTINCT FROM EXCLUDED.rfc9421
                OR host_signature_prefs.updated_at < now() - interval '1 day'",
        host,
        rfc9421,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether a delivery to `host` should emit RFC 9421: only when the host has
/// a *positive*, non-stale row (proven by an RFC 9421 request it signed us).
/// Unknown hosts, hosts with a `false` row, and stale positives all get
/// draft-cavage — the universal, lossless default. Never inferred from an
/// outbound status: a `200` is not proof (see module docs).
pub async fn should_try_rfc9421(pool: &PgPool, host: &str) -> Result<bool, DbError> {
    let pref = find(pool, host).await?;
    Ok(match pref {
        Some(pref) if pref.rfc9421 => {
            OffsetDateTime::now_utc() - pref.updated_at < time::Duration::days(RE_KNOCK_AFTER_DAYS)
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn unknown_hosts_default_to_cavage(pool: PgPool) {
        // No inbound RFC 9421 seen from this host → cavage, not a 9421 knock.
        assert!(!should_try_rfc9421(&pool, "new.example").await.unwrap());
    }

    #[sqlx::test]
    async fn inbound_observation_enables_rfc9421(pool: PgPool) {
        // A peer that signs us a valid RFC 9421 request is recorded positive,
        // and subsequent deliveries to it emit RFC 9421.
        record(&pool, "modern.example", true).await.unwrap();
        assert!(should_try_rfc9421(&pool, "modern.example").await.unwrap());
    }

    #[sqlx::test]
    async fn stale_positive_falls_back_to_cavage(pool: PgPool) {
        record(&pool, "modern.example", true).await.unwrap();
        assert!(should_try_rfc9421(&pool, "modern.example").await.unwrap());

        // No RFC 9421 inbound for over a month: the proof has gone stale, so
        // we conservatively drop back to cavage until the peer re-proves it.
        sqlx::query!(
            "UPDATE host_signature_prefs SET updated_at = now() - interval '31 days'
             WHERE host = 'modern.example'"
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(!should_try_rfc9421(&pool, "modern.example").await.unwrap());
    }

    #[sqlx::test]
    async fn false_row_stays_cavage(pool: PgPool) {
        record(&pool, "cavage.example", false).await.unwrap();
        assert!(!should_try_rfc9421(&pool, "cavage.example").await.unwrap());
    }

    #[sqlx::test]
    async fn unchanged_verdicts_skip_the_row_write(pool: PgPool) {
        record(&pool, "steady.example", true).await.unwrap();
        let first = find(&pool, "steady.example").await.unwrap().unwrap();
        record(&pool, "steady.example", true).await.unwrap();
        let second = find(&pool, "steady.example").await.unwrap().unwrap();
        assert_eq!(first.updated_at, second.updated_at);
    }
}
