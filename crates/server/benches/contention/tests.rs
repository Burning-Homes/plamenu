use super::*;
use axum::routing::{get, post};

async fn mixed_report(pool: &PgPool, write_status: StatusCode, read_status: StatusCode) -> Report {
    let router = Router::new()
        .route(
            "/api/v1/statuses",
            post(move || async move { write_status }),
        )
        .route(
            "/api/v1/timelines/home",
            get(move || async move { read_status }),
        );
    let counter = Arc::new(AtomicU64::new(0));
    let report = mixed_scenario(pool, &router, "writer", "reader", &counter, 0, 4, 3, 2, 1).await;
    assert_eq!(
        counter.load(Ordering::Relaxed),
        6,
        "every writer must finish"
    );
    report
}

#[sqlx::test(migrations = "../db/migrations")]
async fn failed_writes_fail_the_mixed_scenario_and_its_record(pool: PgPool) {
    for status in [StatusCode::INTERNAL_SERVER_ERROR, StatusCode::CONFLICT] {
        let report = mixed_report(&pool, status, StatusCode::OK).await;
        assert!(
            report.failed(0.0),
            "failed writes must fail even with no timing budget"
        );
        assert_eq!(report.failures, vec![status; 6]);
        assert_eq!(report.requests, 4, "latency samples must remain read-only");
        assert_eq!(report.as_json(0.0)["failures"], 6);
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn successful_writes_do_not_become_read_samples(pool: PgPool) {
    let report = mixed_report(&pool, StatusCode::CREATED, StatusCode::OK).await;
    assert!(!report.failed(0.0));
    assert_eq!(report.requests, 4);
    assert_eq!(report.as_json(0.0)["requests"], 4);
    assert_eq!(report.as_json(0.0)["failures"], 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn read_and_write_failures_are_both_counted(pool: PgPool) {
    let report = mixed_report(&pool, StatusCode::CONFLICT, StatusCode::SERVICE_UNAVAILABLE).await;
    assert!(report.failed(0.0));
    assert_eq!(report.requests, 0);
    assert_eq!(
        report.failures.len(),
        11,
        "one baseline read, four concurrent reads, six writes"
    );
    assert_eq!(report.as_json(0.0)["failures"], 11);
}
