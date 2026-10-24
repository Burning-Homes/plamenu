mod common;
#[allow(
    dead_code,
    reason = "exercise real cleanup without seeding the staging-scale dataset"
)]
#[path = "../benches/hot_paths/seed.rs"]
mod seed;

use plamenu_db::PgPool;

async fn personas(pool: &PgPool) -> (i64, i64) {
    let writer = common::create_local_account(pool, "bench_writer", "Writer").await;
    let popular = common::create_local_account(pool, "bench_popular", "Popular").await;
    seed::reset_run_residue(pool, writer.id, popular.id).await;
    sqlx::raw_sql("CREATE TABLE bench_seed (key text PRIMARY KEY, value text NOT NULL)")
        .execute(pool)
        .await
        .unwrap();
    (writer.id, popular.id)
}

async fn remote(pool: &PgPool, domain: &str) {
    let account = plamenu_db::id::next();
    sqlx::query("INSERT INTO accounts (id,username,domain,uri,public_key,inbox_url)
        VALUES ($1,'fixture',$2,'https://' || $2 || '/users/fixture','','https://' || $2 || '/inbox')")
        .bind(account).bind(domain).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO statuses (id,account_id,uri,content)
        VALUES ($1,$2,'https://' || $3 || '/statuses/fixture','fixture')",
    )
    .bind(plamenu_db::id::next())
    .bind(account)
    .bind(domain)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn reset_removes_only_owned_retained_identities_and_is_repeatable(pool: PgPool) {
    let (writer, popular) = personas(&pool).await;
    remote(&pool, "d0.bench.invalid").await;
    remote(&pool, "bench-peer.invalid.evil").await;
    sqlx::query(
        "INSERT INTO discovery_identity (canonical_uri,kind,outcome)
        VALUES ('https://d0.bench.invalid/statuses/old-deleted','post','retired')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO discovery_aliases (alias,canonical_uri)
        VALUES ('keep','https://d0.bench.invalid/users/fixture')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let before: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(i)::text FROM discovery_identity i ORDER BY canonical_uri",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    seed::record_dataset_baseline(&pool).await;
    for domain in [
        "bench-peer.invalid",
        "bench-purge.invalid",
        "bench-search.invalid",
    ] {
        remote(&pool, domain).await;
    }
    // A purge/delete may already have removed the original account or status.
    sqlx::query(
        "INSERT INTO discovery_identity (canonical_uri,kind,outcome)
        VALUES ('https://bench-purge.invalid/users/already-consumed','profile','retired')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO discovery_aliases (alias,canonical_uri)
        VALUES ('remove','https://bench-peer.invalid/users/fixture')",
    )
    .execute(&pool)
    .await
    .unwrap();
    for _ in 0..2 {
        seed::reset_run_residue(&pool, writer, popular).await;
        seed::assert_dataset_unchanged(&pool).await;
        let after: Vec<String> = sqlx::query_scalar(
            "SELECT row_to_json(i)::text FROM discovery_identity i ORDER BY canonical_uri",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            after, before,
            "seeded and unrelated identity metadata must not change"
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn reset_does_not_hide_unrelated_dataset_drift(pool: PgPool) {
    let (writer, popular) = personas(&pool).await;
    seed::record_dataset_baseline(&pool).await;
    sqlx::query(
        "INSERT INTO discovery_identity (canonical_uri,kind)
        VALUES ('https://unrelated.invalid/drift','profile')",
    )
    .execute(&pool)
    .await
    .unwrap();
    seed::reset_run_residue(&pool, writer, popular).await;
    let panic = tokio::spawn(async move { seed::assert_dataset_unchanged(&pool).await })
        .await
        .expect_err("unrelated drift must still fail the benchmark")
        .into_panic();
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .contains("discovery_identity: 0 -> 1")
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn missing_pristine_baseline_cannot_disable_drift_checks(pool: PgPool) {
    personas(&pool).await;
    let panic = tokio::spawn(async move { seed::assert_dataset_unchanged(&pool).await })
        .await
        .expect_err("missing baseline must prevent benchmarking")
        .into_panic();
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .contains("no pristine dataset baseline")
    );
}
