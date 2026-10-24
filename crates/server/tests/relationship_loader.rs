//! Populated compatibility of the paired relationship loader and its API scope.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::entities::{render_relationships, rfc3339};
use plamenu_db::account::{self, Account};
use plamenu_db::{
    PgPool, account_domain_block, account_note, block, endorsement, follow, mute, oauth, user,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

struct Fixture {
    alice: Account,
    bob: Account,
    rich: Account,
    pending: Account,
    blank: Account,
    suspended: Account,
}

async fn seed(pool: &PgPool) -> Fixture {
    let alice = create_local_account(pool, "alice", "Alice").await;
    let bob = create_local_account(pool, "bob", "Bob").await;
    let actor = RemoteUser::new("remote.example", "rich");
    let rich = plamenu::remote::store_remote_actor(pool, &actor.actor)
        .await
        .unwrap();
    let pending = create_local_account(pool, "pending", "Pending").await;
    let blank = create_local_account(pool, "blank", "Blank").await;
    let suspended = create_local_account(pool, "suspended", "Suspended").await;
    follow::create(pool, alice.id, rich.id, None).await.unwrap();
    follow::create_request(pool, rich.id, alice.id, None)
        .await
        .unwrap();
    follow::update_settings(
        pool,
        alice.id,
        rich.id,
        Some(false),
        Some(true),
        Some(true),
        Some(&["fr".to_owned(), "en".to_owned()]),
    )
    .await
    .unwrap();
    follow::create_request(pool, alice.id, pending.id, None)
        .await
        .unwrap();
    follow::create(pool, pending.id, alice.id, None)
        .await
        .unwrap();
    follow::update_settings(
        pool,
        alice.id,
        pending.id,
        Some(true),
        Some(false),
        Some(false),
        Some(&[]),
    )
    .await
    .unwrap();
    block::create(pool, alice.id, rich.id, None).await.unwrap();
    block::create(pool, rich.id, alice.id, None).await.unwrap();
    mute::upsert(pool, alice.id, rich.id, true, Some(expiry()))
        .await
        .unwrap();
    mute::upsert(pool, alice.id, pending.id, false, None)
        .await
        .unwrap();
    mute::upsert(
        pool,
        alice.id,
        blank.id,
        true,
        Some(OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap()),
    )
    .await
    .unwrap();
    account_domain_block::create(pool, alice.id, "remote.example")
        .await
        .unwrap();
    account_note::set(pool, alice.id, rich.id, "Alice's private note")
        .await
        .unwrap();
    endorsement::endorse(pool, alice.id, rich.id).await.unwrap();
    account_note::set(pool, alice.id, suspended.id, "still retained")
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(suspended.id)
        .execute(pool)
        .await
        .unwrap();
    let suspended = account::find_by_id(pool, suspended.id)
        .await
        .unwrap()
        .unwrap();
    seed_bob_scope(pool, &bob, &rich, &blank).await;
    Fixture {
        alice,
        bob,
        rich,
        pending,
        blank,
        suspended,
    }
}

async fn seed_bob_scope(pool: &PgPool, bob: &Account, rich: &Account, blank: &Account) {
    // Contradictory viewer-scoped facts must never leak into Alice's page.
    follow::create_request(pool, bob.id, rich.id, None)
        .await
        .unwrap();
    follow::create(pool, rich.id, bob.id, None).await.unwrap();
    follow::update_settings(
        pool,
        bob.id,
        rich.id,
        Some(true),
        Some(false),
        Some(false),
        Some(&["de".to_owned()]),
    )
    .await
    .unwrap();
    account_note::set(pool, bob.id, rich.id, "Bob's private note")
        .await
        .unwrap();
    account_domain_block::create(pool, bob.id, "unrelated.example")
        .await
        .unwrap();
    block::create(pool, bob.id, blank.id, None).await.unwrap();
    mute::upsert(pool, bob.id, blank.id, true, None)
        .await
        .unwrap();
    endorsement::endorse(pool, bob.id, blank.id).await.unwrap();
    account_note::set(pool, bob.id, blank.id, "Bob only")
        .await
        .unwrap();
}

fn expiry() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap()
}

// The previous loader's exact eight sequential reads, with its independent
// serializer kept in the test. It never calls the new paired loader.
async fn sequential_reference(pool: &PgPool, viewer_id: i64, targets: &[Account]) -> Vec<Value> {
    let ids: Vec<i64> = targets.iter().map(|target| target.id).collect();
    let follows = follow::find_out_batch(pool, viewer_id, &ids).await.unwrap();
    let incoming = follow::pending_state_in_batch(pool, viewer_id, &ids)
        .await
        .unwrap();
    let blocking = block::blocking_out_batch(pool, viewer_id, &ids)
        .await
        .unwrap();
    let blocked_by = block::blocked_by_batch(pool, viewer_id, &ids)
        .await
        .unwrap();
    let mutes = mute::active_batch(pool, viewer_id, &ids).await.unwrap();
    let domains = account_domain_block::all_domains(pool, viewer_id)
        .await
        .unwrap();
    let notes = account_note::get_batch(pool, viewer_id, &ids)
        .await
        .unwrap();
    let endorsed = endorsement::exists_batch(pool, viewer_id, &ids)
        .await
        .unwrap();
    targets.iter().map(|target| {
        let edge = follows.get(&target.id);
        let active_mute = mutes.get(&target.id);
        json!({
            "id": target.id.to_string(),
            "following": edge.is_some_and(|edge| !edge.pending),
            "requested": edge.is_some_and(|edge| edge.pending),
            "showing_reblogs": edge.is_some_and(|edge| edge.show_reblogs),
            "showing_replies": edge.is_some_and(|edge| edge.with_replies),
            "notifying": edge.is_some_and(|edge| edge.notify),
            "languages": edge.and_then(|edge| edge.languages.as_ref()),
            "followed_by": incoming.get(&target.id) == Some(&false),
            "requested_by": incoming.get(&target.id) == Some(&true),
            "blocking": blocking.contains(&target.id),
            "blocked_by": blocked_by.contains(&target.id),
            "muting": active_mute.is_some(),
            "muting_notifications": active_mute.is_some_and(|mute| mute.hide_notifications),
            "muting_expires_at": active_mute.and_then(|mute| mute.expires_at).map(|at| rfc3339(at).unwrap()),
            "domain_blocking": target.domain.as_ref().is_some_and(|domain| domains.contains(domain)),
            "note": notes.get(&target.id).cloned().unwrap_or_default(),
            "endorsed": endorsed.contains(&target.id),
        })
    }).collect()
}

fn empty_relationship(id: i64) -> Value {
    json!({
        "id": id.to_string(), "following": false, "requested": false,
        "showing_reblogs": false, "showing_replies": false, "notifying": false,
        "languages": null, "followed_by": false, "requested_by": false,
        "blocking": false, "blocked_by": false, "muting": false,
        "muting_notifications": false, "muting_expires_at": null,
        "domain_blocking": false, "note": "", "endorsed": false,
    })
}

fn expected_rich(fixture: &Fixture, alice: bool) -> Value {
    let mut value = empty_relationship(fixture.rich.id);
    value["following"] = json!(alice);
    value["requested"] = json!(!alice);
    value["showing_reblogs"] = json!(!alice);
    value["showing_replies"] = json!(alice);
    value["notifying"] = json!(alice);
    value["languages"] = if alice {
        json!(["fr", "en"])
    } else {
        json!(["de"])
    };
    value["followed_by"] = json!(!alice);
    value["requested_by"] = json!(alice);
    for field in [
        "blocking",
        "blocked_by",
        "muting",
        "muting_notifications",
        "domain_blocking",
        "endorsed",
    ] {
        value[field] = json!(alice);
    }
    if alice {
        value["muting_expires_at"] = json!(rfc3339(expiry()).unwrap());
    }
    value["note"] = if alice {
        json!("Alice's private note")
    } else {
        json!("Bob's private note")
    };
    value
}

async fn one_connection(pool: &PgPool) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn populated_pairs_equal_sequential_loader_on_one_connection(pool: PgPool) {
    let fixture = seed(&pool).await;
    let single = one_connection(&pool).await;
    let targets = [
        fixture.rich.clone(),
        fixture.pending.clone(),
        fixture.blank.clone(),
        fixture.suspended.clone(),
        fixture.rich.clone(),
    ];
    for (viewer, alice) in [(&fixture.alice, true), (&fixture.bob, false)] {
        let actual = tokio::time::timeout(
            Duration::from_secs(10),
            render_relationships(&single, viewer.id, &targets),
        )
        .await
        .expect("paired reads must release the sole connection")
        .unwrap();
        let old = sequential_reference(&single, viewer.id, &targets).await;
        assert_eq!(
            actual, old,
            "complete output differs for viewer {}",
            viewer.id
        );
        assert_eq!(actual[0], expected_rich(&fixture, alice));
        assert_eq!(
            actual[0], actual[4],
            "duplicate target must retain output/order"
        );
        if alice {
            let mut pending = empty_relationship(fixture.pending.id);
            for field in ["requested", "showing_reblogs", "followed_by", "muting"] {
                pending[field] = json!(true);
            }
            assert_eq!(actual[1], pending, "pending settings and indefinite mute");
            assert_eq!(
                actual[2],
                empty_relationship(fixture.blank.id),
                "expired mute and Bob's facts excluded"
            );
            let mut suspended = empty_relationship(fixture.suspended.id);
            suspended["note"] = json!("still retained");
            assert_eq!(
                actual[3], suspended,
                "renderer preserves explicitly supplied suspended target"
            );
        } else {
            assert_eq!(actual[1], empty_relationship(fixture.pending.id));
            let mut blank = empty_relationship(fixture.blank.id);
            for field in ["blocking", "muting", "muting_notifications", "endorsed"] {
                blank[field] = json!(true);
            }
            blank["note"] = json!("Bob only");
            assert_eq!(actual[2], blank);
            assert_eq!(actual[3], empty_relationship(fixture.suspended.id));
        }
    }
    assert!(
        render_relationships(&single, fixture.alice.id, &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(single.size(), 1);
    single.close().await;
}

async fn token_for(pool: &PgPool, account: &Account) -> String {
    let hash = hash_password("pw").unwrap();
    let local_user = user::create(
        pool,
        account.id,
        Some(&format!("{}@plamenu.test", account.username)),
        &hash,
    )
    .await
    .unwrap();
    let scopes = "read:follows";
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "relationship-loader-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes,
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        pool,
        &hash_secret(&token),
        app.id,
        Some(local_user.id),
        scopes,
    )
    .await
    .unwrap();
    token
}

async fn get_relationships(pool: &PgPool, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        test_app(pool.clone()).oneshot(request.body(Body::empty()).unwrap()),
    )
    .await
    .expect("API must work with one connection")
    .unwrap();
    let code = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap())
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn relationship_api_preserves_order_missing_suspended_and_viewer_scope(pool: PgPool) {
    let fixture = seed(&pool).await;
    let alice_token = token_for(&pool, &fixture.alice).await;
    let bob_token = token_for(&pool, &fixture.bob).await;
    let single = one_connection(&pool).await;
    let uri = format!(
        "/api/v1/accounts/relationships?id[]=999999&id[]={}&id[]={}&id[]={}&id[]={}&id[]={}",
        fixture.rich.id,
        fixture.suspended.id,
        fixture.pending.id,
        fixture.rich.id,
        fixture.blank.id
    );
    for (viewer, token, alice) in [
        (&fixture.alice, &alice_token, true),
        (&fixture.bob, &bob_token, false),
    ] {
        let (code, body) = get_relationships(&single, &uri, Some(token)).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let targets = [
            fixture.rich.clone(),
            fixture.pending.clone(),
            fixture.rich.clone(),
            fixture.blank.clone(),
        ];
        assert_eq!(
            body,
            json!(sequential_reference(&single, viewer.id, &targets).await)
        );
        assert_eq!(body[0], expected_rich(&fixture, alice));
        assert_eq!(body[0], body[2]);
        let (code, included) =
            get_relationships(&single, &format!("{uri}&with_suspended=true"), Some(token)).await;
        assert_eq!(code, StatusCode::OK, "{included}");
        let targets = [
            fixture.rich.clone(),
            fixture.suspended.clone(),
            fixture.pending.clone(),
            fixture.rich.clone(),
            fixture.blank.clone(),
        ];
        assert_eq!(
            included,
            json!(sequential_reference(&single, viewer.id, &targets).await)
        );
        assert_eq!(included[1]["id"], fixture.suspended.id.to_string());
        let (code, empty) = get_relationships(
            &single,
            "/api/v1/accounts/relationships?id[]=999999",
            Some(token),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(empty, json!([]));
    }
    let (code, _) = get_relationships(&single, &uri, None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert_eq!(single.size(), 1);
    single.close().await;
}
