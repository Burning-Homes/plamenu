mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::auth::hash_password;
use plamenu::{AppState, notification_config::NotificationsConfig, notification_mail, webmentions};
use plamenu_db::{
    PgPool, email, id, marker, notification,
    notification_delivery::{self, Preference},
    notification_read,
    status::{self, NewLocalStatus},
    user,
};
use std::sync::Arc;
use time::OffsetDateTime;
async fn fixture(pool: PgPool) -> (AppState, i64, i64, i64) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let actor = create_local_account(&pool, "bob", "Bob").await.id;
    let uid = user::create(
        &pool,
        owner,
        Some("alice@example.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap()
    .id;
    (
        test_state_with(pool, StubFederation::with_actors([])),
        owner,
        uid,
        actor,
    )
}
#[test]
fn notification_config_has_reference_defaults_and_rejects_invalid_days_and_utc_hours() {
    let defaults = NotificationsConfig::default();
    assert_eq!(
        (
            defaults.retention_days,
            defaults.purge_hour,
            defaults.digest_hour
        ),
        (90, 3, 8)
    );
    for s in [
        "retention_days=0",
        "purge_hour=24",
        "digest_hour=24",
        "retention_days=365001",
    ] {
        let c: NotificationsConfig = toml::from_str(s).unwrap();
        assert!(c.validate().is_err());
    }
    let c: NotificationsConfig =
        toml::from_str("retention_days=12\npurge_hour=23\ndigest_hour=0").unwrap();
    let c = c.validate().unwrap();
    assert_eq!((c.retention_days, c.purge_hour, c.digest_hour), (12, 23, 0));
}
#[sqlx::test(migrations = "../db/migrations")]
async fn retention_preserves_unseen_overrides_recent_rows_and_disabled_delivery_kinds(
    pool: PgPool,
) {
    let (state, owner, uid, actor) = fixture(pool).await;
    for _ in 0..5 {
        notification::create(&state.pool, owner, actor, "follow", None)
            .await
            .unwrap();
    }
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM notifications WHERE account_id=$1 ORDER BY id")
            .bind(owner)
            .fetch_all(&state.pool)
            .await
            .unwrap();
    sqlx::query("UPDATE notifications SET created_at=now()-interval '100 days' WHERE account_id=$1 AND id<>$2").bind(owner).bind(ids[3]).execute(&state.pool).await.unwrap();
    marker::advance(&state.pool, uid, "notifications", ids[1])
        .await
        .unwrap();
    notification_read::set_read(&state.pool, owner, ids[0], false)
        .await
        .unwrap();
    notification_read::set_read(&state.pool, owner, ids[2], true)
        .await
        .unwrap();
    notification_delivery::update(
        &state.pool,
        owner,
        &[Preference {
            kind: "follow".into(),
            in_app: false,
            email: false,
            email_digest: false,
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        notification_read::purge_seen(&state.pool, 90)
            .await
            .unwrap(),
        2
    );
    let remaining: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM notifications WHERE account_id=$1 ORDER BY id")
            .bind(owner)
            .fetch_all(&state.pool)
            .await
            .unwrap();
    assert_eq!(remaining, vec![ids[0], ids[3], ids[4]]);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn retention_is_bounded_and_repeated_ticks_drain_only_old_seen_rows(pool: PgPool) {
    let (state, owner, uid, actor) = fixture(pool).await;
    let base = id::next();
    sqlx::query("INSERT INTO notifications(id,account_id,from_account_id,kind,reasons,created_at) SELECT $1+g,$2,$3,'follow',ARRAY['follow'],now()-interval '100 days' FROM generate_series(1,2001) g").bind(base).bind(owner).bind(actor).execute(&state.pool).await.unwrap();
    marker::advance(&state.pool, uid, "notifications", base + 2001)
        .await
        .unwrap();
    assert_eq!(
        notification_read::purge_seen(&state.pool, 90)
            .await
            .unwrap(),
        2000
    );
    assert_eq!(
        notification_read::purge_seen(&state.pool, 90)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        notification_read::purge_seen(&state.pool, 90)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn retention_keeps_private_and_website_mentions_and_reverification_does_not_renotify(
    pool: PgPool,
) {
    let (state, owner, uid, actor) = fixture(pool).await;
    let note = status::create_local(
        &state.pool,
        NewLocalStatus::new(actor, "<p>Private mention</p>", "direct", None),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO status_mentions(status_id,account_id) VALUES($1,$2)")
        .bind(note.id)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    notification::create(&state.pool, owner, actor, "mention", Some(note.id))
        .await
        .unwrap();
    let target = status::create_local(
        &state.pool,
        NewLocalStatus::new(owner, "<p>Website target</p>", "public", None),
    )
    .await
    .unwrap();
    let target = format!("https://{}/@alice/{}", state.config.domain, target.id);
    let source = "https://website.example/review";
    let stub = StubFederation::with_actors([]);
    stub.mention_pages.lock().unwrap().insert(source.into(),plamenu_federation::MentionPage{final_url:source.into(),content_type:"text/html".into(),links:Vec::new(),body:format!("<article class=h-entry><p class=p-summary>Website mention</p><a href='{target}'>Target</a></article>")});
    let state = test_state_with(state.pool.clone(), stub);
    let mention = webmentions::receive(&state, source, &target).await.unwrap();
    webmentions::worker::run(&state, mention).await.unwrap();
    notification_read::mark_all_read(&state.pool, uid, owner)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE notifications SET created_at=now()-interval '100 days' WHERE account_id=$1",
    )
    .bind(owner)
    .execute(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        notification_read::purge_seen(&state.pool, 90)
            .await
            .unwrap(),
        2
    );
    let archive = plamenu::mentions::page(&state, owner, &plamenu::mentions::Query::default())
        .await
        .unwrap();
    assert_eq!(archive.items.len(), 2);
    webmentions::receive(&state, source, &target).await.unwrap();
    webmentions::worker::run(&state, mention).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications WHERE account_id=$1")
        .bind(owner)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn digest_clock_and_initial_eligibility_use_the_configured_utc_hour(pool: PgPool) {
    let (mut state, owner, _, actor) = fixture(pool).await;
    let config = Arc::make_mut(&mut state.config);
    config.notifications.digest_hour = 0;
    config.smtp = Some(plamenu::config::SmtpConfig {
        server: "127.0.0.1".into(),
        port: 1,
        login: None,
        password: None,
        from_address: "notifications@example.test".into(),
        ssl: false,
        starttls: plamenu::config::Starttls::Never,
    });
    notification_delivery::update(
        &state.pool,
        owner,
        &[Preference {
            kind: "follow".into(),
            in_app: true,
            email: false,
            email_digest: true,
        }],
    )
    .await
    .unwrap();
    notification::create(&state.pool, owner, actor, "follow", None)
        .await
        .unwrap();
    assert_eq!(notification_mail::enqueue_due(&state).await.unwrap(), 1);
    let job = email::claim_due(&state.pool, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let prepared = notification_mail::prepare(&state, &job)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    notification_mail::finish(&state, &job, true, &prepared.notification_ids)
        .await
        .unwrap();
    let next: OffsetDateTime = sqlx::query_scalar(
        "SELECT next_digest_at FROM notification_digest_clock WHERE account_id=$1",
    )
    .bind(owner)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(next.hour(), 0);
    assert!(next > OffsetDateTime::now_utc());
}
