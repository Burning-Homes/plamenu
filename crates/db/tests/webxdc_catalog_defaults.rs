use plamenu_db::{MIGRATOR, PgPool, webxdc};

const DEFAULT_FEED_URL: &str = "https://apps.testrun.org/xdcget-lock.json";

#[sqlx::test(migrations = false)]
async fn upgrade_preserves_an_existing_default_catalog_source(pool: PgPool) {
    // Keep the upgrade case independent of the development runner's optional
    // template1 seed, just like the other migration-specific tests.
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
        .execute(&pool)
        .await
        .unwrap();
    let before_default = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version < 79)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before_default.run(&pool).await.unwrap();

    let existing = webxdc::create_catalog_source(&pool, "Operator catalog", DEFAULT_FEED_URL)
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let sources = webxdc::catalog_sources(&pool).await.unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].id, existing.id);
    assert_eq!(sources[0].name, "Operator catalog");
    assert_eq!(sources[0].feed_url, DEFAULT_FEED_URL);
}
