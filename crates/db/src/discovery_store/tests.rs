use super::*;
use std::sync::Arc;
use time::Duration;
use tokio::sync::Barrier;
use tokio::task::JoinSet;

#[sqlx::test(migrations = "./migrations")]
async fn sampling_position_survives_rounds_and_pause_without_extending_actor_freshness(
    pool: PgPool,
) {
    enable(&pool).await;
    let endpoint = "https://sampling.example/api/v1/directory";
    let source = added(add_source(&pool, endpoint, None).await.unwrap());
    for round in 0..7 {
        enqueue(&pool, source, endpoint, Operation::InspectSource, None)
            .await
            .unwrap();
        let lease = claim_due(&pool, "sampling").await.unwrap().remove(0);
        let plan = sampling_plan(&pool, &lease).await.unwrap();
        assert_eq!(plan.round, round);
        assert_eq!(plan.newcomers(), round == 4);
        assert_eq!(
            plan.offset,
            match round {
                0 | 4 => 0,
                1 => 20,
                2 => 40,
                3 => 60,
                5 => 80,
                _ => 100,
            }
        );
        assert_eq!(
            finish_directory_page(&pool, &lease, &[], true)
                .await
                .unwrap(),
            Completion::Committed
        );
    }
    let next = sqlx::query_scalar::<_, bool>(
        "SELECT next_at>now()+interval '14 minutes' FROM discovery_sampling WHERE source_id=$1",
    )
    .bind(source)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        next,
        "successful small probes schedule another bounded round"
    );
    let mut policy = settings(&pool).await.unwrap();
    policy.paused = true;
    save_settings(&pool, &policy).await.unwrap();
    assert!(claim_due(&pool, "paused").await.unwrap().is_empty());
    assert_eq!(diagnostics(&pool).await.unwrap().entries, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn video_sampling_keeps_advancing_when_profile_rounds_would_switch_to_new(pool: PgPool) {
    enable(&pool).await;
    let endpoint = "https://video-sampling.example/api/v1/videos";
    let source = added(add_source(&pool, endpoint, None).await.unwrap());
    for round in 0..7 {
        enqueue(&pool, source, endpoint, Operation::InspectSource, None)
            .await
            .unwrap();
        let lease = claim_due_for(&pool, "sampling", &[Operation::InspectSource])
            .await
            .unwrap()
            .remove(0);
        let plan = sampling_plan(&pool, &lease).await.unwrap();
        assert_eq!(plan.round, round);
        assert_eq!(i64::from(plan.offset), round * 8);
        finish_video_sample(&pool, &lease, &[], true).await.unwrap();
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn scheduler_balances_candidate_costs_instead_of_cheap_wire_attempts(pool: PgPool) {
    let mut policy = enable(&pool).await;
    policy.max_inflight = 1;
    policy.requests_per_day = 100;
    save_settings(&pool, &policy).await.unwrap();
    for endpoint in [
        "https://people.example/api/v1/directory",
        "https://groups.example/api/v3/community/list",
        "https://films.example/api/v1/videos",
    ] {
        let source = added(add_source(&pool, endpoint, None).await.unwrap());
        enqueue(&pool, source, endpoint, Operation::InspectSource, None)
            .await
            .unwrap();
    }
    let mut rounds = std::collections::HashMap::new();
    for _ in 0..12 {
        let lease = claim_due(&pool, "fair").await.unwrap().remove(0);
        let cost = if lease.target_uri.ends_with("/videos") {
            4
        } else if lease.target_uri.ends_with("/list") {
            2
        } else {
            1
        };
        *rounds.entry(lease.source_id).or_insert(0) += 1;
        for _ in 0..cost {
            let Reservation::Reserved(ticket) = reserve_request(&pool, &lease, &lease.target_uri)
                .await
                .unwrap()
            else {
                panic!("budget available")
            };
            complete_request(&pool, ticket, 100).await.unwrap();
        }
        finish_job(&pool, &lease).await.unwrap();
        enqueue(
            &pool,
            lease.source_id,
            &lease.target_uri,
            Operation::InspectSource,
            None,
        )
        .await
        .unwrap();
    }
    assert!(
        rounds.values().all(|n| *n == 4),
        "each family gets four candidate rounds: {rounds:?}"
    );
    assert_eq!(diagnostics(&pool).await.unwrap().requests_24h, 28);
    let usage = family_diagnostics(&pool).await.unwrap();
    assert_eq!(
        usage.iter().find(|r| r.family == "video").unwrap().requests,
        16
    );
    assert_eq!(
        usage
            .iter()
            .find(|r| r.family == "community")
            .unwrap()
            .requests,
        8
    );
    assert_eq!(
        usage
            .iter()
            .find(|r| r.family == "profile")
            .unwrap()
            .requests,
        4
    );
}

async fn enable(pool: &PgPool) -> Settings {
    let mut settings = settings(pool).await.unwrap();
    settings.enabled = true;
    assert!(save_settings(pool, &settings).await.unwrap());
    super::settings(pool).await.unwrap()
}

fn added(outcome: Admission) -> i64 {
    match outcome {
        Admission::Added(id) | Admission::Existing(id) => id,
        other => panic!("{other:?}"),
    }
}

async fn verified_source(pool: &PgPool, endpoint: &str, introducer: Option<i64>) -> i64 {
    let id = added(add_source(pool, endpoint, introducer).await.unwrap());
    enqueue(pool, id, endpoint, Operation::InspectSource, introducer)
        .await
        .unwrap();
    let lease = claim_due(pool, "verify").await.unwrap().remove(0);
    assert_eq!(lease.source_id, id);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        finish_source(
            pool,
            &lease,
            &VerifiedSource {
                canonical_uri: endpoint,
                capabilities: &["directory".into(), "peer_exploration".into()],
                verified_at: now,
                valid_until: now + Duration::days(1)
            }
        )
        .await
        .unwrap(),
        Completion::Committed
    );
    id
}

async fn refresh(pool: &PgPool, source: i64, uri: &str) -> Lease {
    assert!(matches!(
        enqueue(pool, source, uri, Operation::RefreshEntry, None)
            .await
            .unwrap(),
        Admission::Added(_)
    ));
    claim_due(pool, "refresh").await.unwrap().remove(0)
}

fn facts(uri: &str) -> VerifiedEntry<'_> {
    let now = OffsetDateTime::now_utc();
    VerifiedEntry {
        outbox_uri: None,
        published_at: None,
        sensitive: false,
        author_revision: None,
        kind: "profile",
        actor_uri: uri,
        account_id: None,
        discoverable: "missing",
        indexable: "missing",
        visibility: "public",
        title: "Name",
        summary: "Public biography",
        language: Some("en"),
        verified_at: now,
        valid_until: now + Duration::hours(1),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn defaults_and_settings_compare_and_swap(pool: PgPool) {
    let original = settings(&pool).await.unwrap();
    assert!(!original.enabled);
    assert!(!original.paused);
    assert!(original.allow_missing_discoverable && original.allow_missing_indexable);
    assert_eq!(
        add_source(&pool, "https://one.example", None)
            .await
            .unwrap(),
        Admission::Disabled
    );
    let mut next = original.clone();
    next.enabled = true;
    next.allow_missing_indexable = false;
    next.disabled_mechanisms = vec!["outbox".into(), "recommendations".into()];
    assert!(save_settings(&pool, &next).await.unwrap());
    assert!(!save_settings(&pool, &original).await.unwrap());
    let saved = settings(&pool).await.unwrap();
    assert_eq!(saved.revision, original.revision + 1);
    assert!(!saved.allow_missing_indexable);
    assert!(saved.allow_missing_discoverable);
    assert_eq!(saved.disabled_mechanisms, next.disabled_mechanisms);
    let mut invalid = saved.clone();
    invalid.disabled_mechanisms = vec!["typo".into()];
    assert!(save_settings(&pool, &invalid).await.is_err());
    assert_eq!(settings(&pool).await.unwrap(), saved);
}

#[sqlx::test(migrations = "./migrations")]
async fn source_admission_normalizes_coalesces_and_bounds_concurrent_insertions(pool: PgPool) {
    let mut config = enable(&pool).await;
    config.max_sources = 3;
    save_settings(&pool, &config).await.unwrap();
    let existing = added(
        add_source(&pool, "https://ONE.example:443", None)
            .await
            .unwrap(),
    );
    assert_eq!(
        add_source(&pool, "https://one.example/", None)
            .await
            .unwrap(),
        Admission::Existing(existing)
    );
    let start = Arc::new(Barrier::new(12));
    let mut tasks = JoinSet::new();
    for n in 0..12 {
        let pool = pool.clone();
        let start = start.clone();
        tasks.spawn(async move {
            start.wait().await;
            add_source(&pool, &format!("https://host{n}.example/"), None)
                .await
                .unwrap()
        });
    }
    let mut inserted = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Admission::Added(_) => inserted += 1,
            Admission::Capacity => (),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(inserted, 2);
    assert_eq!(
        sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM discovery_sources"#)
            .fetch_one(&pool)
            .await
            .unwrap(),
        3
    );
    let mut lower = settings(&pool).await.unwrap();
    lower.max_sources = 2;
    assert!(save_settings(&pool, &lower).await.is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn source_origin_and_introducer_caps_are_independent(pool: PgPool) {
    let mut config = enable(&pool).await;
    config.sources_per_origin = 2;
    config.sources_per_introducer = 2;
    save_settings(&pool, &config).await.unwrap();
    let intro = added(
        add_source(&pool, "https://seed.example/", None)
            .await
            .unwrap(),
    );
    added(
        add_source(&pool, "https://one.example/a", Some(intro))
            .await
            .unwrap(),
    );
    added(
        add_source(&pool, "https://one.example/b", None)
            .await
            .unwrap(),
    );
    assert_eq!(
        add_source(&pool, "https://one.example/c", Some(intro))
            .await
            .unwrap(),
        Admission::Capacity
    );
    added(
        add_source(&pool, "https://two.example/", Some(intro))
            .await
            .unwrap(),
    );
    assert_eq!(
        add_source(&pool, "https://three.example/", Some(intro))
            .await
            .unwrap(),
        Admission::Capacity
    );
    set_source_enabled(&pool, intro, false).await.unwrap();
    assert_eq!(
        add_source(&pool, "https://seed.example/", None)
            .await
            .unwrap(),
        Admission::Suppressed
    );
    assert_eq!(
        add_source(&pool, "https://four.example/", Some(intro))
            .await
            .unwrap(),
        Admission::Suppressed
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn job_flood_is_bounded_and_duplicates_do_not_change_provenance(pool: PgPool) {
    let mut config = enable(&pool).await;
    config.max_jobs = 4;
    save_settings(&pool, &config).await.unwrap();
    let source = verified_source(&pool, "https://one.example/", None).await;
    let start = Arc::new(Barrier::new(32));
    let mut tasks = JoinSet::new();
    for n in 0..32 {
        let pool = pool.clone();
        let start = start.clone();
        tasks.spawn(async move {
            start.wait().await;
            enqueue(
                &pool,
                source,
                &format!("https://one.example/{n}"),
                Operation::RefreshEntry,
                None,
            )
            .await
            .unwrap()
        });
    }
    let mut ids = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Admission::Added(id) => ids.push(id),
            Admission::Capacity => (),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(ids.len(), 4);
    let row = sqlx::query!("SELECT target_uri,id FROM discovery_jobs ORDER BY id LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        enqueue(
            &pool,
            source,
            &row.target_uri,
            Operation::RefreshEntry,
            Some(source)
        )
        .await
        .unwrap(),
        Admission::Existing(row.id)
    );
    assert_eq!(
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM discovery_jobs WHERE introduced_by IS NULL"#
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        4
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn job_caps_reject_mislabeled_origins_and_honor_introducer_limits(pool: PgPool) {
    let mut config = enable(&pool).await;
    config.jobs_per_origin = 2;
    config.jobs_per_introducer = 2;
    save_settings(&pool, &config).await.unwrap();
    let a = verified_source(&pool, "https://a.example/", None).await;
    let b = verified_source(&pool, "https://b.example/", None).await;
    let c = verified_source(&pool, "https://c.example/", None).await;
    added(
        enqueue(
            &pool,
            a,
            "https://a.example/1",
            Operation::RefreshEntry,
            Some(a),
        )
        .await
        .unwrap(),
    );
    added(
        enqueue(
            &pool,
            a,
            "https://a.example/2",
            Operation::RefreshEntry,
            Some(b),
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        enqueue(
            &pool,
            a,
            "https://a.example/3",
            Operation::RefreshEntry,
            Some(c)
        )
        .await
        .unwrap(),
        Admission::Capacity
    );
    added(
        enqueue(
            &pool,
            b,
            "https://b.example/1",
            Operation::RefreshEntry,
            Some(a),
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        enqueue(
            &pool,
            c,
            "https://c.example/1",
            Operation::RefreshEntry,
            Some(a)
        )
        .await
        .unwrap(),
        Admission::Capacity
    );
    assert!(
        enqueue(
            &pool,
            a,
            "https://b.example/else",
            Operation::RefreshEntry,
            None
        )
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn claims_are_concurrently_bounded_by_global_and_origin_capacity(pool: PgPool) {
    enable(&pool).await;
    let a = verified_source(&pool, "https://a.example/", None).await;
    let b = verified_source(&pool, "https://b.example/", None).await;
    let c = verified_source(&pool, "https://c.example/", None).await;
    for (source, uri) in [
        (a, "https://a.example/1"),
        (a, "https://a.example/2"),
        (b, "https://b.example/1"),
        (c, "https://c.example/1"),
    ] {
        added(
            enqueue(&pool, source, uri, Operation::RefreshEntry, None)
                .await
                .unwrap(),
        );
    }
    let (left, right) = tokio::join!(claim_due(&pool, "left"), claim_due(&pool, "right"));
    let mut leases = left.unwrap();
    leases.extend(right.unwrap());
    assert_eq!(leases.len(), 2);
    assert_ne!(leases[0].source_id, leases[1].source_id);
    assert!(claim_due(&pool, "third").await.unwrap().is_empty());
    assert_eq!(
        finish_job(&pool, &leases[0]).await.unwrap(),
        Completion::Committed
    );
    assert_eq!(claim_due(&pool, "third").await.unwrap().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_lease_and_retry_cursor_survive_worker_restart(pool: PgPool) {
    enable(&pool).await;
    let source = verified_source(&pool, "https://one.example/", None).await;
    let first = refresh(&pool, source, "https://one.example/actor").await;
    retry_job(&pool, &first, Some("next-page"), 0, "transient")
        .await
        .unwrap();
    let second = claim_due(&pool, "old-process").await.unwrap().remove(0);
    assert_eq!(second.attempts, 2);
    assert_eq!(second.cursor.as_deref(), Some("next-page"));
    sqlx::query!(
        "UPDATE discovery_jobs SET leased_until=clock_timestamp()-interval '1 second' WHERE id=$1",
        second.id
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(finish_job(&pool, &second).await.unwrap(), Completion::Stale);
    let third = claim_due(&pool, "new-process").await.unwrap().remove(0);
    assert_eq!(third.id, second.id);
    assert_ne!(third.lease_token, second.lease_token);
    assert_eq!(third.attempts, 3);
    assert_eq!(third.cursor, second.cursor);
    assert_eq!(
        finish_entry(&pool, &second, &facts(&second.target_uri))
            .await
            .unwrap(),
        Completion::Stale
    );
    assert_eq!(
        finish_entry(&pool, &third, &facts(&third.target_uri))
            .await
            .unwrap(),
        Completion::Committed
    );
    assert_eq!(
        super::source(&pool, source)
            .await
            .unwrap()
            .unwrap()
            .attempts_total,
        4
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn policy_changes_pause_and_source_changes_fence_old_results(pool: PgPool) {
    enable(&pool).await;
    let source = verified_source(&pool, "https://one.example/", None).await;
    let old = refresh(&pool, source, "https://one.example/actor").await;
    let mut current = settings(&pool).await.unwrap();
    current.paused = true;
    save_settings(&pool, &current).await.unwrap();
    assert_eq!(
        finish_entry(&pool, &old, &facts(&old.target_uri))
            .await
            .unwrap(),
        Completion::Stale
    );
    assert!(claim_due(&pool, "paused").await.unwrap().is_empty());
    let mut current = settings(&pool).await.unwrap();
    current.paused = false;
    current.allow_missing_discoverable = false;
    save_settings(&pool, &current).await.unwrap();
    let newer = claim_due(&pool, "resumed").await.unwrap().remove(0);
    assert_eq!(newer.attempts, 2);
    set_source_enabled(&pool, source, false).await.unwrap();
    set_source_enabled(&pool, source, true).await.unwrap();
    assert_eq!(
        finish_entry(&pool, &newer, &facts(&newer.target_uri))
            .await
            .unwrap(),
        Completion::Stale
    );
    let newest = claim_due(&pool, "newest").await.unwrap().remove(0);
    assert_eq!(
        finish_entry(&pool, &newest, &facts(&newest.target_uri))
            .await
            .unwrap(),
        Completion::Committed
    );
    let saved = entry(&pool, &newest.target_uri).await.unwrap().unwrap();
    assert_eq!(saved.state, "denied");
    assert!(saved.summary.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn unverified_sources_disabled_methods_and_cooldowns_prevent_claims(pool: PgPool) {
    enable(&pool).await;
    let source = added(
        add_source(&pool, "https://one.example/", None)
            .await
            .unwrap(),
    );
    added(
        enqueue(
            &pool,
            source,
            "https://one.example/actor",
            Operation::RefreshEntry,
            None,
        )
        .await
        .unwrap(),
    );
    assert!(claim_due(&pool, "test").await.unwrap().is_empty());
    enqueue(
        &pool,
        source,
        "https://one.example/",
        Operation::InspectSource,
        None,
    )
    .await
    .unwrap();
    let inspection = claim_due(&pool, "test").await.unwrap().remove(0);
    assert_eq!(inspection.operation, "inspect_source");
    let now = OffsetDateTime::now_utc();
    finish_source(
        &pool,
        &inspection,
        &VerifiedSource {
            canonical_uri: "https://one.example/",
            capabilities: &[],
            verified_at: now,
            valid_until: now + Duration::days(1),
        },
    )
    .await
    .unwrap();
    let mut config = settings(&pool).await.unwrap();
    config.disabled_mechanisms.push("metadata_cache".into());
    save_settings(&pool, &config).await.unwrap();
    assert!(claim_due(&pool, "test").await.unwrap().is_empty());
    let mut config = settings(&pool).await.unwrap();
    config.disabled_mechanisms.clear();
    save_settings(&pool, &config).await.unwrap();
    set_source_cooldown(&pool, source, now + Duration::hours(1), "rate_limited")
        .await
        .unwrap();
    assert!(claim_due(&pool, "test").await.unwrap().is_empty());
    set_source_cooldown(&pool, source, now - Duration::seconds(1), "elapsed")
        .await
        .unwrap();
    assert_eq!(claim_due(&pool, "test").await.unwrap().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn retries_backoff_and_expiry_are_durable_and_bounded(pool: PgPool) {
    let mut config = enable(&pool).await;
    config.max_attempts = 2;
    save_settings(&pool, &config).await.unwrap();
    let source = verified_source(&pool, "https://one.example/", None).await;
    let first = refresh(&pool, source, "https://one.example/actor").await;
    retry_job(&pool, &first, Some("cursor"), 3600, "retry")
        .await
        .unwrap();
    assert_eq!(
        enqueue(
            &pool,
            source,
            &first.target_uri,
            Operation::RefreshEntry,
            None
        )
        .await
        .unwrap(),
        Admission::Existing(first.id)
    );
    assert!(claim_due(&pool, "early").await.unwrap().is_empty());
    sqlx::query!(
        "UPDATE discovery_jobs SET run_at=clock_timestamp()-interval '1 second' WHERE id=$1",
        first.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let second = claim_due(&pool, "later").await.unwrap().remove(0);
    retry_job(&pool, &second, None, 0, "exhausted")
        .await
        .unwrap();
    assert!(claim_due(&pool, "again").await.unwrap().is_empty());
    added(
        enqueue(
            &pool,
            source,
            "https://one.example/expired",
            Operation::RefreshEntry,
            None,
        )
        .await
        .unwrap(),
    );
    sqlx::query!("UPDATE discovery_jobs SET expires_at=clock_timestamp()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let mut config = settings(&pool).await.unwrap();
    config.enabled = false;
    save_settings(&pool, &config).await.unwrap();
    assert_eq!(prune_jobs(&pool).await.unwrap(), 1);
    assert_eq!(
        super::source(&pool, source)
            .await
            .unwrap()
            .unwrap()
            .attempts_total,
        3
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_catalogue_writes_cannot_exceed_the_global_cap(pool: PgPool) {
    let mut config = enable(&pool).await;
    config.max_entries = 2;
    config.max_inflight = 8;
    save_settings(&pool, &config).await.unwrap();
    let mut targets = Vec::new();
    for n in 0..4 {
        let base = format!("https://host{n}.example/");
        let source = verified_source(&pool, &base, None).await;
        targets.push((source, format!("{base}actor")));
    }
    for (source, target) in &targets {
        enqueue(&pool, *source, target, Operation::RefreshEntry, None)
            .await
            .unwrap();
    }
    let leases = claim_due(&pool, "parallel").await.unwrap();
    assert_eq!(leases.len(), 4);
    let start = Arc::new(Barrier::new(4));
    let mut tasks = JoinSet::new();
    for lease in leases {
        let pool = pool.clone();
        let start = start.clone();
        tasks.spawn(async move {
            start.wait().await;
            finish_entry(&pool, &lease, &facts(&lease.target_uri))
                .await
                .unwrap()
        });
    }
    let mut saved = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Completion::Committed => saved += 1,
            Completion::Capacity => (),
            other @ Completion::Stale => panic!("{other:?}"),
        }
    }
    assert_eq!(saved, 2);
    assert_eq!(
        sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM discovery_entries"#)
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn entry_origin_and_introducer_limits_include_withdrawal_guards(pool: PgPool) {
    let mut config = enable(&pool).await;
    config.entries_per_origin = 1;
    config.entries_per_introducer = 2;
    save_settings(&pool, &config).await.unwrap();
    let seed = verified_source(&pool, "https://seed.example/", None).await;
    let a = verified_source(&pool, "https://a.example/", Some(seed)).await;
    let b = verified_source(&pool, "https://b.example/", Some(seed)).await;
    let c = verified_source(&pool, "https://c.example/", Some(seed)).await;
    for (source, uri) in [(a, "https://a.example/one"), (b, "https://b.example/one")] {
        let lease = refresh(&pool, source, uri).await;
        assert_eq!(
            finish_entry(&pool, &lease, &facts(uri)).await.unwrap(),
            Completion::Committed
        );
    }
    invalidate_entry(
        &pool,
        "https://a.example/one",
        Invalidation::Withdrawn,
        "deleted",
    )
    .await
    .unwrap();
    let lease = refresh(&pool, a, "https://a.example/two").await;
    assert_eq!(
        finish_entry(&pool, &lease, &facts(&lease.target_uri))
            .await
            .unwrap(),
        Completion::Capacity
    );
    finish_job(&pool, &lease).await.unwrap();
    let lease = refresh(&pool, c, "https://c.example/one").await;
    assert_eq!(
        finish_entry(&pool, &lease, &facts(&lease.target_uri))
            .await
            .unwrap(),
        Completion::Capacity
    );
    let guard = entry(&pool, "https://a.example/one")
        .await
        .unwrap()
        .unwrap();
    assert!(guard.title.is_none() && guard.summary.is_none());
    assert_eq!(guard.state, "withdrawn");
}

#[sqlx::test(migrations = "./migrations")]
async fn withdrawal_and_denial_fence_refreshes_without_resurrection(pool: PgPool) {
    enable(&pool).await;
    let source = verified_source(&pool, "https://one.example/", None).await;
    let uri = "https://one.example/actor";
    let initial = refresh(&pool, source, uri).await;
    finish_entry(&pool, &initial, &facts(uri)).await.unwrap();
    let old = refresh(&pool, source, uri).await;
    invalidate_entry(&pool, uri, Invalidation::Denied, "opt_out")
        .await
        .unwrap();
    assert_eq!(
        finish_entry(&pool, &old, &facts(uri)).await.unwrap(),
        Completion::Stale
    );
    assert!(entry(&pool, uri).await.unwrap().unwrap().summary.is_none());
    let fresh = refresh(&pool, source, uri).await;
    assert_eq!(
        finish_entry(&pool, &fresh, &facts(uri)).await.unwrap(),
        Completion::Committed
    );
    let late = refresh(&pool, source, uri).await;
    invalidate_entry(&pool, uri, Invalidation::Withdrawn, "deleted")
        .await
        .unwrap();
    invalidate_entry(&pool, uri, Invalidation::Denied, "old_denial")
        .await
        .unwrap();
    assert_eq!(
        finish_entry(&pool, &late, &facts(uri)).await.unwrap(),
        Completion::Stale
    );
    assert_eq!(
        enqueue(&pool, source, uri, Operation::RefreshEntry, None)
            .await
            .unwrap(),
        Admission::Suppressed
    );
    assert_eq!(entry(&pool, uri).await.unwrap().unwrap().state, "withdrawn");
}

#[sqlx::test(migrations = "./migrations")]
async fn first_fetch_withdrawal_guards_and_competing_sources_cannot_overwrite_entries(
    pool: PgPool,
) {
    enable(&pool).await;
    let a = verified_source(&pool, "https://one.example/a", None).await;
    let b = verified_source(&pool, "https://one.example/b", None).await;
    let uri = "https://one.example/actor";
    let old = refresh(&pool, a, uri).await;
    invalidate_entry(&pool, uri, Invalidation::Withdrawn, "deleted_before_insert")
        .await
        .unwrap();
    assert_eq!(
        finish_entry(&pool, &old, &facts(uri)).await.unwrap(),
        Completion::Stale
    );
    assert_eq!(
        enqueue(&pool, b, uri, Operation::RefreshEntry, None)
            .await
            .unwrap(),
        Admission::Suppressed
    );
    sqlx::query!("UPDATE discovery_jobs SET expires_at=clock_timestamp()-interval '1 second',attempts=100 WHERE id=$1",old.id)
        .execute(&pool).await.unwrap();
    assert_eq!(prune_jobs(&pool).await.unwrap(), 0);
    invalidate_entry(&pool, uri, Invalidation::Denied, "late_denial")
        .await
        .unwrap();
    assert!(claim_due(&pool, "guard").await.unwrap().is_empty());
    assert_eq!(
        enqueue(&pool, a, uri, Operation::RefreshEntry, None)
            .await
            .unwrap(),
        Admission::Suppressed
    );
    let uri = "https://one.example/another";
    let fresh = refresh(&pool, a, uri).await;
    finish_entry(&pool, &fresh, &facts(uri)).await.unwrap();
    let competing = refresh(&pool, b, uri).await;
    assert_eq!(
        finish_entry(&pool, &competing, &facts(uri)).await.unwrap(),
        Completion::Stale
    );
    assert_eq!(entry(&pool, uri).await.unwrap().unwrap().source_id, a);
}

#[sqlx::test(migrations = "./migrations")]
async fn entry_refusal_clears_payload_and_missing_signals_round_trip(pool: PgPool) {
    enable(&pool).await;
    let source = verified_source(&pool, "https://one.example/", None).await;
    let uri = "https://one.example/actor";
    let lease = refresh(&pool, source, uri).await;
    finish_entry(&pool, &lease, &facts(uri)).await.unwrap();
    let saved = entry(&pool, uri).await.unwrap().unwrap();
    assert_eq!(saved.discoverable, "missing");
    assert_eq!(saved.state, "active");
    let lease = refresh(&pool, source, uri).await;
    let mut refused = facts(uri);
    refused.discoverable = "deny";
    finish_entry(&pool, &lease, &refused).await.unwrap();
    let saved = entry(&pool, uri).await.unwrap().unwrap();
    assert_eq!(saved.state, "denied");
    assert_eq!(saved.discoverable, "deny");
    assert!(saved.title.is_none() && saved.summary.is_none() && saved.language.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn field_bounds_invalid_urls_and_old_evidence_cannot_write(pool: PgPool) {
    enable(&pool).await;
    for uri in [
        "file:///etc/passwd",
        "https://user:pass@example.org/",
        "https://example.org/#fragment",
    ] {
        assert!(add_source(&pool, uri, None).await.is_err());
    }
    assert!(
        add_source(
            &pool,
            &format!("https://example.org/{}", "x".repeat(2048)),
            None
        )
        .await
        .is_err()
    );
    let source = verified_source(&pool, "https://one.example/", None).await;
    let lease = refresh(&pool, source, "https://one.example/actor").await;
    let long = "x".repeat(8193);
    let mut bad = facts(&lease.target_uri);
    bad.summary = &long;
    assert!(finish_entry(&pool, &lease, &bad).await.is_err());
    let mut old = facts(&lease.target_uri);
    old.valid_until = old.verified_at - Duration::seconds(1);
    assert!(finish_entry(&pool, &lease, &old).await.is_err());
    let mut replay = facts(&lease.target_uri);
    replay.verified_at = OffsetDateTime::now_utc() - Duration::hours(1);
    assert_eq!(
        finish_entry(&pool, &lease, &replay).await.unwrap(),
        Completion::Stale
    );
    assert!(entry(&pool, &lease.target_uri).await.unwrap().is_none());
    assert_eq!(
        finish_entry(&pool, &lease, &facts(&lease.target_uri))
            .await
            .unwrap(),
        Completion::Committed
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn exploration_retains_a_request_allowance_under_refresh_pressure(pool: PgPool) {
    let mut settings = enable(&pool).await;
    settings.requests_per_day = 5;
    save_settings(&pool, &settings).await.unwrap();
    let source = verified_source(&pool, "https://refresh.example/api/v1/directory", None).await;
    let uri = "https://refresh.example/users/alice";
    let first = refresh(&pool, source, uri).await;
    finish_entry(&pool, &first, &facts(uri)).await.unwrap();
    let maintenance = refresh(&pool, source, uri).await;
    assert!(!maintenance.exploration);
    let other = added(
        add_source(&pool, "https://new.example/api/v1/directory", None)
            .await
            .unwrap(),
    );
    enqueue(
        &pool,
        other,
        "https://new.example/api/v1/directory",
        Operation::InspectSource,
        None,
    )
    .await
    .unwrap();
    for _ in 0..4 {
        let Reservation::Reserved(id) = reserve_request(&pool, &maintenance, uri).await.unwrap()
        else {
            panic!("refresh allowance")
        };
        complete_request(&pool, id, 10).await.unwrap();
    }
    assert_eq!(
        reserve_request(&pool, &maintenance, uri).await.unwrap(),
        Reservation::Budget
    );
    let explorer = claim_due(&pool, "restarted-explorer")
        .await
        .unwrap()
        .remove(0);
    assert!(explorer.exploration);
    assert!(matches!(
        reserve_request(&pool, &explorer, &explorer.target_uri)
            .await
            .unwrap(),
        Reservation::Reserved(_)
    ));
    assert_eq!(diagnostics(&pool).await.unwrap().requests_24h, 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn peer_capacity_leaves_room_for_roots_and_rotates_only_safe_leaves(pool: PgPool) {
    let mut settings = enable(&pool).await;
    settings.max_sources = 5;
    save_settings(&pool, &settings).await.unwrap();
    let root = added(
        add_source(&pool, "https://root.example/api/v1/directory", None)
            .await
            .unwrap(),
    );
    let mut peers = Vec::new();
    for n in 0..3 {
        peers.push(added(
            add_source_with_provenance(
                &pool,
                &format!("https://peer{n}.example/api/v1/directory"),
                Some(root),
                "peer",
            )
            .await
            .unwrap(),
        ));
    }
    assert_eq!(
        add_source_with_provenance(
            &pool,
            "https://crowded.example/api/v1/directory",
            Some(root),
            "peer"
        )
        .await
        .unwrap(),
        Admission::Capacity
    );
    assert!(matches!(
        add_source(&pool, "https://independent.example/api/v1/directory", None)
            .await
            .unwrap(),
        Admission::Added(_)
    ));
    sqlx::query(
        "UPDATE discovery_sources SET created_at=now()-interval '2 days' WHERE provenance='peer'",
    )
    .execute(&pool)
    .await
    .unwrap();
    set_source_enabled(&pool, peers[0], false).await.unwrap();
    assert!(matches!(
        add_source_with_provenance(
            &pool,
            "https://replacement.example/api/v1/directory",
            None,
            "import"
        )
        .await
        .unwrap(),
        Admission::Added(_)
    ));
    assert!(source(&pool, peers[0]).await.unwrap().is_some());
    assert!(source(&pool, root).await.unwrap().is_some());
    assert_eq!(diagnostics(&pool).await.unwrap().sources, 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn peer_failure_does_not_poison_a_working_directory(pool: PgPool) {
    enable(&pool).await;
    let source = verified_source(&pool, "https://one.example/api/v1/directory", None).await;
    schedule_peer_exploration(&pool).await.unwrap();
    let lease = claim_due_for(&pool, "peers", &[Operation::ListPeers])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        fail_peer_page(&pool, &lease, "endpoint_unsupported")
            .await
            .unwrap(),
        Completion::Committed
    );
    let source = super::source(&pool, source).await.unwrap().unwrap();
    assert!(source.cooldown_until.is_none());
    assert!(source.valid_until.unwrap() > OffsetDateTime::now_utc());
    assert_eq!(source.peer_error.as_deref(), Some("endpoint_unsupported"));
    assert!(source.peer_next_at > OffsetDateTime::now_utc() + Duration::hours(23));
    schedule_peer_exploration(&pool).await.unwrap();
    assert!(
        claim_due_for(&pool, "restart", &[Operation::ListPeers])
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn explicit_import_adopts_a_peer_without_clearing_its_backoff(pool: PgPool) {
    enable(&pool).await;
    let root = added(
        add_source(&pool, "https://root.example/api/v1/directory", None)
            .await
            .unwrap(),
    );
    let endpoint = "https://independent.example/api/v1/directory";
    let peer = added(
        add_source_with_provenance(&pool, endpoint, Some(root), "peer")
            .await
            .unwrap(),
    );
    set_source_enabled(&pool, root, false).await.unwrap();
    set_source_cooldown(
        &pool,
        peer,
        OffsetDateTime::now_utc() + Duration::hours(1),
        "rate_limited",
    )
    .await
    .unwrap();
    let result = import_sources(&pool, &[endpoint.into()], "import", true)
        .await
        .unwrap();
    assert_eq!(result.existing, 1);
    let source = source(&pool, peer).await.unwrap().unwrap();
    assert_eq!(source.introduced_by, None);
    assert_eq!(source.depth, 0);
    assert_eq!(source.provenance, "import");
    assert!(source.cooldown_until.unwrap() > OffsetDateTime::now_utc());
    assert!(source.verified_at.is_none());
    set_source_enabled(&pool, peer, false).await.unwrap();
    assert_eq!(
        import_sources(&pool, &[endpoint.into()], "import", false)
            .await
            .unwrap()
            .refused,
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn catalogue_rotation_keeps_withdrawals_and_persistent_positions(pool: PgPool) {
    let mut settings = enable(&pool).await;
    settings.max_entries = 2;
    save_settings(&pool, &settings).await.unwrap();
    let source = verified_source(&pool, "https://one.example/api/v1/directory", None).await;
    let first = "https://one.example/users/first";
    let second = "https://one.example/users/second";
    let third = "https://one.example/users/third";
    let lease = refresh(&pool, source, first).await;
    finish_entry(&pool, &lease, &facts(first)).await.unwrap();
    let lease = refresh(&pool, source, second).await;
    finish_entry(&pool, &lease, &facts(second)).await.unwrap();
    let ordinal = entry(&pool, second).await.unwrap().unwrap().browse_ordinal;
    invalidate_entry(&pool, first, Invalidation::Withdrawn, "deleted")
        .await
        .unwrap();
    sqlx::query("UPDATE discovery_entries SET verified_at=now()-interval '13 hours'")
        .execute(&pool)
        .await
        .unwrap();
    let lease = refresh(&pool, source, third).await;
    assert_eq!(
        finish_entry(&pool, &lease, &facts(third)).await.unwrap(),
        Completion::Committed
    );
    assert_eq!(
        entry(&pool, first).await.unwrap().unwrap().state,
        "withdrawn"
    );
    assert!(entry(&pool, second).await.unwrap().is_none());
    assert!(entry(&pool, third).await.unwrap().unwrap().browse_ordinal > ordinal);
    assert_eq!(diagnostics(&pool).await.unwrap().entries, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn discovery_topics_commit_replace_and_withdraw_with_their_entry(pool: PgPool) {
    enable(&pool).await;
    let source = verified_source(&pool, "https://topics.example/directory", None).await;
    let uri = "https://topics.example/users/alice";
    let lease = refresh(&pool, source, uri).await;
    assert_eq!(
        finish_entry_with_topics(&pool, &lease, &facts(uri), &["garden".into()])
            .await
            .unwrap(),
        Completion::Committed
    );
    let id = entry(&pool, uri).await.unwrap().unwrap().id;
    assert_eq!(entry_topics(&pool, id).await.unwrap(), ["garden"]);
    let lease = refresh(&pool, source, uri).await;
    assert_eq!(
        finish_entry_with_topics(&pool, &lease, &facts(uri), &["repair".into()])
            .await
            .unwrap(),
        Completion::Committed
    );
    assert_eq!(entry_topics(&pool, id).await.unwrap(), ["repair"]);
    let lease = refresh(&pool, source, uri).await;
    invalidate_entry(&pool, uri, Invalidation::Withdrawn, "deleted")
        .await
        .unwrap();
    assert_eq!(
        finish_entry_with_topics(&pool, &lease, &facts(uri), &["garden".into()])
            .await
            .unwrap(),
        Completion::Stale
    );
    assert!(entry_topics(&pool, id).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn discovery_topics_denied_facts_never_retain_edges(pool: PgPool) {
    enable(&pool).await;
    let source = verified_source(&pool, "https://topics.example/directory", None).await;
    let uri = "https://topics.example/users/alice";
    let lease = refresh(&pool, source, uri).await;
    let mut facts = facts(uri);
    facts.discoverable = "deny";
    assert_eq!(
        finish_entry_with_topics(&pool, &lease, &facts, &["garden".into()])
            .await
            .unwrap(),
        Completion::Committed
    );
    let id = entry(&pool, uri).await.unwrap().unwrap().id;
    assert!(entry_topics(&pool, id).await.unwrap().is_empty());
}
