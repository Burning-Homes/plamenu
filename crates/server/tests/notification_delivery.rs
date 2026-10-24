use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    auth::{generate_secret, hash_password, hash_secret},
    config::{SmtpConfig, Starttls},
    mailer, notification_mail,
};
use plamenu_db::{
    PgPool, email, notification,
    notification_delivery::{self, Preference},
    notification_read, oauth,
    status::{self, NewLocalStatus},
    user,
};
use plamenu_test_support as common;
use serde_json::json;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tower::ServiceExt;
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
fn smtp(port: u16) -> SmtpConfig {
    SmtpConfig {
        server: "127.0.0.1".into(),
        port,
        login: None,
        password: None,
        from_address: "Plamenu <notifications@example.test>".into(),
        ssl: false,
        starttls: Starttls::Never,
    }
}
fn pref(kind: &str, in_app: bool, email: bool, digest: bool) -> Preference {
    Preference {
        kind: kind.into(),
        in_app,
        email,
        email_digest: digest,
    }
}
macro_rules! count {
    ($pool:expr,$sql:literal) => {
        sqlx::query_scalar::<_, i64>($sql)
            .fetch_one($pool)
            .await
            .unwrap()
    };
}
async fn capture(accepted: bool) -> (u16, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        stream
            .get_mut()
            .write_all(b"220 localhost fixture\r\n")
            .await
            .unwrap();
        loop {
            let mut line = String::new();
            assert!(stream.read_line(&mut line).await.unwrap() > 0);
            if line.starts_with("DATA") {
                stream
                    .get_mut()
                    .write_all(b"354 Send data\r\n")
                    .await
                    .unwrap();
                let mut body = String::new();
                loop {
                    let mut line = String::new();
                    assert!(stream.read_line(&mut line).await.unwrap() > 0);
                    if line == ".\r\n" {
                        break;
                    }
                    body.push_str(&line);
                }
                stream
                    .get_mut()
                    .write_all(if accepted {
                        b"250 Accepted\r\n"
                    } else {
                        b"451 Try later\r\n"
                    })
                    .await
                    .unwrap();
                return body;
            }
            stream
                .get_mut()
                .write_all(b"250 localhost\r\n")
                .await
                .unwrap();
        }
    });
    (port, task)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn delivery_defaults_and_coalesced_reasons_preserve_enabled_in_app_and_permanent_mentions(
    pool: PgPool,
) {
    let (state, owner, uid, actor) = fixture(pool).await;
    let prefs = notification_delivery::preferences(&state.pool, owner)
        .await
        .unwrap();
    assert_eq!(prefs.len(), notification_delivery::TYPES.len());
    assert!(
        prefs
            .iter()
            .all(|p| p.in_app && !p.email && !p.email_digest)
    );
    let s = status::create_local(
        &state.pool,
        NewLocalStatus::new(actor, "<p>Mentioned</p>", "public", None),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO status_mentions(status_id,account_id) VALUES($1,$2)")
        .bind(s.id)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    notification::create_post_notifications_many(
        &state.pool,
        &[notification::PostNotification {
            account_id: owner,
            mention: true,
            quote: false,
            status: true,
        }],
        actor,
        s.id,
    )
    .await
    .unwrap();
    notification_delivery::update(&state.pool, owner, &[pref("mention", false, false, false)])
        .await
        .unwrap();
    let rows = notification::list(
        &state.pool,
        owner,
        None,
        None,
        None,
        notification::NotificationFilter::default(),
        40,
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "status");
    assert_eq!(
        notification::find_by_id(&state.pool, owner, rows[0].id)
            .await
            .unwrap()
            .unwrap()
            .kind,
        "status"
    );
    let counts = notification::lemmy_unread_counts(&state.pool, uid, owner)
        .await
        .unwrap();
    assert_eq!(counts.mentions, 0);

    assert!(
        notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    notification_delivery::update(&state.pool, owner, &[pref("status", false, false, false)])
        .await
        .unwrap();
    assert!(
        notification::list(
            &state.pool,
            owner,
            None,
            None,
            None,
            notification::NotificationFilter::default(),
            40
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        !notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    assert_eq!(
        plamenu::mentions::page(&state, owner, &plamenu::mentions::Query::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn mail_selection_is_durable_idempotent_opt_in_and_rechecks_current_delivery_before_send(
    pool: PgPool,
) {
    let (mut state, owner, _, actor) = fixture(pool).await;
    notification::create(&state.pool, owner, actor, "follow", None)
        .await
        .unwrap();
    notification_delivery::update(
        &state.pool,
        owner,
        &[
            pref("follow", false, true, false),
            pref("music.favourite", false, true, false),
        ],
    )
    .await
    .unwrap();
    notification::create(&state.pool, owner, actor, "music.favourite", None)
        .await
        .unwrap();
    assert_eq!(notification_mail::enqueue_due(&state).await.unwrap(), 0);
    assert_eq!(email::pending(&state.pool).await.unwrap(), 0);
    Arc::make_mut(&mut state.config).smtp = Some(smtp(1));
    assert_eq!(notification_mail::enqueue_due(&state).await.unwrap(), 1);
    assert_eq!(notification_mail::enqueue_due(&state).await.unwrap(), 0);
    assert_eq!(
        count!(
            &state.pool,
            "SELECT count(*) FROM notification_mail_receipts WHERE sent_at IS NOT NULL"
        ),
        0
    );
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
    assert_eq!(prepared.job.recipient, "alice@example.test");
    assert_eq!(prepared.notification_ids.len(), 1);
    assert!(prepared.job.body.contains("/notifications/inbox?mail=true"));
    notification_delivery::update(
        &state.pool,
        owner,
        &[pref("music.favourite", false, false, false)],
    )
    .await
    .unwrap();
    assert!(
        notification_mail::prepare(&state, &job)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    notification_mail::finish(&state, &job, false, &[])
        .await
        .unwrap();
    assert_eq!(
        count!(
            &state.pool,
            "SELECT count(*) FROM notification_mail_receipts WHERE sent_at IS NOT NULL"
        ),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn mail_suppresses_unconfirmed_disabled_deleted_notifications_and_current_mutes(
    pool: PgPool,
) {
    let (mut state, owner, uid, actor) = fixture(pool).await;
    Arc::make_mut(&mut state.config).smtp = Some(smtp(1));
    notification_delivery::update(&state.pool, owner, &[pref("follow", true, true, false)])
        .await
        .unwrap();
    notification::create(&state.pool, owner, actor, "follow", None)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET confirmed_at=NULL WHERE id=$1")
        .bind(uid)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(notification_mail::enqueue_due(&state).await.unwrap(), 0);
    sqlx::query("UPDATE users SET confirmed_at=now() WHERE id=$1")
        .bind(uid)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(notification_mail::enqueue_due(&state).await.unwrap(), 1);
    let job = email::claim_due(&state.pool, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    sqlx::query("UPDATE users SET disabled=true WHERE id=$1")
        .bind(uid)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        notification_mail::prepare(&state, &job)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE users SET disabled=false WHERE id=$1")
        .bind(uid)
        .execute(&state.pool)
        .await
        .unwrap();
    plamenu_db::mute::upsert(&state.pool, owner, actor, true, None)
        .await
        .unwrap();
    assert!(
        notification_mail::prepare(&state, &job)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    notification::clear(&state.pool, owner).await.unwrap();
    assert!(
        notification_mail::prepare(&state, &job)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert_eq!(
        count!(
            &state.pool,
            "SELECT count(*) FROM notification_mail_receipts"
        ),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn actual_smtp_digest_failure_stays_unsent_then_acceptance_stamps_receipts_and_daily_clock(
    pool: PgPool,
) {
    let (mut state, owner, _, actor) = fixture(pool).await;
    notification_delivery::update(&state.pool, owner, &[pref("follow", false, false, true)])
        .await
        .unwrap();
    notification::create(&state.pool, owner, actor, "follow", None)
        .await
        .unwrap();
    sqlx::query("INSERT INTO notification_digest_clock(account_id,next_digest_at) VALUES($1,now()-interval '1 day')").bind(owner).execute(&state.pool).await.unwrap();
    let (port, captured) = capture(false).await;
    Arc::make_mut(&mut state.config).smtp = Some(smtp(port));
    assert_eq!(
        mailer::run_due(
            &state,
            &mailer::Smtp::from_config(state.config.smtp.as_ref().unwrap()).unwrap()
        )
        .await,
        1
    );
    let body = captured.await.unwrap();
    assert!(body.contains("alice@example.test"));
    assert!(body.contains("/notifications/inbox"));
    assert_eq!(
        count!(
            &state.pool,
            "SELECT count(*) FROM notification_mail_receipts WHERE sent_at IS NOT NULL"
        ),
        0
    );
    assert_eq!(email::pending(&state.pool).await.unwrap(), 1);
    assert_eq!(
        count!(
            &state.pool,
            "SELECT count(*) FROM notification_digest_clock WHERE next_digest_at>now()"
        ),
        0
    );
    sqlx::query("UPDATE email_jobs SET run_at=now()")
        .execute(&state.pool)
        .await
        .unwrap();
    let (port, captured) = capture(true).await;
    Arc::make_mut(&mut state.config).smtp = Some(smtp(port));
    assert_eq!(
        mailer::run_due(
            &state,
            &mailer::Smtp::from_config(state.config.smtp.as_ref().unwrap()).unwrap()
        )
        .await,
        1
    );
    captured.await.unwrap();
    assert_eq!(
        count!(
            &state.pool,
            "SELECT count(*) FROM notification_mail_receipts WHERE sent_at IS NOT NULL"
        ),
        1
    );
    assert_eq!(email::pending(&state.pool).await.unwrap(), 0);
    assert_eq!(
        count!(
            &state.pool,
            "SELECT count(*) FROM notification_digest_clock WHERE next_digest_at>now()"
        ),
        1
    );
    assert_eq!(notification_mail::enqueue_due(&state).await.unwrap(), 0);
    let rows = notification_read::list(
        &state.pool,
        plamenu_db::user::find_by_account_id(&state.pool, owner)
            .await
            .unwrap()
            .unwrap()
            .id,
        owner,
        notification_read::Filter {
            mail: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn security_email_keeps_its_original_envelope_and_body_through_the_shared_worker(
    pool: PgPool,
) {
    let (mut state, _, _, _) = fixture(pool).await;
    let (port, captured) = capture(true).await;
    Arc::make_mut(&mut state.config).smtp = Some(smtp(port));
    email::enqueue(
        &state.pool,
        "security@example.test",
        "Confirm registration",
        "Security fixture token link",
    )
    .await
    .unwrap();
    assert_eq!(
        mailer::run_due(
            &state,
            &mailer::Smtp::from_config(state.config.smtp.as_ref().unwrap()).unwrap()
        )
        .await,
        1
    );
    let body = captured.await.unwrap();
    assert!(body.contains("security@example.test"));
    assert!(body.contains("Confirm registration"));
    assert!(body.contains("Security fixture token link"));
    assert_eq!(email::pending(&state.pool).await.unwrap(), 0);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
#[allow(
    clippy::too_many_lines,
    reason = "checks API and native form boundaries through one authenticated fixture"
)]
async fn preference_api_and_native_forms_are_scoped_and_reject_duplicates_and_bad_csrf(
    pool: PgPool,
) {
    let (state, owner, uid, _) = fixture(pool).await;
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "Delivery tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        &state.pool,
        &hash_secret(&token),
        app.id,
        Some(uid),
        "read:notifications write:notifications",
    )
    .await
    .unwrap();
    let router = plamenu::build_router(state.clone());
    let read_only = generate_secret();
    oauth::create_token(
        &state.pool,
        &hash_secret(&read_only),
        app.id,
        Some(uid),
        "read:notifications",
    )
    .await
    .unwrap();
    let r = router
        .clone()
        .oneshot(
            Request::put("/api/v1/notifications/preferences")
                .header("authorization", format!("Bearer {read_only}"))
                .header("content-type", "application/json")
                .body(Body::from("{\"items\":[]}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    for (items, status) in [
        (json!([pref("follow", false, true, true)]), StatusCode::OK),
        (
            json!([pref("unknown", true, false, false)]),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!([
                pref("follow", true, false, false),
                pref("follow", false, false, false)
            ]),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let r = router
            .clone()
            .oneshot(
                Request::put("/api/v1/notifications/preferences")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"items":items}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), status);
    }
    let login = router
        .clone()
        .oneshot(
            Request::post("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "email=alice%40example.test&password=fixture-password",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let csrf = hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let r = router
        .clone()
        .oneshot(
            Request::get("/settings/notification-delivery")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let html =
        String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(html.contains("name=\"email:music.download\""));
    assert!(html.contains("daily"));
    let r = router
        .clone()
        .oneshot(
            Request::post("/web/settings/notification-delivery")
                .header("cookie", cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("csrf=wrong&in_app:follow=true"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = router
        .oneshot(
            Request::post("/web/settings/notification-delivery")
                .header("cookie", cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "csrf={csrf}&in_app:follow=true&email_digest:follow=true"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    let p = notification_delivery::preferences(&state.pool, owner)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.kind == "follow")
        .unwrap();
    assert!(p.in_app && p.email_digest && !p.email);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn stale_mail_claim_cannot_send_or_stamp_a_reclaimed_batch(pool: PgPool) {
    let (mut state, owner, _, actor) = fixture(pool).await;
    Arc::make_mut(&mut state.config).smtp = Some(smtp(1));
    notification_delivery::update(&state.pool, owner, &[pref("follow", true, true, false)])
        .await
        .unwrap();
    notification::create(&state.pool, owner, actor, "follow", None)
        .await
        .unwrap();
    notification_mail::enqueue_due(&state).await.unwrap();
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
    sqlx::query("UPDATE email_jobs SET attempts=attempts+1 WHERE id=$1")
        .bind(job.id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        notification_mail::prepare(&state, &job)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    let before: time::OffsetDateTime =
        sqlx::query_scalar("SELECT run_at FROM email_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    email::retry_current(&state.pool, &job).await.unwrap();
    let after: time::OffsetDateTime =
        sqlx::query_scalar("SELECT run_at FROM email_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    notification_mail::finish(&state, &job, true, &prepared.notification_ids)
        .await
        .unwrap();
    assert_eq!(
        count!(
            &state.pool,
            "SELECT count(*) FROM notification_mail_receipts WHERE sent_at IS NOT NULL"
        ),
        0
    );
    assert_eq!(email::pending(&state.pool).await.unwrap(), 1);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn account_erasure_cancels_pending_mail_and_a_claimed_placeholder_cannot_be_sent(
    pool: PgPool,
) {
    let (mut state, owner, _, actor) = fixture(pool).await;
    Arc::make_mut(&mut state.config).smtp = Some(smtp(1));
    notification_delivery::update(&state.pool, owner, &[pref("follow", true, true, false)])
        .await
        .unwrap();
    notification::create(&state.pool, owner, actor, "follow", None)
        .await
        .unwrap();
    notification_mail::enqueue_due(&state).await.unwrap();
    let job = email::claim_due(&state.pool, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    sqlx::query("DELETE FROM accounts WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(email::pending(&state.pool).await.unwrap(), 0);
    assert!(
        notification_mail::prepare(&state, &job)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
}
