//! End-to-end service tests for the durable FEP-752d baseline.

mod common;

use std::fmt::Write as _;
use std::io::{Cursor, Write as _};
use std::sync::Arc;
use std::time::SystemTime;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt as _;
use plamenu::actions::{PostParams, WebxdcInvitation};
use plamenu::{build_router, webxdc as protocol};
use plamenu_db::account::AccountSearch;
use plamenu_db::webxdc as db;
use plamenu_db::{PgPool, account, admin_account, group, job};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use zip::write::SimpleFileOptions;

fn package() -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file("index.html", SimpleFileOptions::default())
        .unwrap();
    writer
        .write_all(
            b"<!doctype html><title>FEP test</title><script src=webxdc.js></script><script src=/assets/app.js></script><div id=app></div>",
        )
        .unwrap();
    writer
        .start_file("assets/app.js", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"window.chessLoaded = true;").unwrap();
    writer.finish().unwrap().into_inner()
}

async fn local_session(
    pool: &PgPool,
) -> (plamenu::AppState, plamenu_db::account::Account, db::Session) {
    let alice = create_local_account(pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::<StubFederation>::default());
    let bytes = package();
    plamenu_db::user::create(pool, alice.id, None, "unused")
        .await
        .unwrap();
    let session = protocol::create_local(
        &state,
        protocol::CreateLocal {
            creator: &alice,
            name: "Chess",
            summary: "A durable test game",
            bundle_name: "chess.xdc",
            bundle_bytes: &bytes,
            membership_policy: "open",
            send_update_interval: 0,
            send_update_max_size: 32_768,
        },
    )
    .await
    .unwrap();
    (state, alice, session)
}

async fn cached_remote_session(
    pool: &PgPool,
    participant: &plamenu_db::account::Account,
) -> (RemoteUser, db::Session) {
    let mut coordinator = RemoteUser::new("remote.example", "webxdc-session");
    "Group".clone_into(&mut coordinator.actor.kind);
    let coordinator_account = plamenu::remote::store_remote_actor(pool, &coordinator.actor)
        .await
        .unwrap();
    let bytes = package();
    let validated = protocol::validate_package(&bytes, None).unwrap();
    let bundle_url = format!("{}/bundle.xdc", coordinator.actor.id);
    let session = db::store_remote(
        pool,
        db::NewRemoteSession {
            account_id: coordinator_account.id,
            creator_uri: "https://remote.example/users/creator",
            name: "Remote board",
            summary: "cached remote session",
            coordinator_uri: &coordinator.actor.id,
            bundle_id: &bundle_url,
            bundle_url: &bundle_url,
            bundle_name: "remote.xdc",
            bundle_media_type: protocol::MEDIA_TYPE,
            digest_multibase: &validated.digest_multibase,
            bundle_bytes: &bytes,
            send_update_interval: 0,
            send_update_max_size: 32_768,
            published_at: time::OffsetDateTime::now_utc(),
            ended_at: None,
            files: &validated.files,
        },
    )
    .await
    .unwrap();
    let participant_uri = plamenu::entities::account_uri("plamenu.test", participant);
    let follow_id = format!("{participant_uri}/webxdc-follows/remote-test");
    db::request_membership(
        pool,
        session.id,
        participant.id,
        &participant_uri,
        &follow_id,
        "local-self-address",
    )
    .await
    .unwrap();
    db::mark_accepted(pool, &follow_id, 0).await.unwrap();
    (coordinator, session)
}

async fn response(
    app: &axum::Router,
    uri: &str,
    host: &str,
    accept: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::builder().uri(uri).header(header::HOST, host);
    if let Some(accept) = accept {
        request = request.header(header::ACCEPT, accept);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn post_signed(
    app: &axum::Router,
    path: &str,
    activity: &Value,
    signer: &RequestSigner,
) -> StatusCode {
    let body = serde_json::to_vec(activity).unwrap();
    let request_signature = signer.sign_post("plamenu.test", path, &body, SystemTime::now());
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, request_signature.host)
                .header(header::DATE, request_signature.date)
                .header("digest", request_signature.digest)
                .header("signature", request_signature.signature)
                .header(header::CONTENT_TYPE, "application/activity+json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn actor_bundle_landing_and_isolated_runtime_are_distinct_surfaces(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    let app = build_router(state.clone());

    let actor_response = response(
        &app,
        &format!("/webxdc/{}", session.id),
        "plamenu.test",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(actor_response.status(), StatusCode::OK);
    assert_eq!(
        actor_response.headers()[header::CONTENT_TYPE],
        plamenu_ap::ACTIVITY_JSON_UTF8
    );
    let actor: Value = serde_json::from_slice(
        &actor_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert!(actor["type"].as_array().unwrap().contains(&json!("Group")));
    assert!(
        actor["type"]
            .as_array()
            .unwrap()
            .contains(&json!("WebxdcSession"))
    );
    assert_eq!(actor["webxdcProtocol"], protocol::PROTOCOL);
    assert_eq!(
        actor["attachment"]["digestMultibase"],
        session.digest_multibase
    );

    let bundle = response(
        &app,
        &format!("/webxdc/{}/bundle.xdc", session.id),
        "plamenu.test",
        None,
    )
    .await;
    assert_eq!(bundle.status(), StatusCode::OK);
    assert_eq!(bundle.headers()[header::CONTENT_TYPE], protocol::MEDIA_TYPE);
    assert_eq!(
        bundle.into_body().collect().await.unwrap().to_bytes(),
        db::bundle(&pool, session.id).await.unwrap().unwrap()
    );

    let landing = response(
        &app,
        &format!("/webxdc/{}", session.id),
        "plamenu.test",
        Some("text/html"),
    )
    .await;
    let landing_html = String::from_utf8(
        landing
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(landing_html.contains("App privacy"));
    assert!(
        !landing_html.contains("<iframe"),
        "a landing page never executes the app"
    );

    let runtime_uri = format!(
        "/webxdc-runtime/{}/index.html?self_addr=opaque-test&self_name=Alice",
        session.id
    );
    let wrong_origin = response(&app, &runtime_uri, "plamenu.test", None).await;
    assert_eq!(wrong_origin.status(), StatusCode::NOT_FOUND);

    let runtime_host = format!("{}.webxdc.plamenu.test", session.id);
    let shared_origin = response(&app, &runtime_uri, "webxdc.plamenu.test", None).await;
    assert_eq!(shared_origin.status(), StatusCode::NOT_FOUND);

    let runtime = response(&app, &runtime_uri, &runtime_host, None).await;
    assert_eq!(runtime.status(), StatusCode::OK);
    assert!(runtime.headers().get(header::X_FRAME_OPTIONS).is_none());
    assert!(
        runtime
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
    assert_eq!(runtime.headers()[header::REFERRER_POLICY], "no-referrer");
    let csp = runtime.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    let connect = csp
        .split(';')
        .map(str::trim)
        .find(|directive| directive.starts_with("connect-src "))
        .unwrap();
    assert_eq!(
        connect, "connect-src 'self' blob: data:",
        "local generated assets must be readable without granting external network access"
    );
    assert!(csp.contains("frame-ancestors https://plamenu.test"));
    let runtime_html = String::from_utf8(
        runtime
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(runtime_html.contains("webxdc.js"));
    assert!(runtime_html.contains("opaque-test"));
    assert!(runtime_html.contains(&format!(r#"session:"{}""#, session.id)));
    let actor_uri = plamenu::entities::account_uri("plamenu.test", &alice);
    assert!(!runtime_html.contains(&actor_uri));

    let bridge = response(
        &app,
        &format!("/webxdc-runtime/{}/webxdc.js", session.id),
        &runtime_host,
        None,
    )
    .await;
    assert_eq!(bridge.status(), StatusCode::OK);
    assert_eq!(
        bridge.headers()[header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );

    // This deliberately collides with Plamenu's own `/assets/app.js` route:
    // the dedicated runtime host must still resolve the package entry.
    let root_absolute_asset = response(&app, "/assets/app.js", &runtime_host, None).await;
    assert_eq!(root_absolute_asset.status(), StatusCode::OK);
    assert_eq!(
        root_absolute_asset.headers()[header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        root_absolute_asset
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
        "window.chessLoaded = true;"
    );
    let server_api_on_runtime_origin =
        response(&app, "/api/v1/instance", &runtime_host, None).await;
    assert_eq!(server_api_on_runtime_origin.status(), StatusCode::NOT_FOUND);

    let cert_allowed = response(
        &app,
        &format!("/webxdc/caddy-allow?domain={runtime_host}"),
        "plamenu.test",
        None,
    )
    .await;
    assert_eq!(cert_allowed.status(), StatusCode::NO_CONTENT);
    let cert_refused = response(
        &app,
        "/webxdc/caddy-allow?domain=999.webxdc.plamenu.test",
        "plamenu.test",
        None,
    )
    .await;
    assert_eq!(cert_refused.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn invitation_is_an_ordinary_note_with_stable_fep_enhancement(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    let before = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM statuses WHERE account_id = $1"#,
        alice.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, 0, "session creation must not silently post a Note");
    let text = format!("Join this game: {}", session.coordinator_uri);
    let (status, _) = plamenu::actions::post_webxdc_invitation(
        &state,
        PostParams {
            username: &alice.username,
            text: &text,
            visibility: "public",
            ..PostParams::default()
        },
        WebxdcInvitation {
            session_id: session.id,
            session_uri: &session.coordinator_uri,
            session_name: &session.name,
        },
    )
    .await
    .unwrap();
    let note = plamenu::note::note_for_status(&state, &status, &alice)
        .await
        .unwrap();
    assert_eq!(note["type"], "Note");
    assert!(note["content"].as_str().unwrap().contains("<a href="));
    assert_eq!(note["audience"], session.coordinator_uri);
    assert_eq!(
        note["attachment"].as_array().unwrap().last().unwrap()["rel"],
        protocol::OPEN_REL
    );
    let after = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM statuses WHERE account_id = $1"#,
        alice.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(after, 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unsupported_server_visitor_can_join_and_update_as_session_guest(pool: PgPool) {
    let (state, _alice, session) = local_session(&pool).await;
    let app = build_router(state);
    let join = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/web/webxdc/{}/guest", session.id))
                .header(header::HOST, "plamenu.test")
                .header(header::ORIGIN, "https://plamenu.test")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("display_name=Browser+Guest&accept_risk=yes"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(join.status(), StatusCode::SEE_OTHER);
    let set_cookie = join.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(set_cookie.contains("Secure; HttpOnly; SameSite=Lax"));
    let cookie = set_cookie.split(';').next().unwrap();
    let raw_token = cookie.split_once('=').unwrap().1;
    let guest = db::guest_by_token(&pool, session.id, &plamenu::auth::hash_secret(raw_token))
        .await
        .unwrap()
        .unwrap();
    assert!(guest.accepted);
    assert_eq!(guest.display_name, "Browser Guest");

    let play = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/webxdc/session/{}/play", session.id))
                .header(header::HOST, "plamenu.test")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(play.status(), StatusCode::OK);
    let play_html = String::from_utf8(
        play.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(play_html.contains("sandbox=\"allow-scripts allow-same-origin allow-pointer-lock\""));
    assert!(play_html.contains(&format!("{}.webxdc.plamenu.test", session.id)));
    assert!(play_html.contains(&format!("{}.webxdc.plamenu.test/index.html?", session.id)));
    assert!(!play_html.contains(&format!("/webxdc-runtime/{}/index.html", session.id)));

    let csrf = plamenu::auth::hash_secret(&format!("plamenu-webxdc-guest-csrf:{raw_token}"));
    let update = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/web/webxdc/{}/updates", session.id))
                .header(header::HOST, "plamenu.test")
                .header(header::COOKIE, cookie)
                .header("x-csrf-token", csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"payload":{"item":"Milk"}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update.status(), StatusCode::ACCEPTED);
    let stored = db::all_updates(&pool, session.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].actor_uri, guest.participant_uri);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admission_boundary_replay_and_live_fanout_share_the_durable_log(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    let first = protocol::submit_update(
        &state,
        &session,
        &alice,
        json!({"payload": {"move": "e2e4"}}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.serial, 1);

    let remote = RemoteUser::new("remote.example", "bob");
    let bob = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    let follow_id = format!("{}/activities/join", remote.actor.id);
    db::request_membership(
        &pool,
        session.id,
        bob.id,
        &remote.actor.id,
        &follow_id,
        "remote-session-pseudonym",
    )
    .await
    .unwrap();
    let accepted = protocol::approve(&state, &session, bob.id).await.unwrap();
    assert_eq!(accepted.replay_boundary, 1);
    assert!(accepted.accepted);

    let replay_jobs = job::claim_due(&pool, 10).await.unwrap();
    assert_eq!(replay_jobs.len(), 2);
    assert!(
        replay_jobs
            .iter()
            .any(|queued| queued.activity["type"] == "Accept")
    );
    assert!(replay_jobs.iter().any(|queued| {
        queued.activity["type"] == "Announce" && queued.activity["webxdcSerial"] == 1
    }));

    let second = protocol::submit_update(
        &state,
        &session,
        &alice,
        json!({"payload": {"move": "e7e5"}}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.serial, 2);
    let live = job::claim_due(&pool, 10).await.unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].activity["type"], "Announce");
    assert_eq!(live[0].activity["webxdcSerial"], 2);
    assert_eq!(live[0].inbox_url, remote.actor.inbox);

    assert!(
        db::has_complete_prefix(&pool, session.id, accepted.replay_boundary)
            .await
            .unwrap()
    );
    let stored = db::all_updates(&pool, session.id).await.unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|update| update.serial)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signed_inbox_requires_the_marker_sequences_once_and_honors_undo(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);
    let bytes = package();
    plamenu_db::user::create(&pool, alice.id, None, "unused")
        .await
        .unwrap();
    let session = protocol::create_local(
        &state,
        protocol::CreateLocal {
            creator: &alice,
            name: "Inbox game",
            summary: "signed protocol traffic",
            bundle_name: "inbox.xdc",
            bundle_bytes: &bytes,
            membership_policy: "open",
            send_update_interval: 0,
            send_update_max_size: 32_768,
        },
    )
    .await
    .unwrap();
    let app = build_router(state.clone());
    let inbox = format!("/webxdc/{}/inbox", session.id);
    let follow_id = format!("{}/activities/follow-webxdc", bob.actor.id);

    let unmarked = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": follow_id,
        "type": "Follow",
        "actor": bob.actor.id,
        "object": session.coordinator_uri,
        "context": session.coordinator_uri,
        "audience": session.coordinator_uri,
        "to": session.coordinator_uri,
    });
    assert_eq!(
        post_signed(&app, &inbox, &unmarked, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let bob_account = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        db::membership(&pool, session.id, bob_account.id)
            .await
            .unwrap()
            .is_none()
    );
    let rejection = job::claim_due(&pool, 10).await.unwrap();
    assert_eq!(rejection.len(), 1);
    assert_eq!(rejection[0].activity["type"], "Reject");
    job::complete(&pool, rejection[0].id).await.unwrap();

    let mut follow = unmarked;
    follow["webxdcProtocol"] = json!(protocol::PROTOCOL);
    assert_eq!(
        post_signed(&app, &inbox, &follow, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let membership = db::membership(&pool, session.id, bob_account.id)
        .await
        .unwrap()
        .unwrap();
    assert!(membership.accepted);

    let update = json!({
        "@context": ["https://www.w3.org/ns/activitystreams", protocol::CONTEXT],
        "id": format!("{}/activities/move-1", bob.actor.id),
        "type": "Create",
        "actor": bob.actor.id,
        "to": session.coordinator_uri,
        "context": session.coordinator_uri,
        "audience": session.coordinator_uri,
        "object": {
            "id": format!("{}/webxdc-updates/move-1", bob.actor.id),
            "type": "WebxdcUpdate",
            "attributedTo": bob.actor.id,
            "context": session.coordinator_uri,
            "audience": session.coordinator_uri,
            "webxdcUpdate": {"payload": {"move": "e2e4"}}
        }
    });
    assert_eq!(
        post_signed(&app, &inbox, &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(&app, &inbox, &update, &bob.signer()).await,
        StatusCode::ACCEPTED,
        "redelivery is idempotent"
    );
    let updates = db::all_updates(&pool, session.id).await.unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].serial, 1);

    let undo = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{follow_id}/undo"),
        "type": "Undo",
        "actor": bob.actor.id,
        "object": follow_id,
        "to": session.coordinator_uri,
        "audience": session.coordinator_uri,
    });
    assert_eq!(
        post_signed(&app, &inbox, &undo, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(
        db::membership(&pool, session.id, bob_account.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn owner_delete_federates_then_purges_application_state_and_serves_gone(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    protocol::submit_update(
        &state,
        &session,
        &alice,
        json!({"payload": {"move": "e2e4"}}),
    )
    .await
    .unwrap();
    db::create_guest(
        &pool,
        plamenu_db::id::next(),
        session.id,
        "Temporary guest",
        "guest-token-hash",
        &format!("{}#guest-test", session.coordinator_uri),
        "guest-self-address",
        true,
    )
    .await
    .unwrap();

    let bob = RemoteUser::new("remote.example", "delete-recipient");
    let bob_account = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    db::request_membership(
        &pool,
        session.id,
        bob_account.id,
        &bob.actor.id,
        &format!("{}/activities/webxdc-follow", bob.actor.id),
        "remote-self-address",
    )
    .await
    .unwrap();
    protocol::approve(&state, &session, bob_account.id)
        .await
        .unwrap();
    for queued in job::claim_due(&pool, 10).await.unwrap() {
        job::complete(&pool, queued.id).await.unwrap();
    }

    let invitation_text = format!("Join before cleanup: {}", session.coordinator_uri);
    let (invitation, _) = plamenu::actions::post_webxdc_invitation(
        &state,
        PostParams {
            username: &alice.username,
            text: &invitation_text,
            visibility: "public",
            ..PostParams::default()
        },
        WebxdcInvitation {
            session_id: session.id,
            session_uri: &session.coordinator_uri,
            session_name: &session.name,
        },
    )
    .await
    .unwrap();

    protocol::delete_local(&state, &session).await.unwrap();

    assert!(db::find(&pool, session.id).await.unwrap().is_none());
    let retained_coordinator = account::find_by_id(&pool, session.account_id)
        .await
        .unwrap()
        .expect("the signer remains while Delete is queued");
    assert!(retained_coordinator.suspended());
    assert!(
        account::is_deleted(&pool, session.account_id)
            .await
            .unwrap()
    );
    assert!(
        account::is_internal(&pool, session.account_id)
            .await
            .unwrap()
    );
    let tombstone = db::tombstone_by_id(&pool, session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tombstone.coordinator_uri, session.coordinator_uri);
    assert_eq!(
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM webxdc_sessions WHERE id = $1"#,
            session.id,
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    let child_counts = sqlx::query!(
        r#"SELECT
             (SELECT count(*) FROM webxdc_files WHERE session_id = $1) AS "files!",
             (SELECT count(*) FROM webxdc_updates WHERE session_id = $1) AS "updates!",
             (SELECT count(*) FROM webxdc_memberships WHERE session_id = $1) AS "memberships!",
             (SELECT count(*) FROM webxdc_guests WHERE session_id = $1) AS "guests!""#,
        session.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (
            child_counts.files,
            child_counts.updates,
            child_counts.memberships,
            child_counts.guests,
        ),
        (0, 0, 0, 0)
    );
    assert!(
        sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM statuses WHERE id = $1) AS "exists!""#,
            invitation.id,
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        "the independently authored invitation Note remains"
    );
    assert!(
        db::invitations_for_statuses(&pool, &[invitation.id])
            .await
            .unwrap()
            .is_empty(),
        "the deleted session's enhancement sidecar is removed"
    );

    let deletion_jobs = job::claim_due(&pool, 10).await.unwrap();
    assert_eq!(deletion_jobs.len(), 1);
    assert_eq!(deletion_jobs[0].activity["type"], "Delete");
    assert_eq!(deletion_jobs[0].activity["object"]["type"], "Tombstone");
    assert_eq!(deletion_jobs[0].account_id, Some(session.account_id));

    let app = build_router(state);
    let html_gone = response(
        &app,
        &format!("/webxdc/{}", session.id),
        "plamenu.test",
        Some("text/html"),
    )
    .await;
    assert_eq!(html_gone.status(), StatusCode::GONE);
    assert_eq!(html_gone.headers()[header::VARY], "Accept");
    let html = String::from_utf8(
        html_gone
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Session deleted"));
    let ap_gone = response(
        &app,
        &format!("/webxdc/{}", session.id),
        "plamenu.test",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(ap_gone.status(), StatusCode::GONE);
    assert_eq!(ap_gone.headers()[header::VARY], "Accept");
    assert_eq!(
        ap_gone.headers()[header::CONTENT_TYPE],
        plamenu_ap::ACTIVITY_JSON_UTF8
    );
    let tombstone: Value =
        serde_json::from_slice(&ap_gone.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(tombstone["type"], "Tombstone");
    assert_eq!(tombstone["id"], session.coordinator_uri);
    assert!(tombstone["deleted"].as_str().is_some());

    assert_eq!(
        db::prune_tombstoned_coordinator_accounts(
            &pool,
            time::OffsetDateTime::now_utc() + time::Duration::days(1),
            10,
        )
        .await
        .unwrap(),
        0,
        "a queued delivery keeps its signing account"
    );
    job::complete(&pool, deletion_jobs[0].id).await.unwrap();
    assert_eq!(
        db::prune_tombstoned_coordinator_accounts(
            &pool,
            time::OffsetDateTime::now_utc() + time::Duration::days(1),
            10,
        )
        .await
        .unwrap(),
        1
    );
    assert!(
        account::find_by_id(&pool, session.account_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db::tombstone_by_id(&pool, session.id)
            .await
            .unwrap()
            .is_some(),
        "signer cleanup must retain the protocol tombstone"
    );
    assert!(
        account::local_username_reserved(&pool, &format!("webxdc_{}", session.id))
            .await
            .unwrap(),
        "the synthetic handle remains reserved after account cleanup"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn coordinator_accounts_stay_out_of_generic_account_and_group_surfaces(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    let coordinator = account::find_by_id(&pool, session.account_id)
        .await
        .unwrap()
        .unwrap();
    assert!(account::is_internal(&pool, coordinator.id).await.unwrap());
    assert!(
        account::find_local_by_username(&pool, &coordinator.username)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        account::search(
            &pool,
            &AccountSearch {
                terms: &coordinator.username,
                viewer: Some(alice.id),
                following: false,
                limit: 20,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        admin_account::list(
            &pool,
            &admin_account::AdminAccountFilter {
                username: Some("webxdc_%".into()),
                limit: 40,
                ..admin_account::AdminAccountFilter::default()
            },
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert_eq!(group::count_local(&pool).await.unwrap(), 0);
    assert!(
        group::public_group_ids(&pool, 40, 0)
            .await
            .unwrap()
            .is_empty()
    );

    let username = coordinator.username;
    let app = build_router(state);
    assert_eq!(
        response(
            &app,
            &format!("/!{username}"),
            "plamenu.test",
            Some("text/html")
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        response(
            &app,
            &format!("/users/{username}"),
            "plamenu.test",
            Some("application/activity+json"),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        response(
            &app,
            &format!("/.well-known/webfinger?resource=acct%3A{username}%40plamenu.test"),
            "plamenu.test",
            Some("application/jrd+json"),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        response(
            &app,
            &format!("/api/v1/accounts/{}", session.account_id),
            "plamenu.test",
            Some("application/json"),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        response(
            &app,
            &format!("/webxdc/{}", session.id),
            "plamenu.test",
            Some("application/activity+json"),
        )
        .await
        .status(),
        StatusCode::OK,
        "the canonical Webxdc actor remains available"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn lifecycle_hides_idle_session_then_purges_it_after_the_grace_period(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    sqlx::query!(
        "UPDATE webxdc_sessions
         SET last_activity_at = now() - interval '91 days'
         WHERE id = $1",
        session.id,
    )
    .execute(&pool)
    .await
    .unwrap();

    plamenu::maintenance::webxdc_lifecycle(&state).await;
    let closed = db::find(&pool, session.id).await.unwrap().unwrap();
    assert!(closed.ended());
    assert!(
        db::list_for_participant(&pool, alice.id, false)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db::list_for_participant(&pool, alice.id, true)
            .await
            .unwrap()
            .len(),
        1
    );

    sqlx::query!(
        "UPDATE webxdc_sessions SET ended_at = now() - interval '31 days' WHERE id = $1",
        session.id,
    )
    .execute(&pool)
    .await
    .unwrap();
    plamenu::maintenance::webxdc_lifecycle(&state).await;
    assert!(db::find(&pool, session.id).await.unwrap().is_none());
    assert!(
        db::tombstone_by_id(&pool, session.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn storage_quota_refuses_an_update_without_allocating_a_serial(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    let limits = db::Limits {
        bundle_mb: 1,
        expanded_mb: 1,
        file_mb: 1,
        session_mb: 3,
        account_mb: 3,
        ..db::Limits::default()
    };
    db::set_limits(&pool, limits).await.unwrap();
    let quota = i64::from(limits.session_mb) * 1024 * 1024;
    sqlx::query!(
        "UPDATE webxdc_sessions SET storage_bytes = $2 WHERE id = $1",
        session.id,
        quota - 1,
    )
    .execute(&pool)
    .await
    .unwrap();
    let error = protocol::submit_update(
        &state,
        &session,
        &alice,
        json!({"payload": {"move": "quota-test"}}),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        plamenu::error::ApiError::PayloadTooLargeWithMessage(_)
    ));
    let retained = sqlx::query!(
        "SELECT last_serial, storage_bytes FROM webxdc_sessions WHERE id = $1",
        session.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained.last_serial, 0);
    assert_eq!(retained.storage_bytes, quota - 1);
    assert!(db::all_updates(&pool, session.id).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn leaving_the_last_local_remote_membership_sends_undo_and_drops_the_cache(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::<StubFederation>::default());
    let (_coordinator, session) = cached_remote_session(&pool, &alice).await;

    protocol::leave(&state, &session, &alice).await.unwrap();

    assert!(db::find(&pool, session.id).await.unwrap().is_none());
    assert!(
        account::find_by_id(&pool, session.account_id)
            .await
            .unwrap()
            .is_none(),
        "discarding a remote cache also removes its internal coordinator account"
    );
    assert!(
        db::tombstone_by_uri(&pool, &session.coordinator_uri)
            .await
            .unwrap()
            .is_none(),
        "local cache eviction must not claim the remote actor was deleted"
    );
    let queued = job::claim_due(&pool, 10).await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].activity["type"], "Undo");
    assert_eq!(queued[0].account_id, Some(alice.id));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authenticated_remote_delete_is_idempotent_and_purges_the_cached_session(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (coordinator, session) = cached_remote_session(&pool, &alice).await;
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([coordinator.actor.clone()]),
    );
    let app = build_router(state);
    let deleted = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let activity = json!({
        "@context": ["https://www.w3.org/ns/activitystreams", protocol::CONTEXT],
        "id": format!("{}/activities/delete", session.coordinator_uri),
        "type": "Delete",
        "actor": session.coordinator_uri,
        "object": {
            "id": session.coordinator_uri,
            "type": "Tombstone",
            "formerType": ["Group", "WebxdcSession"],
            "deleted": deleted,
        },
        "to": format!("{}/followers", session.coordinator_uri),
        "audience": session.coordinator_uri,
        "webxdcProtocol": protocol::PROTOCOL,
    });
    assert_eq!(
        post_signed(&app, "/inbox", &activity, &coordinator.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(&app, "/inbox", &activity, &coordinator.signer()).await,
        StatusCode::ACCEPTED,
        "a Delete redelivery is consumed by the retained tombstone"
    );
    assert!(db::find(&pool, session.id).await.unwrap().is_none());
    assert!(
        db::tombstone_by_uri(&pool, &session.coordinator_uri)
            .await
            .unwrap()
            .is_some()
    );
    let local_cache = response(
        &app,
        &format!("/webxdc/session/{}", session.id),
        "plamenu.test",
        Some("text/html"),
    )
    .await;
    assert_eq!(local_cache.status(), StatusCode::GONE);
}

async fn ephemeral_guest(
    pool: &PgPool,
    state: &plamenu::AppState,
    session: &db::Session,
    name: &str,
) -> String {
    let token = plamenu::auth::generate_secret();
    let guest_id = plamenu_db::id::next();
    db::create_guest(
        pool,
        guest_id,
        session.id,
        name,
        &plamenu::auth::hash_secret(&token),
        &format!("{}#guest-{guest_id}", session.coordinator_uri),
        name,
        true,
    )
    .await
    .unwrap();
    state.webxdc_realtime.refresh(session.id);
    format!("__Host-plamenu_webxdc_guest_{}={token}", session.id)
}

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ephemeral_connect(
    address: std::net::SocketAddr,
    session_id: i64,
    cookie: &str,
) -> TestSocket {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};
    let mut request = format!("ws://{address}/webxdc/{session_id}/realtime")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(header::HOST, "plamenu.test".parse().unwrap());
    request
        .headers_mut()
        .insert(header::ORIGIN, "https://plamenu.test".parse().unwrap());
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    // Pong proves the upgraded server has registered this connection.
    socket.send(Message::Ping(vec![1].into())).await.unwrap();
    let pong = tokio::time::timeout(std::time::Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(pong, Message::Pong(_)));
    socket
}

async fn ephemeral_bytes(socket: &mut TestSocket) -> Vec<u8> {
    use futures_util::StreamExt as _;
    loop {
        match socket.next().await.unwrap().unwrap() {
            tokio_tungstenite::tungstenite::Message::Binary(bytes) => return bytes.to_vec(),
            tokio_tungstenite::tungstenite::Message::Close(frame) => {
                panic!("unexpected close: {frame:?}")
            }
            _ => {}
        }
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ephemeral_websockets_deliver_binary_without_storage_or_replay_and_close_on_revocation(
    pool: PgPool,
) {
    use futures_util::StreamExt as _;
    use plamenu::webxdc_realtime::{self as realtime, Peer};
    use std::time::Duration;
    let (state, alice, session) = local_session(&pool).await;
    let cookie = ephemeral_guest(&pool, &state, &session, "guest").await;
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let mut guest = ephemeral_connect(address, session.id, &cookie).await;
    let data = vec![0, 1, 127, 128, 255];
    realtime::submit(
        &state,
        &session.coordinator_uri,
        Peer::Account(alice.id),
        data.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), ephemeral_bytes(&mut guest))
            .await
            .unwrap(),
        data
    );
    assert!(db::all_updates(&pool, session.id).await.unwrap().is_empty());
    assert!(job::claim_due(&pool, 20).await.unwrap().is_empty());
    assert_eq!(
        db::find(&pool, session.id)
            .await
            .unwrap()
            .unwrap()
            .last_serial,
        0
    );
    guest.close(None).await.unwrap();
    // This packet must not appear on a fresh connection.
    realtime::submit(
        &state,
        &session.coordinator_uri,
        Peer::Account(alice.id),
        vec![99],
    )
    .await
    .unwrap();
    let mut rejoined = ephemeral_connect(address, session.id, &cookie).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), ephemeral_bytes(&mut rejoined))
            .await
            .is_err()
    );
    realtime::submit(
        &state,
        &session.coordinator_uri,
        Peer::Account(alice.id),
        vec![42],
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), ephemeral_bytes(&mut rejoined))
            .await
            .unwrap(),
        [42]
    );
    protocol::close_local(&state, &session).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(3), rejoined.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        closed,
        tokio_tungstenite::tungstenite::Message::Close(_)
    ));
    assert!(
        realtime::submit(
            &state,
            &session.coordinator_uri,
            Peer::Account(alice.id),
            vec![1]
        )
        .await
        .is_err()
    );
    server.abort();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ephemeral_signed_federation_authenticates_and_bypasses_durable_jobs(pool: PgPool) {
    use plamenu::webxdc_realtime as realtime;
    let (mut state, alice, session) = local_session(&pool).await;
    let federation = Arc::new(StubFederation::default());
    state.federation = federation.clone();
    let remote = RemoteUser::new("remote.example", "ephemeral-bob");
    let bob = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    db::request_membership(
        &pool,
        session.id,
        bob.id,
        &remote.actor.id,
        &format!("{}/follow", remote.actor.id),
        "bob-self",
    )
    .await
    .unwrap();
    protocol::approve(&state, &session, bob.id).await.unwrap();
    for queued in job::claim_due(&pool, 20).await.unwrap() {
        job::complete(&pool, queued.id).await.unwrap();
    }
    let app = build_router(state.clone());
    let raw = realtime::packet(&session.coordinator_uri, &remote.actor.id, &[0, 255]);
    let signer = remote.signer();
    assert_eq!(
        post_signed(&app, "/inbox", &raw, &signer).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(&app, "/inbox", &raw, &signer).await,
        StatusCode::ACCEPTED
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while federation.deliveries().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let sent = federation.deliveries();
    assert_eq!(sent.len(), 1, "duplicate Create must not fan out twice");
    assert_eq!(sent[0].activity["object"], raw);
    assert_eq!(sent[0].activity["actor"], session.coordinator_uri);
    assert!(sent[0].activity.get("webxdcSerial").is_none());
    assert_eq!(sent[0].activity["endTime"], raw["endTime"]);
    assert!(
        !sent[0].try_rfc9421,
        "ephemeral delivery must not double-knock"
    );
    assert!(job::claim_due(&pool, 20).await.unwrap().is_empty());
    assert!(db::all_updates(&pool, session.id).await.unwrap().is_empty());
    let mut forged = realtime::packet(&session.coordinator_uri, &remote.actor.id, &[1]);
    forged["actor"] = json!(plamenu::entities::account_uri("plamenu.test", &alice));
    assert_eq!(
        post_signed(&app, "/inbox", &forged, &signer).await,
        StatusCode::UNAUTHORIZED
    );
    protocol::remove_participant(&state, &session, &bob)
        .await
        .unwrap();
    let removed = realtime::packet(&session.coordinator_uri, &remote.actor.id, &[2]);
    assert_eq!(
        post_signed(&app, "/inbox", &removed, &signer).await,
        StatusCode::FORBIDDEN
    );
    assert!(
        federation.fetches.lock().unwrap().is_empty(),
        "packet ingress must not refetch actors or payloads"
    );
}

async fn ephemeral_account_cookie(pool: &PgPool, account: &plamenu_db::account::Account) -> String {
    use plamenu_db::{oauth, user};
    let user = if let Some(existing) = user::find_by_account_id(pool, account.id).await.unwrap() {
        existing
    } else {
        user::create(
            pool,
            account.id,
            Some("realtime@example.com"),
            "unused-test-password-hash",
        )
        .await
        .unwrap()
    };
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "Plamenu",
            website: None,
            client_id: "plamenu-web-ui",
            client_secret_hash: "unused",
            redirect_uris: &[],
            scopes: "read write follow push",
        },
    )
    .await
    .unwrap();
    let token = plamenu::auth::generate_secret();
    oauth::create_token(
        pool,
        &plamenu::auth::hash_secret(&token),
        app.id,
        Some(user.id),
        "read write follow push",
    )
    .await
    .unwrap();
    format!("__Host-plamenu_session={token}")
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ephemeral_remote_host_accepts_only_its_coordinator_and_sends_without_a_job(pool: PgPool) {
    use futures_util::{SinkExt as _, StreamExt as _};
    use plamenu::webxdc_realtime as realtime;
    use std::time::Duration;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let cookie = ephemeral_account_cookie(&pool, &alice).await;
    let (coordinator, session) = cached_remote_session(&pool, &alice).await;
    let federation = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), federation.clone());
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            served.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let mut socket = ephemeral_connect(address, session.id, &cookie).await;
    let inner = realtime::packet(
        &session.coordinator_uri,
        "https://other.example/users/bob",
        &[1, 255],
    );
    let announced = json!({
        "id": format!("{}/ephemeral/1", session.coordinator_uri),
        "type": "Announce", "actor": session.coordinator_uri,
        "webxdcProtocol": protocol::PROTOCOL, "audience": session.coordinator_uri,
        "to": format!("{}/followers", session.coordinator_uri),
        "published": inner["published"], "endTime": inner["endTime"], "object": inner,
    });
    assert_eq!(
        post_signed(&app, "/inbox", &announced, &coordinator.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), ephemeral_bytes(&mut socket))
            .await
            .unwrap(),
        [1, 255]
    );
    assert_eq!(
        post_signed(&app, "/inbox", &announced, &coordinator.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), ephemeral_bytes(&mut socket))
            .await
            .is_err()
    );
    let mut extended = announced.clone();
    extended["endTime"] = json!("2099-01-01T00:00:00Z");
    assert_eq!(
        post_signed(&app, "/inbox", &extended, &coordinator.signer()).await,
        StatusCode::FORBIDDEN
    );
    socket
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            vec![0, 128, 255].into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while federation.deliveries().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let sent = federation.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].activity["type"], "Create");
    assert_eq!(sent[0].activity["object"]["webxdcData"], "AID/");
    assert_eq!(sent[0].inbox_url, coordinator.actor.inbox);
    assert!(db::all_updates(&pool, session.id).await.unwrap().is_empty());
    assert!(job::claim_due(&pool, 20).await.unwrap().is_empty());
    let sender = account::find_by_id(&pool, session.account_id)
        .await
        .unwrap()
        .unwrap();
    let remove = json!({"type": "Remove", "actor": session.coordinator_uri,
        "object": plamenu::entities::account_uri("plamenu.test", &alice),
        "audience": session.coordinator_uri, "target": format!("{}/followers", session.coordinator_uri)});
    assert!(
        protocol::handle_remove(&state, &sender, &remove)
            .await
            .unwrap()
    );
    let closed = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        closed,
        tokio_tungstenite::tungstenite::Message::Close(_)
    ));
    server.abort();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cached_remote_app_runs_on_its_local_isolated_origin(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (_, session) = cached_remote_session(&pool, &alice).await;
    let app = build_router(test_state_with(pool, Arc::<StubFederation>::default()));
    let host = format!("{}.webxdc.plamenu.test", session.id);
    for path in ["/index.html", "/webxdc.js", "/assets/app.js"] {
        let result = response(&app, path, &host, None).await;
        assert_eq!(result.status(), StatusCode::OK, "{path}");
        assert!(result.headers().get(header::X_FRAME_OPTIONS).is_none());
    }
    let allowed = response(
        &app,
        &format!("/webxdc/caddy-allow?domain={host}"),
        "plamenu.test",
        None,
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::NO_CONTENT);
    // Caching a remote app must not make this server impersonate its actor.
    let actor = response(
        &app,
        &format!("/webxdc/{}", session.id),
        "plamenu.test",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(actor.status(), StatusCode::NOT_FOUND);
    let wrong_origin = response(
        &app,
        &format!("/webxdc-runtime/{}/index.html", session.id),
        "plamenu.test",
        None,
    )
    .await;
    assert_eq!(wrong_origin.status(), StatusCode::NOT_FOUND);
}

fn compose_multipart(fields: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (key, value) in fields {
        write!(
            body,
            "--xdc-ui\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n"
        )
        .unwrap();
    }
    body.push_str("--xdc-ui--\r\n");
    body
}

#[sqlx::test(migrations = "../db/migrations")]
async fn full_composer_previews_and_posts_an_invitation_then_edit_can_remove_it(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    let cookie = ephemeral_account_cookie(&pool, &alice).await;
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let app = build_router(state.clone());
    let page = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/compose?webxdc={}", session.id))
                .header(header::HOST, "plamenu.test")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let html = String::from_utf8(
        page.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("action=\"/web/compose\""));
    assert!(html.contains(&format!("Join me in Chess\n\n{}", session.coordinator_uri)));
    assert!(html.contains("App invitation"));
    assert!(html.contains("scheduled_at"));
    let text = format!("Join my board\n\n{}", session.coordinator_uri);
    for op in ["preview", "post"] {
        let result = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/web/compose")
                    .header(header::HOST, "plamenu.test")
                    .header(header::ORIGIN, "https://plamenu.test")
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "multipart/form-data; boundary=xdc-ui")
                    .body(Body::from(compose_multipart(&[
                        ("csrf", &csrf),
                        ("status", &text),
                        ("visibility", "public"),
                        ("content_type", "text/plain"),
                        ("quote_policy", "public"),
                        ("op", op),
                    ])))
                    .unwrap(),
            )
            .await
            .unwrap();
        if op == "preview" {
            assert_eq!(result.status(), StatusCode::OK);
            let html = String::from_utf8(
                result
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .to_vec(),
            )
            .unwrap();
            assert!(html.contains("App invitation"));
            assert!(html.contains(&session.coordinator_uri));
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM webxdc_invitations")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "preview must not post");
        } else {
            assert_eq!(result.status(), StatusCode::SEE_OTHER);
            assert!(
                result.headers()[header::LOCATION]
                    .to_str()
                    .unwrap()
                    .starts_with("/@alice/")
            );
        }
    }
    let id: i64 =
        sqlx::query_scalar("SELECT status_id FROM webxdc_invitations WHERE session_id=$1")
            .bind(session.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let stored = plamenu_db::status::find_by_id(&pool, id)
        .await
        .unwrap()
        .unwrap();
    let entity = plamenu::entities::render_status(&pool, "plamenu.test", &stored, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(entity["webxdc_invitation"]["name"], "Chess");
    assert!(
        entity["content"]
            .as_str()
            .unwrap()
            .contains(&session.coordinator_uri)
    );
    let note = plamenu::note::note_for_status(&state, &stored, &alice)
        .await
        .unwrap();
    assert_eq!(note["attachment"][0]["rel"], protocol::OPEN_REL);
    let edited = plamenu::actions::edit_status(
        &state,
        &alice,
        id,
        plamenu::actions::EditParams {
            text: Some("Invitation withdrawn"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let entity = plamenu::entities::render_status(&pool, "plamenu.test", &edited, Some(alice.id))
        .await
        .unwrap();
    assert!(entity["webxdc_invitation"].is_null());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_invitation_card_does_not_download_or_start_the_app(pool: PgPool) {
    let remote = RemoteUser::new("remote.example", "alice");
    let author = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    let state = test_state_with(pool.clone(), Arc::<StubFederation>::default());
    let uri = "https://remote.example/webxdc/board";
    let mut object = json!({"id":"https://remote.example/notes/invite","type":"Note","attributedTo":remote.actor.id,"to":["https://www.w3.org/ns/activitystreams#Public"],"content":format!("<p>Join <a href=\"{uri}\">this board</a></p>"),"published":"2026-09-08T00:00:00Z"});
    protocol::enhance_invitation(&mut object, uri, "Shared board");
    let stored = plamenu::ingest::ingest_remote_note(&state, &author, &object)
        .await
        .unwrap();
    let entity = plamenu::entities::render_status(&pool, "plamenu.test", &stored, None)
        .await
        .unwrap();
    assert_eq!(entity["webxdc_invitation"]["name"], "Shared board");
    assert_eq!(entity["webxdc_invitation"]["url"], uri);
    assert!(
        entity["webxdc_invitation"]["open_url"]
            .as_str()
            .unwrap()
            .starts_with("/webxdc/open?url=")
    );
    assert!(
        db::find_by_uri(&pool, uri).await.unwrap().is_none(),
        "viewing an invitation must not fetch the bundle"
    );
    assert!(
        stored.external_url.is_none(),
        "avoid rendering the invitation twice as a generic link post"
    );
    object["attachment"] = json!([]);
    plamenu::ingest::update_remote_note(&state, &author, &stored, &object)
        .await
        .unwrap();
    let entity = plamenu::entities::render_status(&pool, "plamenu.test", &stored, None)
        .await
        .unwrap();
    assert!(entity["webxdc_invitation"].is_null());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scheduled_invitation_uses_the_same_posting_path(pool: PgPool) {
    let (state, alice, session) = local_session(&pool).await;
    let text = format!("Join this app: {}", session.coordinator_uri);
    plamenu_db::scheduled_status::create(
        &pool,
        plamenu_db::scheduled_status::NewScheduledStatus {
            account_id: alice.id,
            scheduled_at: time::OffsetDateTime::now_utc() - time::Duration::seconds(1),
            text: &text,
            object_type: "Note",
            title: None,
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
            quote_approval_policy: Some(0),
        },
    )
    .await
    .unwrap();
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 1);
    let status_id: i64 =
        sqlx::query_scalar("SELECT status_id FROM webxdc_invitations WHERE session_id=$1")
            .bind(session.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let stored = plamenu_db::status::find_by_id(&pool, status_id)
        .await
        .unwrap()
        .unwrap();
    let note = plamenu::note::note_for_status(&state, &stored, &alice)
        .await
        .unwrap();
    assert_eq!(note["attachment"][0]["rel"], protocol::OPEN_REL);
    assert!(
        note["content"]
            .as_str()
            .unwrap()
            .contains(&session.coordinator_uri)
    );
}

fn upload_multipart(csrf: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = format!("--xdc-upload\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\n{csrf}\r\n--xdc-upload\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nLarge app\r\n--xdc-upload\r\nContent-Disposition: form-data; name=\"bundle\"; filename=\"game.xdc\"\r\nContent-Type: application/webxdc+zip\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n--xdc-upload--\r\n");
    body
}

async fn upload_package(
    app: &axum::Router,
    cookie: &str,
    bytes: &[u8],
) -> axum::response::Response {
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    // HTTP arrives in chunks; let the form's text fields reach the parser
    // before a later package chunk crosses the limit.
    let chunks: Vec<_> = upload_multipart(&csrf, bytes)
        .chunks(16 * 1024)
        .map(|chunk| Ok::<_, std::convert::Infallible>(axum::body::Bytes::copy_from_slice(chunk)))
        .collect();
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/web/webxdc")
                .header(header::HOST, "plamenu.test")
                .header(header::COOKIE, cookie)
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=xdc-upload",
                )
                .body(Body::from_stream(futures_util::StreamExt::then(
                    futures_util::stream::iter(chunks.into_iter().enumerate()),
                    |(index, chunk)| async move {
                        if index > 0 {
                            tokio::task::yield_now().await;
                        }
                        chunk
                    },
                )))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn upload_limits_are_live_and_multipart_failures_preserve_the_form(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::<StubFederation>::default());
    let cookie = ephemeral_account_cookie(&pool, &alice).await;
    let app = build_router(state);
    db::set_limits(
        &pool,
        db::Limits {
            bundle_mb: 1,
            ..db::Limits::default()
        },
    )
    .await
    .unwrap();
    // Cross the whole multipart limit, reproducing Axum's nested body-limit
    // error rather than only the handler's package-length check.
    let response = upload_package(&app, &cookie, &vec![0; 3 * 1024 * 1024]).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("at most 1 MiB"));
    assert!(html.contains("value=\"Large app\""));
    assert!(html.contains("role=\"alert\""));
    assert!(!html.contains("Error parsing multipart"));

    let response = upload_package(&app, &cookie, b"invalid archive").await;
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = std::str::from_utf8(&body).unwrap();
    let alert = html
        .split("role=\"alert\">")
        .nth(1)
        .unwrap_or("")
        .split("</p>")
        .next()
        .unwrap_or("");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{alert}");
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("not a valid ZIP")
    );

    db::set_limits(&pool, db::Limits::default()).await.unwrap();
    // A stored entry avoids the compression-ratio guard and crosses the old
    // compressed and per-file limits without checking in a large fixture.
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "index.html",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
    writer.write_all(&vec![b' '; 17 * 1024 * 1024]).unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let response = upload_package(&app, &cookie, &bytes).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let session_id: i64 = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let session = db::find(&pool, session_id).await.unwrap().unwrap();
    assert_eq!(db::bundle(&pool, session.id).await.unwrap().unwrap(), bytes);
    let storage: i64 = sqlx::query_scalar("SELECT storage_bytes FROM webxdc_sessions WHERE id=$1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(storage > 34 * 1024 * 1024);
}

#[test]
fn package_limits_reject_expansion_and_individual_files_independently() {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for name in ["index.html", "game.data"] {
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(&vec![b' '; 1024 * 1024 + 1]).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    for (limits, expected) in [
        (
            db::Limits {
                file_mb: 1,
                ..db::Limits::default()
            },
            "individual file",
        ),
        (
            db::Limits {
                expanded_mb: 2,
                file_mb: 2,
                ..db::Limits::default()
            },
            "expanded package",
        ),
    ] {
        assert!(
            protocol::validate_package_with_limits(&bytes, None, limits)
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_webxdc_limits_save_independently_and_validate_storage_budget(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let cookie = ephemeral_account_cookie(&pool, &alice).await;
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let state = test_state_with(pool.clone(), Arc::<StubFederation>::default());
    let app = build_router(state);
    let before = plamenu_db::instance_settings::get(&pool).await.unwrap();
    for (staff, session_mb, expected) in [
        (false, 800, StatusCode::FORBIDDEN),
        (true, 10, StatusCode::SEE_OTHER),
        (true, 800, StatusCode::SEE_OTHER),
    ] {
        if staff {
            plamenu_db::role::assign_to_account(&pool, alice.id, Some(3))
                .await
                .unwrap();
        }
        let response = app.clone().oneshot(Request::builder().method("POST")
            .uri("/web/admin/settings").header(header::HOST, "plamenu.test")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("csrf={csrf}&section=webxdc&webxdc_bundle_mb=200&webxdc_expanded_mb=400&webxdc_file_mb=200&webxdc_session_mb={session_mb}&webxdc_account_mb=1600&webxdc_total_mb=10240")))
            .unwrap()).await.unwrap();
        assert_eq!(response.status(), expected);
        let saved = db::limits(&pool).await.unwrap();
        if staff && session_mb == 800 {
            assert_eq!(saved.bundle_mb, 200);
            assert_eq!(saved.session_mb, 800);
            assert_eq!(saved.account_mb, 1600);
        } else {
            assert_eq!(saved.bundle_mb, db::Limits::default().bundle_mb);
        }
    }
    let after = plamenu_db::instance_settings::get(&pool).await.unwrap();
    assert_eq!(after.max_characters, before.max_characters);
    assert_eq!(after.site_title, before.site_title);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/webxdc/new")
                .header(header::HOST, "plamenu.test")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("up to 200 MiB")
    );
}

async fn create_with_package(
    state: &plamenu::AppState,
    creator: &account::Account,
    bytes: &[u8],
) -> Result<db::Session, plamenu::error::ApiError> {
    protocol::create_local(
        state,
        protocol::CreateLocal {
            creator,
            name: "Storage test",
            summary: "",
            bundle_name: "test.xdc",
            bundle_bytes: bytes,
            membership_policy: "open",
            send_update_interval: 0,
            send_update_max_size: 32_768,
        },
    )
    .await
}

fn storage_package(length: usize, fill: u8) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file(
        "index.html",
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )
    .unwrap();
    zip.write_all(&vec![fill; length]).unwrap();
    zip.finish().unwrap().into_inner()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn identical_packages_share_storage_and_cleanup_preserves_other_sessions(pool: PgPool) {
    let (state, alice, first) = local_session(&pool).await;
    let bytes = db::bundle(&pool, first.id).await.unwrap().unwrap();
    let second = create_with_package(&state, &alice, &bytes).await.unwrap();
    let (_, remote) = cached_remote_session(&pool, &alice).await;
    assert_eq!(remote.digest_multibase, first.digest_multibase);
    let before = db::storage_usage(&pool, None).await.unwrap();
    assert_eq!(before.packages, 1);
    assert_eq!(before.sessions, 3);
    assert_eq!(before.data_bytes, 0);
    let owned = db::storage_usage(&pool, Some(alice.id)).await.unwrap();
    assert_eq!(owned.package_bytes, before.package_bytes);
    assert_eq!(owned.sessions, 2);
    protocol::submit_update(&state, &second, &alice, json!({"payload":{"move":1}}))
        .await
        .unwrap();
    assert!(db::all_updates(&pool, first.id).await.unwrap().is_empty());
    assert_eq!(db::all_updates(&pool, second.id).await.unwrap().len(), 1);
    assert!(db::storage_usage(&pool, None).await.unwrap().data_bytes > 0);
    protocol::delete_local(&state, &first).await.unwrap();
    assert_eq!(db::bundle(&pool, second.id).await.unwrap().unwrap(), bytes);
    assert!(
        db::file(&pool, second.id, "index.html")
            .await
            .unwrap()
            .is_some()
    );
    protocol::delete_local(&state, &second).await.unwrap();
    assert_eq!(db::storage_usage(&pool, None).await.unwrap().packages, 1);
    assert_eq!(
        db::storage_usage(&pool, Some(alice.id))
            .await
            .unwrap()
            .total_bytes(),
        0
    );
    protocol::evict_remote(&state, &remote).await.unwrap();
    assert_eq!(
        db::storage_usage(&pool, None).await.unwrap().total_bytes(),
        0
    );
    assert!(db::bundle(&pool, remote.id).await.unwrap().is_none());
    let undos: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Undo'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(undos > 0, "cache eviction must notify the coordinator");
    let files: i64 = sqlx::query_scalar("SELECT count(*) FROM webxdc_package_files")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(files, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn account_quota_counts_distinct_apps_and_server_quota_serializes_creations(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    plamenu_db::user::create(&pool, alice.id, None, "unused")
        .await
        .unwrap();
    let state = test_state_with(pool.clone(), Arc::<StubFederation>::default());
    db::set_limits(
        &pool,
        db::Limits {
            bundle_mb: 1,
            expanded_mb: 1,
            file_mb: 1,
            session_mb: 2,
            account_mb: 2,
            total_mb: 10,
        },
    )
    .await
    .unwrap();
    let bytes = storage_package(600_000, b'a');
    let first = create_with_package(&state, &alice, &bytes).await.unwrap();
    let second = create_with_package(&state, &alice, &bytes).await.unwrap();
    let other = storage_package(600_000, b'b');
    assert!(
        create_with_package(&state, &alice, &other)
            .await
            .unwrap_err()
            .to_string()
            .contains("account storage quota")
    );
    assert_eq!(db::storage_usage(&pool, None).await.unwrap().packages, 1);
    protocol::delete_local(&state, &first).await.unwrap();
    protocol::delete_local(&state, &second).await.unwrap();
    db::set_limits(
        &pool,
        db::Limits {
            bundle_mb: 1,
            expanded_mb: 1,
            file_mb: 1,
            session_mb: 2,
            account_mb: 2,
            total_mb: 1,
        },
    )
    .await
    .unwrap();
    let a = storage_package(280_000, b'a');
    let b = storage_package(280_000, b'b');
    let (one, two) = tokio::join!(
        create_with_package(&state, &alice, &a),
        create_with_package(&state, &alice, &b)
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    let rejected = one.err().or_else(|| two.err()).unwrap();
    assert!(rejected.to_string().contains("server storage quota"));
    assert_eq!(
        db::storage_usage(&pool, None).await.unwrap().packages,
        1,
        "failed allocations roll back"
    );
}

async fn admin_request(
    app: &axum::Router,
    cookie: &str,
    uri: &str,
    body: Option<String>,
) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(if body.is_some() { "POST" } else { "GET" })
                .uri(uri)
                .header(header::HOST, "plamenu.test")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body.unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn creation_permission_is_not_staff_and_admin_actions_require_permission_and_csrf(
    pool: PgPool,
) {
    use plamenu_db::role::{self, permission};
    let (state, alice, session) = local_session(&pool).await;
    let cookie = ephemeral_account_cookie(&pool, &alice).await;
    let app = build_router(state.clone());
    assert_eq!(
        admin_request(&app, &cookie, "/webxdc/new", None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        admin_request(&app, &cookie, "/admin/webxdc", None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let restricted = role::create(&pool, "No apps", "", 0, 0, false)
        .await
        .unwrap();
    role::assign_to_account(&pool, alice.id, Some(restricted.id))
        .await
        .unwrap();
    assert_eq!(
        admin_request(&app, &cookie, "/webxdc/new", None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        upload_package(&app, &cookie, &package()).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        create_with_package(&state, &alice, &package())
            .await
            .is_err()
    );
    let manager = role::create(
        &pool,
        "App manager",
        "",
        5,
        permission::MANAGE_WEBXDC,
        false,
    )
    .await
    .unwrap();
    role::assign_to_account(&pool, alice.id, Some(manager.id))
        .await
        .unwrap();
    let list = admin_request(&app, &cookie, "/admin/webxdc", None).await;
    assert_eq!(list.status(), StatusCode::OK);
    let html = String::from_utf8(
        list.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        html.contains("Chess") && html.contains("App storage") && html.contains("Session data")
    );
    let op = format!("/web/admin/webxdc/{}/op", session.id);
    assert_eq!(
        admin_request(&app, &cookie, &op, Some("op=delete&csrf=wrong".into()))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(db::find(&pool, session.id).await.unwrap().is_some());
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    assert_eq!(
        admin_request(&app, &cookie, &op, Some(format!("op=close&csrf={csrf}")))
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert!(db::find(&pool, session.id).await.unwrap().unwrap().ended());
    assert_eq!(
        admin_request(&app, &cookie, &op, Some(format!("op=delete&csrf={csrf}")))
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert!(db::find(&pool, session.id).await.unwrap().is_none());
    let logs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_action_logs WHERE target_type='WebxdcSession'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(logs, 2);
}

#[sqlx::test(migrations = false)]
async fn shared_storage_migration_preserves_existing_sessions_and_files(pool: PgPool) {
    // `./dev test` seeds template1 so thousands of ordinary sqlx tests do not
    // replay every migration. This upgrade test deliberately needs an empty
    // database, so make its already-isolated per-test database independent of
    // whether the surrounding runner uses that optimization.
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
        .execute(&pool)
        .await
        .unwrap();
    let old = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            plamenu_db::MIGRATOR
                .iter()
                .filter(|migration| migration.version < 70)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    old.run(&pool).await.unwrap();
    let first = create_local_account(&pool, "coordinator1", "").await;
    let second = create_local_account(&pool, "coordinator2", "").await;
    for account in [first, second] {
        sqlx::query("INSERT INTO webxdc_sessions (id,account_id,coordinator_uri,creator_uri,name,bundle_id,bundle_url,bundle_name,bundle_media_type,digest_multibase,bundle_bytes,storage_bytes)
            VALUES ($1,$1,$2,'https://example.test/alice','Migrated app','bundle','https://example.test/bundle','test.xdc','application/webxdc+zip','same-digest',$3,8)")
            .bind(account.id).bind(format!("https://example.test/sessions/{}",account.id)).bind(b"zip".as_slice()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO webxdc_files (session_id,path,media_type,bytes) VALUES ($1,'index.html','text/html',$2)")
            .bind(account.id).bind(b"hello".as_slice()).execute(&pool).await.unwrap();
    }
    sqlx::raw_sql(include_str!(
        "../../db/migrations/0070_webxdc_shared_storage.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let usage = db::storage_usage(&pool, None).await.unwrap();
    assert_eq!(usage.sessions, 2);
    assert_eq!(usage.packages, 1);
    assert_eq!(usage.total_bytes(), 8);
    let rows = db::admin_sessions(&pool, "", "", None, 10).await.unwrap();
    for row in rows {
        assert_eq!(db::bundle(&pool, row.id).await.unwrap().unwrap(), b"zip");
        assert_eq!(
            db::file(&pool, row.id, "index.html")
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"hello"
        );
    }
}
