//! Authoritative presentation reconciliation after signed inline actor Updates.
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{RemoteUser, StubFederation, test_state_with};
use plamenu::{AppState, build_router};
use plamenu_db::{PgPool, account, instance_policy};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tower::ServiceExt;

async fn fixture(pool: &PgPool) -> (AppState, Arc<StubFederation>, RemoteUser, i64) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let username = format!("bob{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let bob = RemoteUser::new("remote.example", &username);
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let stored = plamenu::remote::refresh_remote_actor(&state, &bob.actor)
        .await
        .unwrap();
    (state, stub, bob, stored.id)
}

fn update(bob: &RemoteUser, name: &str, sequence: u64) -> Value {
    json!({
        "id": format!("{}#updates/{sequence}", bob.actor.id), "type": "Update", "actor": bob.actor.id,
        "object": {"id": bob.actor.id, "type": bob.actor.kind, "preferredUsername": bob.actor.preferred_username, "name": name,
            "summary": "<p>inline</p>", "inbox": bob.actor.inbox,
            "publicKey": {"id": bob.actor.public_key.id, "owner": bob.actor.id, "publicKeyPem": bob.actor.public_key.public_key_pem}}
    })
}

fn origin_document(actor: &plamenu_ap::actor::RemoteActor) -> Value {
    json!({"id":actor.id,"type":actor.kind,"preferredUsername":actor.preferred_username,
        "name":actor.name,"summary":actor.summary,"inbox":actor.inbox,"suspended":actor.suspended,
        "mediaType":actor.media_type,"playlists":actor.playlists,
        "assertionMethod": actor.assertion_method,
        "publicKey":{"id":actor.public_key.id,"owner":actor.public_key.owner,"publicKeyPem":actor.public_key.public_key_pem}})
}

async fn send(state: &AppState, bob: &RemoteUser, body: &Value) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed = bob
        .signer()
        .sign_post("plamenu.test", "/inbox", &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed.host)
        .header("date", signed.date)
        .header("digest", signed.digest)
        .header("signature", signed.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

async fn idle(state: &AppState) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.actor_reconciliation.active_count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bounded reconciliation should finish");
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn channel_reconciliation_keeps_plain_name_and_markdown_summary(pool: PgPool) {
    let channel = RemoteUser::channel("video.example", "forum_demos");
    let stub = StubFederation::with_actors([channel.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let stored = plamenu::remote::refresh_remote_actor(&state, &channel.actor)
        .await
        .unwrap();
    let mut origin = channel.actor.clone();
    origin.name = Some("Current Demos & Keynotes".into());
    origin.summary = Some("[Current playlist](https://video.example/playlist)".into());
    stub.objects
        .lock()
        .unwrap()
        .insert(channel.actor.id.clone(), origin_document(&origin));
    assert_eq!(
        send(&state, &channel, &update(&channel, "Stale", 1)).await,
        StatusCode::ACCEPTED
    );
    idle(&state).await;
    let after = account::find_by_id(&pool, stored.id)
        .await
        .unwrap()
        .unwrap();
    assert!(after.is_group());
    assert_eq!(after.display_name, "Current Demos & Keynotes");
    assert!(
        after
            .note
            .contains("href=\"https://video.example/playlist\"")
    );
    assert!(!after.note.contains("[Current playlist]"));
}

async fn actor_fetch_started(stub: &StubFederation, uri: &str, initial: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while stub.fetches().iter().filter(|f| f.as_str() == uri).count() <= initial {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn stale_inline_text_converges_without_identity_or_key_rewrite(pool: PgPool) {
    let (state, stub, bob, id) = fixture(&pool).await;
    let before = account::find_by_id(&pool, id).await.unwrap().unwrap();
    let mut origin = bob.actor.clone();
    origin.name = Some("Current & <b>name</b>".into());
    origin.summary = Some("<p>Current bio</p><script>alert(1)</script>".into());
    // Even a changed unverified handle claim cannot rename the stored account.
    origin.preferred_username = "unverified-rename".into();
    let replacement_keys = plamenu_ap::keys::generate_keypair().unwrap();
    origin
        .public_key
        .public_key_pem
        .clone_from(&replacement_keys.public_pem);
    stub.objects
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), origin_document(&origin));
    assert_eq!(
        send(&state, &bob, &update(&bob, "Stale", 1)).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        account::find_by_id(&pool, id)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Stale"
    );
    idle(&state).await;
    let after = account::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(after.display_name, "Current & name");
    assert_eq!(after.note, "<p>Current bio</p>");
    assert_eq!(after.username, before.username);
    assert_eq!(after.domain, before.domain);
    assert_eq!(after.uri, before.uri);
    assert_eq!(after.public_key, before.public_key);
    assert_eq!(after.inbox_url, before.inbox_url);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn foreign_malformed_key_only_or_failed_fetch_preserves_inline(pool: PgPool) {
    for mode in 0..4 {
        let (state, stub, bob, id) = fixture(&pool).await;
        assert_eq!(
            send(&state, &bob, &update(&bob, "Validated inline", mode)).await,
            StatusCode::ACCEPTED
        );
        let mut origin = bob.actor.clone();
        origin.name = Some("Must not be applied".into());
        match mode {
            0 => {
                stub.objects.lock().unwrap().remove(&bob.actor.id);
            }
            1 => {
                origin.id = "https://foreign.example/users/mallory".into();
                stub.objects
                    .lock()
                    .unwrap()
                    .insert(bob.actor.id.clone(), origin_document(&origin));
            }
            2 => {
                origin.public_key.owner = "https://foreign.example/users/mallory".into();
                stub.objects
                    .lock()
                    .unwrap()
                    .insert(bob.actor.id.clone(), origin_document(&origin));
            }
            _ => {
                origin.inbox.clear();
                stub.objects
                    .lock()
                    .unwrap()
                    .insert(bob.actor.id.clone(), origin_document(&origin));
            }
        }
        idle(&state).await;
        assert_eq!(
            account::find_by_id(&pool, id)
                .await
                .unwrap()
                .unwrap()
                .display_name,
            "Validated inline"
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn dirty_generation_discards_held_old_origin_and_refetches_current(pool: PgPool) {
    let (state, stub, bob, id) = fixture(&pool).await;
    let initial = stub
        .fetches()
        .iter()
        .filter(|f| *f == &bob.actor.id)
        .count();
    let mut old_origin = bob.actor.clone();
    old_origin.name = Some("Old origin".into());
    stub.objects
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), origin_document(&old_origin));
    stub.gate_activitypub_fetches();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            send(&state, &bob, &update(&bob, "First inline", 1))
        )
        .await
        .unwrap(),
        StatusCode::ACCEPTED
    );
    actor_fetch_started(&stub, &bob.actor.id, initial).await;
    let mut new_origin = bob.actor.clone();
    new_origin.name = Some("Current origin".into());
    stub.objects
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), origin_document(&new_origin));
    for sequence in 2..22 {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                send(&state, &bob, &update(&bob, "Second inline", sequence))
            )
            .await
            .unwrap(),
            StatusCode::ACCEPTED
        );
        assert_eq!(state.actor_reconciliation.active_count(), 1);
    }
    stub.open_activitypub_fetches();
    idle(&state).await;
    assert_eq!(
        account::find_by_id(&pool, id)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Current origin"
    );
    assert_eq!(
        stub.fetches()
            .iter()
            .filter(|f| *f == &bob.actor.id)
            .count()
            - initial,
        2,
        "one old held response and one coalesced current pass"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn intervening_revision_change_suppresses_optional_write_without_fetch_amplification(
    pool: PgPool,
) {
    let (state, stub, bob, id) = fixture(&pool).await;
    let initial = stub
        .fetches()
        .iter()
        .filter(|f| *f == &bob.actor.id)
        .count();
    let mut origin = bob.actor.clone();
    origin.name = Some("Current origin".into());
    stub.objects
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), origin_document(&origin));
    stub.gate_activitypub_fetches();
    assert_eq!(
        send(&state, &bob, &update(&bob, "Inline", 1)).await,
        StatusCode::ACCEPTED
    );
    actor_fetch_started(&stub, &bob.actor.id, initial).await;
    let old = account::find_by_id(&pool, id)
        .await
        .unwrap()
        .unwrap()
        .indexable;
    sqlx::query("UPDATE accounts SET indexable = NOT indexable WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    stub.open_activitypub_fetches();
    idle(&state).await;
    let after = account::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(after.display_name, "Inline");
    assert_eq!(after.indexable, !old);
    assert_eq!(
        stub.fetches()
            .iter()
            .filter(|f| *f == &bob.actor.id)
            .count()
            - initial,
        1
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn suspension_or_new_domain_block_during_fetch_prevents_text_write(pool: PgPool) {
    for suspend in [true, false] {
        let (state, stub, bob, id) = fixture(&pool).await;
        let initial = stub
            .fetches()
            .iter()
            .filter(|f| *f == &bob.actor.id)
            .count();
        let mut origin = bob.actor.clone();
        origin.name = Some("Must not be applied".into());
        stub.objects
            .lock()
            .unwrap()
            .insert(bob.actor.id.clone(), origin_document(&origin));
        stub.gate_activitypub_fetches();
        assert_eq!(
            send(&state, &bob, &update(&bob, "Validated inline", 1)).await,
            StatusCode::ACCEPTED
        );
        actor_fetch_started(&stub, &bob.actor.id, initial).await;
        if suspend {
            account::suspend(&pool, id, "local").await.unwrap();
        } else {
            instance_policy::create_domain_block(
                &pool,
                instance_policy::NewDomainBlock {
                    domain: "remote.example",
                    severity: "suspend",
                    reject_media: false,
                    reject_reports: false,
                    private_comment: None,
                    public_comment: None,
                    obfuscate: false,
                },
            )
            .await
            .unwrap();
        }
        stub.open_activitypub_fetches();
        idle(&state).await;
        assert_eq!(
            account::find_by_id(&pool, id)
                .await
                .unwrap()
                .unwrap()
                .display_name,
            "Validated inline"
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn foreign_or_unsigned_update_does_not_admit_reconciliation(pool: PgPool) {
    let (state, _, bob, _) = fixture(&pool).await;
    let mut foreign = update(&bob, "Forged", 1);
    foreign["object"]["id"] = json!("https://foreign.example/users/mallory");
    assert_eq!(send(&state, &bob, &foreign).await, StatusCode::ACCEPTED);
    assert_eq!(state.actor_reconciliation.active_count(), 0);
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("content-type", "application/activity+json")
        .body(Body::from(
            serde_json::to_vec(&update(&bob, "Unsigned", 2)).unwrap(),
        ))
        .unwrap();
    assert_eq!(
        build_router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(state.actor_reconciliation.active_count(), 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn later_real_change_is_not_suppressed_by_freshness_window(pool: PgPool) {
    let (state, stub, bob, id) = fixture(&pool).await;
    for sequence in 1..=2 {
        let expected = format!("Current {sequence}");
        let mut origin = bob.actor.clone();
        origin.name = Some(expected.clone());
        stub.objects
            .lock()
            .unwrap()
            .insert(bob.actor.id.clone(), origin_document(&origin));
        assert_eq!(
            send(&state, &bob, &update(&bob, "Queued stale", sequence)).await,
            StatusCode::ACCEPTED
        );
        idle(&state).await;
        assert_eq!(
            account::find_by_id(&pool, id)
                .await
                .unwrap()
                .unwrap()
                .display_name,
            expected
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn identical_text_inline_commit_before_admission_rejects_old_xmin(pool: PgPool) {
    let (state, stub, bob, id) = fixture(&pool).await;
    let initial = stub
        .fetches()
        .iter()
        .filter(|f| *f == &bob.actor.id)
        .count();
    let mut origin = bob.actor.clone();
    origin.name = Some("Held old origin".into());
    stub.objects
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), origin_document(&origin));
    stub.gate_activitypub_fetches();
    assert_eq!(
        send(&state, &bob, &update(&bob, "Same inline", 1)).await,
        StatusCode::ACCEPTED
    );
    actor_fetch_started(&stub, &bob.actor.id, initial).await;
    // Deliberately model the gap between a same-text inline DB commit and its
    // later schedule/admission call. The generation remains unchanged here.
    sqlx::query("UPDATE accounts SET display_name = display_name, note = note WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    stub.open_activitypub_fetches();
    idle(&state).await;
    assert_eq!(
        account::find_by_id(&pool, id)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Same inline"
    );
    assert_eq!(
        stub.fetches()
            .iter()
            .filter(|f| *f == &bob.actor.id)
            .count()
            - initial,
        1
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn invalid_lifecycle_controller_or_key_scope_preserves_inline_without_key_fetch(
    pool: PgPool,
) {
    for mode in 0..7 {
        let (state, stub, bob, id) = fixture(&pool).await;
        let mut origin = origin_document(&bob.actor);
        origin["name"] = json!("Must not be applied");
        let key_uri = format!("{}#auxiliary", bob.actor.id);
        let multibase = plamenu_ap::keys::generate_ed25519_keypair()
            .public_multibase
            .clone();
        origin["assertionMethod"] = match mode {
            0 => {
                json!([{"id":key_uri,"type":"Multikey","controller":bob.actor.id,"publicKeyMultibase":multibase,"revoked":"2020-01-01T00:00:00Z"}])
            }
            1 => {
                json!([{"id":key_uri,"type":"Multikey","controller":bob.actor.id,"publicKeyMultibase":multibase,"expires":"2020-01-01T00:00:00Z"}])
            }
            2 => json!([key_uri]),
            3 => {
                json!([{"id":key_uri,"type":"Multikey","controller":"https://foreign.example/users/other","publicKeyMultibase":multibase}])
            }
            4 => {
                json!([{"id":"https://foreign.example/key","type":"Multikey","controller":bob.actor.id,"publicKeyMultibase":multibase}])
            }
            _ => {
                json!([{"id":key_uri,"type":"Multikey","controller":bob.actor.id,"publicKeyMultibase":multibase}])
            }
        };
        if mode == 5 {
            origin["publicKey"]["id"] = json!("https://foreign.example/key");
        }
        if mode == 6 {
            origin["publicKey"] = json!([origin["publicKey"].clone(), {
                "id":"https://foreign.example/extra", "owner":bob.actor.id, "publicKeyPem":bob.actor.public_key.public_key_pem
            }]);
        }
        stub.objects
            .lock()
            .unwrap()
            .insert(bob.actor.id.clone(), origin);
        let initial = stub.fetches().len();
        assert_eq!(
            send(&state, &bob, &update(&bob, "Validated inline", mode)).await,
            StatusCode::ACCEPTED
        );
        idle(&state).await;
        assert_eq!(
            account::find_by_id(&pool, id)
                .await
                .unwrap()
                .unwrap()
                .display_name,
            "Validated inline"
        );
        assert_eq!(
            &stub.fetches()[initial..],
            std::slice::from_ref(&bob.actor.id),
            "only the authoritative actor URI is fetched"
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn shutdown_cancels_held_origin_and_releases_task_state(pool: PgPool) {
    let (state, stub, bob, id) = fixture(&pool).await;
    let initial = stub
        .fetches()
        .iter()
        .filter(|f| *f == &bob.actor.id)
        .count();
    stub.objects
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), origin_document(&bob.actor));
    stub.gate_activitypub_fetches();
    assert_eq!(
        send(&state, &bob, &update(&bob, "Validated inline", 1)).await,
        StatusCode::ACCEPTED
    );
    actor_fetch_started(&stub, &bob.actor.id, initial).await;
    state.shutdown.cancel();
    idle(&state).await;
    assert_eq!(state.actor_reconciliation.active_count(), 0);
    assert_eq!(
        account::find_by_id(&pool, id)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Validated inline"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn slow_origin_timeout_preserves_inline_and_returns_accepted_promptly(pool: PgPool) {
    let (state, stub, bob, id) = fixture(&pool).await;
    stub.objects
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), origin_document(&bob.actor));
    stub.gate_activitypub_fetches();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            send(&state, &bob, &update(&bob, "Validated inline", 1))
        )
        .await
        .unwrap(),
        StatusCode::ACCEPTED
    );
    tokio::time::timeout(Duration::from_secs(35), async {
        while state.actor_reconciliation.active_count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("30-second origin deadline must release the slot");
    assert_eq!(
        account::find_by_id(&pool, id)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Validated inline"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn inline_commit_after_generation_check_before_cas_cannot_be_overwritten(pool: PgPool) {
    let (state, stub, bob, id) = fixture(&pool).await;
    let initial = stub
        .fetches()
        .iter()
        .filter(|f| *f == &bob.actor.id)
        .count();
    let mut origin = bob.actor.clone();
    origin.name = Some("Held old origin".into());
    stub.objects
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), origin_document(&origin));
    stub.gate_activitypub_fetches();
    assert_eq!(
        send(&state, &bob, &update(&bob, "Same inline", 1)).await,
        StatusCode::ACCEPTED
    );
    actor_fetch_started(&stub, &bob.actor.id, initial).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(id)
        .execute(&mut *lock)
        .await
        .unwrap();
    stub.open_activitypub_fetches();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event = 'advisory' AND query LIKE '%pg_advisory_xact_lock%')")
                .fetch_one(&pool).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("worker must have passed generation/policy checks and wait before CAS");
    // Preserve the name/bio to model the previously unsafe retry condition.
    sqlx::query("UPDATE accounts SET display_name = display_name, note = note WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    lock.commit().await.unwrap();
    idle(&state).await;
    assert_eq!(
        account::find_by_id(&pool, id)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Same inline"
    );
}
