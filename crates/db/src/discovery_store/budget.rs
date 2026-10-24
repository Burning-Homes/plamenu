use super::{
    Completion, Lease, PgPool, address, bounded, delete_claim, invalid, lock_settings, valid_lease,
};
use crate::{DbError, id};
use time::OffsetDateTime;

pub const RESPONSE_BYTES: i64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reservation {
    Reserved(i64),
    Stale,
    Capacity,
    Budget,
}

fn request_purpose(lease: &Lease) -> &'static str {
    if lease.exploration {
        "exploration"
    } else if lease.operation == "refresh_presentation"
        || (lease.entry_revision.is_some()
            && matches!(
                lease.operation.as_str(),
                "refresh_entry" | "revalidate_entry"
            ))
    {
        "maintenance"
    } else {
        "acquisition"
    }
}

/// Each wire attempt, including redirects and signature negotiation, reserves
/// a full body before network I/O. The row lock makes all limits process-wide.
/// Discovery currently follows only same-origin redirects; new origins need
/// their own admitted source/job rather than inheriting a source's authority.
pub async fn reserve_request(
    pool: &PgPool,
    lease: &Lease,
    uri: &str,
) -> Result<Reservation, DbError> {
    let (_, origin) = address(uri)?;
    if !super::request_origin_allowed(pool, lease, uri).await? {
        return Err(invalid("discovery redirect left its source origin"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Reservation::Stale);
    }
    sqlx::query!("DELETE FROM discovery_requests WHERE started_at<=clock_timestamp()-interval '24 hours' AND active_until<=clock_timestamp()")
        .execute(&mut *tx).await?;
    let usage = sqlx::query!(r#"SELECT count(*) AS "requests!",
        count(*) FILTER (WHERE origin=$1) AS "origin_requests!",
        count(*) FILTER (WHERE NOT exploration) AS "refresh_requests!",
        COALESCE(sum(bytes) FILTER (WHERE NOT exploration),0)::bigint AS "refresh_bytes!",
        COALESCE(sum(bytes),0)::bigint AS "bytes!",
        count(*) FILTER (WHERE NOT completed AND active_until>clock_timestamp()) AS "active!",
        count(*) FILTER (WHERE origin=$1 AND NOT completed AND active_until>clock_timestamp()) AS "origin_active!"
        FROM discovery_requests"#,origin).fetch_one(&mut *tx).await?;
    let purpose = request_purpose(lease);
    let allowance: (i64, i64, i64, bool) = sqlx::query_as(
        "SELECT
        count(*) FILTER(WHERE purpose='acquisition'),count(*) FILTER(WHERE purpose='maintenance'),
        count(*) FILTER(WHERE purpose='exploration'), EXISTS(SELECT 1 FROM discovery_jobs j
          JOIN discovery_identity i ON i.canonical_uri=j.target_uri
          WHERE j.operation IN ('refresh_entry','revalidate_entry') AND NOT j.withdrawn
            AND i.verified_at IS NOT NULL AND i.next_check_at<=now() AND j.run_at<=now())
        FROM discovery_requests",
    )
    .fetch_one(&mut *tx)
    .await?;
    // Migration 0187 constrains this persisted limit to 1..=100_000.
    let refill_rate = f64::from(
        i32::try_from(settings.requests_per_day)
            .map_err(|_| invalid("discovery request limit is out of range"))?,
    );
    let tokens: f64=sqlx::query_scalar("UPDATE discovery_pacing SET tokens=LEAST(42,tokens+EXTRACT(epoch FROM now()-refilled_at)::double precision*$1/86400),refilled_at=now() RETURNING tokens")
        .bind(refill_rate).fetch_one(&mut *tx).await?;
    let continuing: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM discovery_requests WHERE lease_token=$1)")
            .bind(lease.lease_token)
            .fetch_one(&mut *tx)
            .await?;
    let purpose_full = match purpose {
        "exploration" => allowance.2 >= (settings.requests_per_day / 10).max(1),
        "maintenance" => allowance.1 >= (settings.requests_per_day / 5).max(1),
        _ => {
            allowance.0 >= settings.requests_per_day * 7 / 10
                && (allowance.3 || allowance.0 + allowance.1 >= settings.requests_per_day * 9 / 10)
        }
    };
    let result = if usage.active >= i64::from(settings.max_inflight) || usage.origin_active > 0 {
        Reservation::Capacity
    } else if usage.requests >= settings.requests_per_day
        || usage.origin_requests >= settings.requests_per_origin
        || usage.bytes + RESPONSE_BYTES > settings.bytes_per_day
        || purpose_full
        || tokens < if continuing { -16.0 } else { 1.0 }
    {
        Reservation::Budget
    } else {
        if let Some(run) = lease.search_run_id
            && !super::wider::charge_search(&mut tx, run).await?
        {
            return Ok(Reservation::Budget);
        }
        // Advance fairness on real work, not on claims that only discover an
        // empty token bucket. Otherwise repair turns disappear without a fetch.
        if !continuing && lease.search_run_id.is_none() {
            sqlx::query("UPDATE discovery_scheduler SET turn=(turn+1)%5")
                .execute(&mut *tx)
                .await?;
        }
        let id = id::next();
        sqlx::query("INSERT INTO discovery_requests(id,origin,exploration,family,operation,purpose,lease_token)
             SELECT $1,$2,$3,CASE WHEN $4::bigint IS NOT NULL THEN 'search'
               ELSE discovery_family($5,$6,$7,s.capabilities,(SELECT kind FROM discovery_entries WHERE canonical_uri=$8)) END,$5,$10,$11
             FROM discovery_sources s WHERE s.id=$9")
            .bind(id).bind(&origin).bind(lease.exploration).bind(lease.search_run_id).bind(&lease.operation)
            .bind(uri).bind(&lease.cursor).bind(&lease.target_uri).bind(lease.source_id).bind(purpose).bind(lease.lease_token)
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE discovery_pacing SET tokens=tokens-1")
            .execute(&mut *tx)
            .await?;
        if purpose == "acquisition" && allowance.0 >= settings.requests_per_day * 7 / 10 {
            sqlx::query(
                "SELECT discovery_count($1,'all','acquisition','maintenance_allowance_transfer')",
            )
            .bind(&origin)
            .execute(&mut *tx)
            .await?;
        }
        Reservation::Reserved(id)
    };
    tx.commit().await?;
    Ok(result)
}

/// Idempotent settlement cannot erase request counts. Failed body reads and
/// crashed workers retain the reservation; a failed send settles zero bytes.
/// Completion works even after pause.
pub async fn complete_request(pool: &PgPool, ticket: i64, bytes: i64) -> Result<(), DbError> {
    if !(0..=RESPONSE_BYTES).contains(&bytes) {
        return Err(invalid("invalid discovery body charge"));
    }
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    sqlx::query!("UPDATE discovery_requests SET bytes=$2,completed=true,active_until=clock_timestamp() WHERE id=$1 AND NOT completed",ticket,bytes)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn lease_is_current(pool: &PgPool, lease: &Lease) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    valid_lease(&mut tx, &settings, lease).await
}

/// Local admission pressure is not a failed remote attempt. Keep the cursor
/// and TTL and defer without using up the finite failure retry allowance.
pub async fn defer_job(
    pool: &PgPool,
    lease: &Lease,
    seconds: i32,
    reason: &str,
) -> Result<Completion, DbError> {
    bounded(reason, 128)?;
    if !(1..=86400).contains(&seconds) {
        return Err(invalid("invalid discovery deferral"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    let seconds = if reason == "budget_exhausted" {
        budget_delay(&mut tx, lease, &settings).await?
    } else {
        seconds
    };
    sqlx::query!("UPDATE discovery_jobs SET attempts=GREATEST(attempts-1,0),run_at=clock_timestamp()+make_interval(secs=>$3),last_error=$4,lease_token=NULL,lease_owner=NULL,leased_until=NULL WHERE id=$1 AND lease_token=$2",lease.id,lease.lease_token,f64::from(seconds),reason)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

/// Cache endpoint failures on their retained source, so re-enqueued hints do
/// not bypass backoff after the last job attempt. Rate limits cover the origin.
pub async fn fail_job(
    pool: &PgPool,
    lease: &Lease,
    seconds: i32,
    reason: &str,
    whole_origin: bool,
) -> Result<Completion, DbError> {
    bounded(reason, 128)?;
    if !(1..=604_800).contains(&seconds) {
        return Err(invalid("invalid discovery backoff"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    if lease.attempts >= settings.max_attempts {
        delete_claim(&mut tx, lease).await?;
    } else {
        sqlx::query!("UPDATE discovery_jobs SET run_at=clock_timestamp()+make_interval(secs=>$3),last_error=$4 WHERE id=$1 AND lease_token=$2",lease.id,lease.lease_token,f64::from(seconds),reason).execute(&mut *tx).await?;
    }
    let purpose = if lease.exploration {
        "exploration"
    } else if lease.entry_revision.is_some() || lease.operation == "refresh_presentation" {
        "maintenance"
    } else {
        "acquisition"
    };
    let origin = address(&lease.target_uri)?.1;
    sqlx::query("SELECT discovery_count($1,'candidate',$2,$3)")
        .bind(&origin)
        .bind(purpose)
        .bind(format!("failure:{reason}"))
        .execute(&mut *tx)
        .await?;
    if lease.operation == "refresh_entry" {
        sqlx::query("INSERT INTO discovery_identity(canonical_uri,kind,source_id,outcome,next_check_at,forget_after)
          VALUES($1,'candidate',$2,'error',now()+make_interval(secs=>$3),now()+interval '365 days')
          ON CONFLICT(canonical_uri) DO UPDATE SET next_check_at=GREATEST(discovery_identity.next_check_at,excluded.next_check_at)")
          .bind(&lease.target_uri).bind(lease.source_id).bind(f64::from(seconds)).execute(&mut *tx).await?;
    }
    let (_, origin) = address(&lease.target_uri)?;
    if whole_origin || lease.operation == "inspect_source" {
        sqlx::query!("UPDATE discovery_sources SET cooldown_until=GREATEST(cooldown_until,clock_timestamp()+make_interval(secs=>$3)),last_error=$4,last_failure_at=clock_timestamp(),revision=revision+1 WHERE id=$1 OR ($2 AND origin=$5)",lease.source_id,whole_origin,f64::from(seconds),reason,origin).execute(&mut *tx).await?;
        sqlx::query!("UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL WHERE source_id IN (SELECT id FROM discovery_sources WHERE id=$1 OR ($2 AND origin=$3))",lease.source_id,whole_origin,origin).execute(&mut *tx).await?;
    } else {
        sqlx::query("UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL WHERE id=$1")
          .bind(lease.id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Completion::Committed)
}

#[derive(Debug, serde::Serialize)]
pub struct Diagnostics {
    pub sources: i64,
    pub entries: i64,
    pub queued: i64,
    pub leased: i64,
    pub withdrawal_guards: i64,
    pub requests_24h: i64,
    /// Actual completed bodies plus conservative reservations for failures.
    pub charged_bytes_24h: i64,
    pub active_requests: i64,
    pub next_charge_expires_at: Option<OffsetDateTime>,
    pub last_success_at: Option<OffsetDateTime>,
    pub last_failure_at: Option<OffsetDateTime>,
    pub last_failure_reason: Option<String>,
}

pub async fn diagnostics(pool: &PgPool) -> Result<Diagnostics, DbError> {
    Ok(sqlx::query_as!(Diagnostics,r#"SELECT
        (SELECT count(*) FROM discovery_sources) AS "sources!",
        (SELECT count(*) FROM discovery_entries) AS "entries!",
        (SELECT count(*) FROM discovery_jobs WHERE NOT withdrawn) AS "queued!",
        (SELECT count(*) FROM discovery_jobs WHERE lease_token IS NOT NULL AND leased_until>clock_timestamp()) AS "leased!",
        (SELECT count(*) FROM discovery_jobs WHERE withdrawn) AS "withdrawal_guards!",
        (SELECT count(*) FROM discovery_requests WHERE started_at>clock_timestamp()-interval '24 hours') AS "requests_24h!",
        (SELECT COALESCE(sum(bytes),0)::bigint FROM discovery_requests WHERE started_at>clock_timestamp()-interval '24 hours') AS "charged_bytes_24h!",
        (SELECT count(*) FROM discovery_requests WHERE NOT completed AND active_until>clock_timestamp()) AS "active_requests!",
        (SELECT min(started_at)+interval '24 hours' FROM discovery_requests WHERE started_at>clock_timestamp()-interval '24 hours') AS next_charge_expires_at,
        (SELECT max(last_success_at) FROM discovery_sources) AS last_success_at,
        (SELECT max(last_failure_at) FROM discovery_sources) AS last_failure_at,
        (SELECT last_error FROM discovery_sources WHERE last_failure_at IS NOT NULL ORDER BY last_failure_at DESC LIMIT 1) AS last_failure_reason
        "#).fetch_one(pool).await?)
}

/// Bounded operator view. No remote work or refresh is triggered by inspection.
pub async fn source_diagnostics(pool: &PgPool) -> Result<Vec<super::Source>, DbError> {
    Ok(sqlx::query_as!(
        super::Source,
        "SELECT * FROM discovery_sources ORDER BY id LIMIT 200"
    )
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, serde::Serialize)]
pub struct QueueDiagnostic {
    pub operation: String,
    pub reason: Option<String>,
    pub jobs: i64,
    pub next_run_at: Option<OffsetDateTime>,
}

pub async fn queue_diagnostics(pool: &PgPool) -> Result<Vec<QueueDiagnostic>, DbError> {
    Ok(sqlx::query_as!(QueueDiagnostic,r#"SELECT operation,last_error AS reason,count(*) AS "jobs!",min(run_at) AS next_run_at FROM discovery_jobs WHERE NOT withdrawn GROUP BY operation,last_error ORDER BY count(*) DESC,operation,last_error LIMIT 50"#).fetch_all(pool).await?)
}

#[derive(Debug, serde::Serialize)]
pub struct OriginDiagnostic {
    pub origin: String,
    pub requests: i64,
    pub charged_bytes: i64,
}

pub async fn origin_diagnostics(pool: &PgPool) -> Result<Vec<OriginDiagnostic>, DbError> {
    Ok(sqlx::query_as!(OriginDiagnostic,r#"SELECT origin,count(*) AS "requests!",COALESCE(sum(bytes),0)::bigint AS "charged_bytes!" FROM discovery_requests WHERE started_at>clock_timestamp()-interval '24 hours' GROUP BY origin ORDER BY count(*) DESC,origin LIMIT 200"#).fetch_all(pool).await?)
}

async fn budget_delay(
    tx: &mut super::Tx<'_>,
    lease: &Lease,
    s: &super::Settings,
) -> Result<i32, DbError> {
    let origin = address(&lease.target_uri)?.1;
    let purpose = request_purpose(lease);
    let seconds: f64=sqlx::query_scalar("SELECT GREATEST(1,
      COALESCE((SELECT EXTRACT(epoch FROM min(started_at)+interval '24 hours'-now()) FROM discovery_requests HAVING count(*) >= $1 OR sum(bytes)+2097152>$3),0),
      COALESCE((SELECT EXTRACT(epoch FROM min(started_at)+interval '24 hours'-now()) FROM discovery_requests WHERE origin=$2 HAVING count(*) >= $4),0),
      COALESCE((SELECT EXTRACT(epoch FROM min(started_at)+interval '24 hours'-now()) FROM discovery_requests WHERE purpose=$5 HAVING count(*) >= CASE $5 WHEN 'exploration' THEN GREATEST(1,$1/10) WHEN 'maintenance' THEN GREATEST(1,$1/5) ELSE GREATEST(1,$1*7/10) END),0),
      (SELECT GREATEST(0,1-tokens)*86400/$1 FROM discovery_pacing))::double precision")
      .bind(s.requests_per_day).bind(origin).bind(s.bytes_per_day).bind(s.requests_per_origin).bind(purpose)
      .fetch_one(&mut **tx).await?;
    Ok(bounded_delay(seconds))
}

fn bounded_delay(seconds: f64) -> i32 {
    std::time::Duration::try_from_secs_f64(seconds.ceil().clamp(1.0, 86400.0)).map_or(1, |delay| {
        i32::try_from(delay.as_secs()).expect("delay is bounded to one day")
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn delay_rounds_up_and_saturates_at_the_existing_bounds() {
        for (seconds, expected) in [
            (f64::NAN, 1),
            (f64::NEG_INFINITY, 1),
            (-1.0, 1),
            (0.0, 1),
            (1.0, 1),
            (1.001, 2),
            (86399.001, 86400),
            (86400.0, 86400),
            (f64::MAX, 86400),
            (f64::INFINITY, 86400),
        ] {
            assert_eq!(super::bounded_delay(seconds), expected);
        }
    }

    use super::*;
    use crate::discovery_store::{self as db, Admission, Operation};
    async fn setup(pool: &PgPool, uri: &str) -> Lease {
        let mut settings = db::settings(pool).await.unwrap();
        settings.enabled = true;
        db::save_settings(pool, &settings).await.unwrap();
        let Admission::Added(source) = db::add_source(pool, uri, None).await.unwrap() else {
            panic!("new source")
        };
        db::enqueue(pool, source, uri, Operation::InspectSource, None)
            .await
            .unwrap();
        db::claim_due(pool, "test").await.unwrap().remove(0)
    }
    async fn verified_lease(pool: &PgPool, uri: &str, operation: Operation) -> Lease {
        let inspection = setup(pool, uri).await;
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            db::finish_source(
                pool,
                &inspection,
                &db::VerifiedSource {
                    canonical_uri: uri,
                    capabilities: &["directory".into()],
                    verified_at: now,
                    valid_until: now + time::Duration::days(1),
                },
            )
            .await
            .unwrap(),
            Completion::Committed
        );
        assert!(matches!(
            db::enqueue(pool, inspection.source_id, uri, operation, None)
                .await
                .unwrap(),
            Admission::Added(_)
        ));
        db::claim_due_for(pool, "verified", &[operation])
            .await
            .unwrap()
            .remove(0)
    }
    fn receipt(result: Reservation) -> i64 {
        let Reservation::Reserved(id) = result else {
            panic!("{result:?}")
        };
        id
    }
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn request_limits_are_atomic_and_settlement_is_idempotent(pool: PgPool) {
        let lease = setup(&pool, "https://a.example/source").await;
        let (first, second) = tokio::join!(
            reserve_request(&pool, &lease, &lease.target_uri),
            reserve_request(&pool, &lease, &lease.target_uri)
        );
        let results = [first.unwrap(), second.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Reservation::Reserved(_)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Reservation::Capacity))
                .count(),
            1
        );
        let ticket = receipt(
            *results
                .iter()
                .find(|r| matches!(r, Reservation::Reserved(_)))
                .unwrap(),
        );
        complete_request(&pool, ticket, 42).await.unwrap();
        complete_request(&pool, ticket, 0).await.unwrap();
        assert_eq!(diagnostics(&pool).await.unwrap().charged_bytes_24h, 42);
        assert!(
            reserve_request(&pool, &lease, "https://other.example/redirect")
                .await
                .is_err()
        );
        assert_eq!(diagnostics(&pool).await.unwrap().requests_24h, 1);
    }
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn request_and_origin_caps_survive_reclaims(pool: PgPool) {
        setup(&pool, "https://a.example/source").await;
        let mut settings = db::settings(&pool).await.unwrap();
        // Two exploration slots keep the first refusal specific to the origin cap.
        settings.requests_per_day = 20;
        settings.requests_per_origin = 1;
        db::save_settings(&pool, &settings).await.unwrap();
        let lease = db::claim_due(&pool, "new").await.unwrap().remove(0);
        let ticket = receipt(
            reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap(),
        );
        complete_request(&pool, ticket, 2).await.unwrap();
        assert_eq!(
            reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap(),
            Reservation::Budget
        );
        // Tightening policy fences the old lease without refunding its receipt.
        // Spend one maintenance slot next, so a later acquisition has its own
        // allowance left and can only be refused by the global two-wire cap.
        let mut settings = db::settings(&pool).await.unwrap();
        settings.requests_per_day = 2;
        assert!(db::save_settings(&pool, &settings).await.unwrap());
        let reclaimed = db::claim_due(&pool, "after-tightening")
            .await
            .unwrap()
            .remove(0);
        assert_eq!(reclaimed.id, lease.id);
        assert_ne!(reclaimed.lease_token, lease.lease_token);
        assert_eq!(
            reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap(),
            Reservation::Stale
        );
        assert_eq!(diagnostics(&pool).await.unwrap().requests_24h, 1);
        db::finish_job(&pool, &reclaimed).await.unwrap();
        let other = verified_lease(
            &pool,
            "https://b.example/source",
            Operation::RefreshPresentation,
        )
        .await;
        let ticket = receipt(
            reserve_request(&pool, &other, &other.target_uri)
                .await
                .unwrap(),
        );
        complete_request(&pool, ticket, 2).await.unwrap();
        db::finish_job(&pool, &other).await.unwrap();
        let third =
            verified_lease(&pool, "https://c.example/source", Operation::ListDirectory).await;
        assert_eq!(
            reserve_request(&pool, &third, &third.target_uri)
                .await
                .unwrap(),
            Reservation::Budget
        );
        assert_eq!(diagnostics(&pool).await.unwrap().requests_24h, 2);
        assert_eq!(diagnostics(&pool).await.unwrap().charged_bytes_24h, 4);
        let purposes: Vec<String> =
            sqlx::query_scalar("SELECT purpose FROM discovery_requests ORDER BY purpose")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(purposes, ["exploration", "maintenance"]);
    }
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn exploration_allowance_blocks_a_fresh_origin_before_the_global_cap(pool: PgPool) {
        setup(&pool, "https://a.example/source").await;
        let mut settings = db::settings(&pool).await.unwrap();
        settings.requests_per_day = 2;
        assert!(db::save_settings(&pool, &settings).await.unwrap());
        let first = db::claim_due(&pool, "exploration").await.unwrap().remove(0);
        assert!(first.exploration);
        let ticket = receipt(
            reserve_request(&pool, &first, &first.target_uri)
                .await
                .unwrap(),
        );
        complete_request(&pool, ticket, 2).await.unwrap();
        assert_eq!(
            db::finish_job(&pool, &first).await.unwrap(),
            Completion::Committed
        );
        let second = setup(&pool, "https://b.example/source").await;
        assert!(second.exploration);
        assert_eq!(
            reserve_request(&pool, &second, &second.target_uri)
                .await
                .unwrap(),
            Reservation::Budget
        );
        assert_eq!(diagnostics(&pool).await.unwrap().requests_24h, 1);
        assert_eq!(diagnostics(&pool).await.unwrap().charged_bytes_24h, 2);
    }
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn crash_charges_full_body_until_rolling_window_expires(pool: PgPool) {
        setup(&pool, "https://a.example/source").await;
        let mut settings = db::settings(&pool).await.unwrap();
        settings.bytes_per_day = RESPONSE_BYTES;
        db::save_settings(&pool, &settings).await.unwrap();
        let lease = db::claim_due(&pool, "new").await.unwrap().remove(0);
        receipt(
            reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap(),
        );
        // Simulate the network deadline passing after a worker crash; the
        // allowance remains fully spent even though its concurrency slot frees.
        sqlx::query!(
            "UPDATE discovery_requests SET active_until=clock_timestamp()-interval '1 second'"
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap(),
            Reservation::Budget
        );
        assert_eq!(
            diagnostics(&pool).await.unwrap().charged_bytes_24h,
            RESPONSE_BYTES
        );
        sqlx::query!(
            "UPDATE discovery_requests SET started_at=clock_timestamp()-interval '25 hours'"
        )
        .execute(&pool)
        .await
        .unwrap();
        receipt(
            reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap(),
        );
        assert_eq!(diagnostics(&pool).await.unwrap().requests_24h, 1);
    }
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn pause_fences_requests_without_freeing_physical_slots(pool: PgPool) {
        let lease = setup(&pool, "https://a.example/source").await;
        let ticket = receipt(
            reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap(),
        );
        let mut settings = db::settings(&pool).await.unwrap();
        settings.paused = true;
        db::save_settings(&pool, &settings).await.unwrap();
        assert_eq!(
            reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap(),
            Reservation::Stale
        );
        let mut settings = db::settings(&pool).await.unwrap();
        settings.paused = false;
        db::save_settings(&pool, &settings).await.unwrap();
        let current = db::claim_due(&pool, "resumed").await.unwrap().remove(0);
        assert_eq!(
            reserve_request(&pool, &current, &current.target_uri)
                .await
                .unwrap(),
            Reservation::Capacity
        );
        complete_request(&pool, ticket, 2).await.unwrap();
        receipt(
            reserve_request(&pool, &current, &current.target_uri)
                .await
                .unwrap(),
        );
        assert_eq!(diagnostics(&pool).await.unwrap().requests_24h, 2);
    }
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn local_deferral_preserves_cursor_and_remote_retry_allowance(pool: PgPool) {
        let lease = setup(&pool, "https://a.example/source").await;
        assert_eq!(
            defer_job(&pool, &lease, 30, "budget_exhausted")
                .await
                .unwrap(),
            Completion::Committed
        );
        let row=sqlx::query!("SELECT attempts,lease_token,last_error,run_at>clock_timestamp() AS delayed FROM discovery_jobs WHERE id=$1",lease.id).fetch_one(&pool).await.unwrap();
        assert_eq!(row.attempts, 0);
        assert!(row.lease_token.is_none());
        assert_eq!(row.delayed, Some(true));
        assert_eq!(row.last_error.as_deref(), Some("budget_exhausted"));
        assert_eq!(
            defer_job(&pool, &lease, 1, "stale").await.unwrap(),
            Completion::Stale
        );
    }
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn negative_cache_survives_job_exhaustion_and_limits_whole_origin(pool: PgPool) {
        let lease = setup(&pool, "https://a.example/source").await;
        let mut settings = db::settings(&pool).await.unwrap();
        settings.max_attempts = 1;
        db::save_settings(&pool, &settings).await.unwrap();
        // The settings fence retains the attempt count. A fresh operator hint
        // after normal pruning starts a new bounded job, not a new source.
        db::prune_jobs(&pool).await.unwrap();
        db::enqueue(
            &pool,
            lease.source_id,
            &lease.target_uri,
            Operation::InspectSource,
            None,
        )
        .await
        .unwrap();
        let lease = db::claim_due(&pool, "new").await.unwrap().remove(0);
        db::add_source(&pool, "https://a.example/other", None)
            .await
            .unwrap();
        assert_eq!(
            fail_job(&pool, &lease, 7200, "rate_limited", true)
                .await
                .unwrap(),
            Completion::Committed
        );
        db::enqueue(
            &pool,
            lease.source_id,
            &lease.target_uri,
            Operation::InspectSource,
            None,
        )
        .await
        .unwrap();
        assert!(db::claim_due(&pool, "again").await.unwrap().is_empty());
        db::add_source(&pool, "https://a.example/added-after-limit", None)
            .await
            .unwrap();
        assert!(
            db::source_diagnostics(&pool)
                .await
                .unwrap()
                .iter()
                .all(|s| s.cooldown_until.is_some()
                    && s.last_error.as_deref() == Some("rate_limited"))
        );
        let status = diagnostics(&pool).await.unwrap();
        assert!(status.last_failure_at.is_some());
        assert_eq!(status.last_failure_reason.as_deref(), Some("rate_limited"));
    }
}
