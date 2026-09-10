//! Scheduled-status integration tests: `POST /statuses` scheduling vs. posting
//! now, the minimum-offset and daily-limit validations, the CRUD endpoints, and
//! the publish sweeper replaying a due row into a real, federated status.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{StubFederation, create_local_account, test_app, test_state_with};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, scheduled_status, user};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    let app = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "scheduled",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read write",
        })),
    )
    .await;
    let client_id = app.1["client_id"].as_str().unwrap().to_owned();
    let client_secret = app.1["client_secret"].as_str().unwrap().to_owned();
    let auth = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        None,
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("scope", "read write"),
            ("email", &email),
            ("password", "pw"),
        ],
    )
    .await
    .1;
    let code = auth
        .split("<pre class=\"oob-code\">")
        .nth(1)
        .unwrap()
        .split("</pre>")
        .next()
        .unwrap()
        .to_owned();
    let token = api(
        test_app(pool.clone()),
        "POST",
        "/oauth/token",
        None,
        Some(json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
        })),
    )
    .await;
    (
        account,
        token.1["access_token"].as_str().unwrap().to_owned(),
    )
}

async fn post_form(
    app: Router,
    uri: &str,
    bearer: Option<&str>,
    fields: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn api(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn rfc3339_in(minutes: i64) -> String {
    (OffsetDateTime::now_utc() + time::Duration::minutes(minutes))
        .format(&Rfc3339)
        .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn far_future_scheduled_at_queues_instead_of_posting(pool: PgPool) {
    let (account, token) = user_with_token(&pool, "alice").await;
    let scheduled_at = rfc3339_in(30);
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({ "status": "later", "scheduled_at": scheduled_at })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A ScheduledStatus entity, not a Status: no `content`, but `params`.
    assert!(
        body["content"].is_null(),
        "should not be a posted status: {body}"
    );
    assert_eq!(body["params"]["text"], "later");
    assert_eq!(body["params"]["visibility"], "public");
    assert!(body["scheduled_at"].is_string());

    // It shows up in the queue and did not create a real status.
    let (_, list) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/scheduled_statuses",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], body["id"]);

    let (_, statuses) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", account.id),
        Some(&token),
        None,
    )
    .await;
    assert!(
        statuses.as_array().unwrap().is_empty(),
        "no status yet: {statuses}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scheduled_at_in_the_past_posts_immediately(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({ "status": "now", "scheduled_at": rfc3339_in(-10) })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A real Status, published immediately.
    assert_eq!(body["content"], "<p>now</p>");
    assert!(body["scheduled_at"].is_null());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scheduled_at_within_minimum_offset_is_rejected(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({ "status": "too soon", "scheduled_at": rfc3339_in(2) })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn crud_show_reschedule_and_delete(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let (_, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({ "status": "draft", "scheduled_at": rfc3339_in(30) })),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let path = format!("/api/v1/scheduled_statuses/{id}");

    let (status, shown) = api(test_app(pool.clone()), "GET", &path, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shown["id"], created["id"]);

    // PATCH only changes scheduled_at, and re-validates the offset.
    let new_at = rfc3339_in(120);
    let (status, updated) = api(
        test_app(pool.clone()),
        "PUT",
        &path,
        Some(&token),
        Some(json!({ "scheduled_at": new_at })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    // Postgres keeps microsecond precision, so compare instants, not strings.
    let returned =
        OffsetDateTime::parse(updated["scheduled_at"].as_str().unwrap(), &Rfc3339).unwrap();
    let requested = OffsetDateTime::parse(&new_at, &Rfc3339).unwrap();
    assert!((returned - requested).abs() < time::Duration::seconds(1));

    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &path,
        Some(&token),
        Some(json!({ "scheduled_at": rfc3339_in(1) })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "too-soon reschedule"
    );

    let (status, _) = api(test_app(pool.clone()), "DELETE", &path, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(test_app(pool.clone()), "GET", &path, Some(&token), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn daily_limit_is_enforced(pool: PgPool) {
    let (account, token) = user_with_token(&pool, "alice").await;
    // Seed the day's quota directly (25 = Mastodon's DAILY_LIMIT).
    let day = OffsetDateTime::now_utc() + time::Duration::hours(6);
    for _ in 0..25 {
        scheduled_status::create(
            &pool,
            scheduled_status::NewScheduledStatus {
                object_type: "Note",
                title: None,
                account_id: account.id,
                scheduled_at: day,
                text: "seed",
                content_type: "text/plain",
                visibility: "public",
                in_reply_to_id: None,
                quoted_status_id: None,
                spoiler_text: "",
                sensitive: false,
                language: None,
                application_id: None,
                media_ids: &[],
                poll_options: None,
                poll_expires_in: None,
                poll_multiple: false,
                poll_hide_totals: false,
                quote_approval_policy: None,
            },
        )
        .await
        .unwrap();
    }
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({ "status": "26th", "scheduled_at": day.format(&Rfc3339).unwrap() })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// TZ slice 6 (G8): the per-day cap counts the *owner's* day, not the UTC day.
/// A user at UTC+13 posting in their evening is on the next UTC date, so a
/// UTC-bucketed cap would let them exceed their quota (and, on the other side
/// of their midnight, cut it short). Two instants on the same Auckland day but
/// different UTC days must share one bucket.
#[sqlx::test(migrations = "../db/migrations")]
async fn daily_limit_counts_the_owners_day_not_the_utc_day(pool: PgPool) {
    let (account, token) = user_with_token(&pool, "alice").await;
    sqlx::query("UPDATE users SET time_zone = 'Pacific/Auckland' WHERE account_id = $1")
        .bind(account.id)
        .execute(&pool)
        .await
        .unwrap();

    // Auckland is UTC+12 in June. These two instants are the 10th in UTC but
    // both fall on the 11th locally:
    //   2099-06-10T12:00Z = 00:00 on the 11th in Auckland
    //   2099-06-10T23:00Z = 11:00 on the 11th in Auckland
    let early = OffsetDateTime::parse("2099-06-10T12:00:00Z", &Rfc3339).unwrap();
    let late = OffsetDateTime::parse("2099-06-10T23:00:00Z", &Rfc3339).unwrap();

    // Fill the quota at the first instant.
    for _ in 0..25 {
        scheduled_status::create(
            &pool,
            scheduled_status::NewScheduledStatus {
                object_type: "Note",
                title: None,
                account_id: account.id,
                scheduled_at: early,
                text: "seed",
                content_type: "text/plain",
                visibility: "public",
                in_reply_to_id: None,
                quoted_status_id: None,
                spoiler_text: "",
                sensitive: false,
                language: None,
                application_id: None,
                media_ids: &[],
                poll_options: None,
                poll_expires_in: None,
                poll_multiple: false,
                poll_hide_totals: false,
                quote_approval_policy: None,
            },
        )
        .await
        .unwrap();
    }

    let local = scheduled_status::count_on_day(&pool, account.id, late, "Pacific/Auckland")
        .await
        .unwrap();
    assert_eq!(local, 25, "both instants fall on one Auckland day");

    // 2099-06-11T11:00Z is 23:00 on the 11th in Auckland — still the same
    // local day, but a *different* UTC day. UTC bucketing would see zero.
    let next_utc_day = OffsetDateTime::parse("2099-06-11T11:00:00Z", &Rfc3339).unwrap();
    let local = scheduled_status::count_on_day(&pool, account.id, next_utc_day, "Pacific/Auckland")
        .await
        .unwrap();
    assert_eq!(
        local, 25,
        "still the owner's 11th, so the quota is still spent"
    );
    let utc = scheduled_status::count_on_day(&pool, account.id, next_utc_day, "UTC")
        .await
        .unwrap();
    assert_eq!(utc, 0, "UTC bucketing would have reset the quota mid-day");

    // And the API enforces the owner-local bucket.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "26th",
            "scheduled_at": next_utc_day.format(&Rfc3339).unwrap(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn sweeper_publishes_due_status(pool: PgPool) {
    let (account, token) = user_with_token(&pool, "alice").await;
    // A row already due (publishing-time set in the past, bypassing the API's
    // future-only validation, as the sweeper would eventually see it).
    let row = scheduled_status::create(
        &pool,
        scheduled_status::NewScheduledStatus {
            object_type: "Note",
            title: None,
            account_id: account.id,
            scheduled_at: OffsetDateTime::now_utc() - time::Duration::seconds(1),
            text: "published by the sweeper",
            content_type: "text/plain",
            visibility: "public",
            in_reply_to_id: None,
            quoted_status_id: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            application_id: None,
            media_ids: &[],
            poll_options: None,
            poll_expires_in: None,
            poll_multiple: false,
            poll_hide_totals: false,
            quote_approval_policy: None,
        },
    )
    .await
    .unwrap();

    let state = test_state_with(pool.clone(), StubFederation::with_users(&[]));
    let claimed = plamenu::scheduled_status_publish::run_due(&state).await;
    assert_eq!(claimed, 1);

    // The scheduled row is gone…
    assert!(
        scheduled_status::find_for_account(&pool, account.id, row.id)
            .await
            .unwrap()
            .is_none()
    );
    // …and a real status with its text now exists.
    let (_, statuses) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", account.id),
        Some(&token),
        None,
    )
    .await;
    let posts = statuses.as_array().unwrap();
    assert_eq!(posts.len(), 1, "{statuses}");
    assert_eq!(posts[0]["content"], "<p>published by the sweeper</p>");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scheduling_resolves_and_replays_the_quote_policy(pool: PgPool) {
    let (account, token) = user_with_token(&pool, "alice").await;
    // The policy is resolved to the bitmap at schedule time and surfaced as
    // the client string in the entity's params, like Mastodon's serializer.
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "later, followers only",
            "scheduled_at": rfc3339_in(30),
            "quote_approval_policy": "followers",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["params"]["quote_approval_policy"], "followers");
    let scheduled_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // An unknown value is rejected before anything is queued.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "nope",
            "scheduled_at": rfc3339_in(30),
            "quote_approval_policy": "everyone",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Publish the queued row (due once its scheduled_at passes) and check the
    // stored bitmap survived the round trip.
    sqlx::query("UPDATE scheduled_statuses SET scheduled_at = now() WHERE id = $1")
        .bind(scheduled_id)
        .execute(&pool)
        .await
        .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[]));
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 1);
    let (_, statuses) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", account.id),
        Some(&token),
        None,
    )
    .await;
    let posts = statuses.as_array().unwrap();
    assert_eq!(posts.len(), 1, "{statuses}");
    assert_eq!(
        posts[0]["quote_approval"]["automatic"],
        json!(["followers"]),
        "{statuses}"
    );
}

async fn due_note(pool: &PgPool, account_id: i64, text: &str) -> scheduled_status::ScheduledStatus {
    scheduled_status::create(
        pool,
        scheduled_status::NewScheduledStatus {
            object_type: "Note",
            title: None,
            account_id,
            scheduled_at: OffsetDateTime::now_utc() - time::Duration::seconds(1),
            text,
            content_type: "text/plain",
            visibility: "public",
            in_reply_to_id: None,
            quoted_status_id: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            application_id: None,
            media_ids: &[],
            poll_options: None,
            poll_expires_in: None,
            poll_multiple: false,
            poll_hide_totals: false,
            quote_approval_policy: None,
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn failed_publication_keeps_scheduled_post(pool: PgPool) {
    let (account, _) = user_with_token(&pool, "alice").await;
    let row = due_note(&pool, account.id, "Must survive a database failure").await;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_scheduled_insert() RETURNS trigger AS $$
        BEGIN RAISE EXCEPTION 'injected publication failure'; END;
        $$ LANGUAGE plpgsql;
        CREATE TRIGGER fail_scheduled_insert BEFORE INSERT ON statuses
        FOR EACH ROW EXECUTE FUNCTION fail_scheduled_insert();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[]));
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 1);
    assert!(
        scheduled_status::find_for_account(&pool, account.id, row.id)
            .await
            .unwrap()
            .is_some(),
        "a failed publication must leave the scheduled post recoverable"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(account.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

async fn expire_claims(pool: &PgPool) {
    sqlx::query("UPDATE scheduled_statuses SET publish_after = now() - interval '1 second'")
        .execute(pool)
        .await
        .unwrap();
}

async fn published_count(pool: &PgPool, account_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn abandoned_batch_is_recovered_after_restart(pool: PgPool) {
    let (account, _) = user_with_token(&pool, "alice").await;
    for n in 0..20 {
        due_note(&pool, account.id, &format!("Recover batch post {n}")).await;
    }
    let abandoned = scheduled_status::claim_due(&pool, 20).await.unwrap();
    assert_eq!(abandoned.len(), 20);
    assert_eq!(
        scheduled_status::count_total(&pool, account.id)
            .await
            .unwrap(),
        20
    );
    assert!(
        scheduled_status::claim_due(&pool, 20)
            .await
            .unwrap()
            .is_empty()
    );
    drop(abandoned);
    // A restarted worker has no in-memory copy of the lost batch. Only the
    // database and the expired lease are needed to recover it.
    expire_claims(&pool).await;
    let restarted = test_state_with(pool.clone(), StubFederation::with_users(&[]));
    assert_eq!(
        plamenu::scheduled_status_publish::run_due(&restarted).await,
        20
    );
    assert_eq!(published_count(&pool, account.id).await, 20);
    assert_eq!(
        scheduled_status::count_total(&pool, account.id)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        plamenu::scheduled_status_publish::run_due(&restarted).await,
        0
    );
    assert_eq!(published_count(&pool, account.id).await, 20);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn expired_claim_cannot_publish_after_takeover(pool: PgPool) {
    let (account, _) = user_with_token(&pool, "alice").await;
    due_note(&pool, account.id, "Only the current claim may publish").await;
    let old = scheduled_status::claim_due(&pool, 1)
        .await
        .unwrap()
        .remove(0);
    expire_claims(&pool).await;
    let current = scheduled_status::claim_due(&pool, 1)
        .await
        .unwrap()
        .remove(0);
    assert!(current.publish_generation > old.publish_generation);
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[]));
    assert!(matches!(
        plamenu::scheduled_status_publish::publish_claimed(&state, &old).await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    assert_eq!(published_count(&pool, account.id).await, 0);
    plamenu::scheduled_status_publish::publish_claimed(&state, &current)
        .await
        .unwrap();
    // Simulate a caller that did not observe the successful commit.
    assert!(
        plamenu::scheduled_status_publish::publish_claimed(&state, &current)
            .await
            .is_err()
    );
    assert_eq!(published_count(&pool, account.id).await, 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn concurrent_publication_commits_one_post_and_outbox(pool: PgPool) {
    let (account, _) = user_with_token(&pool, "alice").await;
    let remote = common::RemoteUser::new("remote.example", "bob");
    let follower = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, follower.id, account.id, None)
        .await
        .unwrap();
    due_note(
        &pool,
        account.id,
        "Publish once despite concurrent attempts",
    )
    .await;
    let row = scheduled_status::claim_due(&pool, 1)
        .await
        .unwrap()
        .remove(0);
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[&remote]));
    let (first, second) = tokio::join!(
        plamenu::scheduled_status_publish::publish_claimed(&state, &row),
        plamenu::scheduled_status_publish::publish_claimed(&state, &row),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert_eq!(published_count(&pool, account.id).await, 1);
    assert_eq!(plamenu_db::job::pending_count(&pool).await.unwrap(), 1);
    assert_eq!(
        scheduled_status::count_total(&pool, account.id)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rescheduling_and_cancellation_invalidate_inflight_claims(pool: PgPool) {
    let (account, _) = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[]));
    let row = due_note(&pool, account.id, "Reschedule while preparing publication").await;
    let old = scheduled_status::claim_due(&pool, 1)
        .await
        .unwrap()
        .remove(0);
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    scheduled_status::update_scheduled_at(&pool, account.id, row.id, later)
        .await
        .unwrap()
        .unwrap();
    assert!(
        plamenu::scheduled_status_publish::publish_claimed(&state, &old)
            .await
            .is_err()
    );
    assert!(
        scheduled_status::claim_due(&pool, 1)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(published_count(&pool, account.id).await, 0);
    // Once due again it can be claimed, but deleting the entry cancels that
    // in-flight attempt too.
    scheduled_status::update_scheduled_at(&pool, account.id, row.id, OffsetDateTime::now_utc())
        .await
        .unwrap()
        .unwrap();
    let new = scheduled_status::claim_due(&pool, 1)
        .await
        .unwrap()
        .remove(0);
    assert!(new.publish_generation > old.publish_generation);
    assert!(
        scheduled_status::delete(&pool, account.id, row.id)
            .await
            .unwrap()
    );
    assert!(
        plamenu::scheduled_status_publish::publish_claimed(&state, &new)
            .await
            .is_err()
    );
    assert_eq!(published_count(&pool, account.id).await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
#[allow(
    clippy::too_many_lines,
    reason = "failure and recovery across scheduling, media, and the outbox"
)]
async fn failed_outbox_keeps_post_and_media_reservation_for_retry(pool: PgPool) {
    let (account, token) = user_with_token(&pool, "alice").await;
    let remote = common::RemoteUser::new("remote.example", "bob");
    let follower = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, follower.id, account.id, None)
        .await
        .unwrap();
    // A small real upload exercises reservations and the API's schedule path.
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let mut multipart = b"--schedule-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
    multipart.extend_from_slice(png.get_ref());
    multipart.extend_from_slice(b"\r\n--schedule-test--\r\n");
    let response = test_app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/media")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=schedule-test",
                )
                .body(Body::from(multipart))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let uploaded: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let media_id: i64 = uploaded["id"].as_str().unwrap().parse().unwrap();
    let (status, scheduled) = api(test_app(pool.clone()), "POST", "/api/v1/statuses", Some(&token), Some(json!({
        "status":"Survive a late outbox failure", "scheduled_at":rfc3339_in(10), "media_ids":[uploaded["id"]],
    }))).await;
    assert_eq!(status, StatusCode::OK, "{scheduled}");
    let id: i64 = scheduled["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE scheduled_statuses SET scheduled_at = now() WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE FUNCTION fail_scheduled_delivery() RETURNS trigger AS $$
        BEGIN RAISE EXCEPTION 'injected outbox failure'; END;
        $$ LANGUAGE plpgsql;
        CREATE TRIGGER fail_scheduled_delivery BEFORE INSERT ON delivery_jobs
        FOR EACH ROW EXECUTE FUNCTION fail_scheduled_delivery();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[&remote]));
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 1);
    assert_eq!(published_count(&pool, account.id).await, 0);
    assert_eq!(plamenu_db::job::pending_count(&pool).await.unwrap(), 0);
    assert!(
        scheduled_status::find_for_account(&pool, account.id, id)
            .await
            .unwrap()
            .is_some()
    );
    let reserved: Option<i64> =
        sqlx::query_scalar("SELECT scheduled_status_id FROM media_attachments WHERE id = $1")
            .bind(media_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reserved, Some(id));
    // A failure is not retried in the tight drain loop.
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 0);
    sqlx::query("DROP TRIGGER fail_scheduled_delivery ON delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    expire_claims(&pool).await;
    let restarted = test_state_with(pool.clone(), StubFederation::with_users(&[&remote]));
    assert_eq!(
        plamenu::scheduled_status_publish::run_due(&restarted).await,
        1
    );
    assert_eq!(published_count(&pool, account.id).await, 1);
    assert_eq!(plamenu_db::job::pending_count(&pool).await.unwrap(), 1);
    assert!(
        scheduled_status::find_for_account(&pool, account.id, id)
            .await
            .unwrap()
            .is_none()
    );
    let attachment: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT status_id, scheduled_status_id FROM media_attachments WHERE id = $1",
    )
    .bind(media_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(attachment.0.is_some());
    assert_eq!(attachment.1, None);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn one_failed_post_does_not_block_the_batch(pool: PgPool) {
    let (account, _) = user_with_token(&pool, "alice").await;
    let failed = due_note(&pool, account.id, "Reject this scheduled post").await;
    let successful = due_note(&pool, account.id, "Publish this scheduled post").await;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_one_scheduled_post() RETURNS trigger AS $$
        BEGIN
            IF NEW.text = 'Reject this scheduled post' THEN
                RAISE EXCEPTION 'injected single-post failure';
            END IF;
            RETURN NEW;
        END; $$ LANGUAGE plpgsql;
        CREATE TRIGGER reject_one_scheduled_post BEFORE INSERT ON statuses
        FOR EACH ROW EXECUTE FUNCTION reject_one_scheduled_post();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[]));
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 2);
    assert!(
        scheduled_status::find_for_account(&pool, account.id, failed.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        scheduled_status::find_for_account(&pool, account.id, successful.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(published_count(&pool, account.id).await, 1);
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 0);
    sqlx::query("DROP TRIGGER reject_one_scheduled_post ON statuses")
        .execute(&pool)
        .await
        .unwrap();
    expire_claims(&pool).await;
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 1);
    assert_eq!(published_count(&pool, account.id).await, 2);
}
