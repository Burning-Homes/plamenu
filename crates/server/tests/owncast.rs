//! Owncast's federated Note → live HLS bridge, backed by the real database.

mod common;

use common::{RemoteUser, StubFederation, test_state_with};
use plamenu::ingest::ingest_remote_note;
use plamenu::owncast;
use plamenu::remote::resolve_remote_accounts;
use plamenu_ap::acct::Acct;
use plamenu_db::account::ActorClass;
use plamenu_db::{PgPool, media, remote_stream_source};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn owncast_stub() -> (RemoteUser, std::sync::Arc<StubFederation>) {
    let mut caster = RemoteUser::new("owncast.example", "inex");
    caster.actor.kind = "Service".into();
    caster.actor.url = Some(serde_json::json!("https://owncast.example/"));
    let stub = StubFederation::with_users(&[&caster]);
    stub.webfinger_hls.lock().unwrap().insert(
        caster.acct.clone(),
        "https://owncast.example/hls/stream.m3u8".into(),
    );
    stub.serve_page_as(
        "https://owncast.example/.well-known/nodeinfo",
        "https://owncast.example/.well-known/nodeinfo",
        "application/json",
        r#"{"links":[{"rel":"http://nodeinfo.diaspora.software/ns/schema/2.0","href":"https://owncast.example/nodeinfo/2.0"}]}"#,
    );
    stub.serve_page_as(
        "https://owncast.example/nodeinfo/2.0",
        "https://owncast.example/nodeinfo/2.0",
        "application/json",
        r#"{"software":{"name":"owncast","version":"0.3.0"},"protocols":["activitypub"],"metadata":{"federation":{"username":"inex"}}}"#,
    );
    (caster, stub)
}

fn serve_status(stub: &StubFederation, online: bool, connected_at: OffsetDateTime) {
    stub.serve_page_as(
        "https://owncast.example/api/status",
        "https://owncast.example/api/status",
        "application/json",
        &serde_json::json!({
            "online": online,
            "streamTitle": "Local Owncast test stream",
            "lastConnectTime": connected_at.format(&Rfc3339).unwrap(),
        })
        .to_string(),
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn live_note_becomes_hls_and_old_note_never_rearms_for_later_session(pool: PgPool) {
    let (_caster, stub) = owncast_stub();
    let state = test_state_with(pool.clone(), stub.clone());
    let acct: Acct = "inex@owncast.example".parse().unwrap();
    let author = resolve_remote_accounts(&state, &acct, ActorClass::PersonLike)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let source = remote_stream_source::find_by_account(&pool, author.id)
        .await
        .unwrap()
        .expect("NodeInfo-verified HLS capability retained");
    assert_eq!(source.homepage_url, "https://owncast.example/");
    assert_eq!(
        source.hls_master_url,
        "https://owncast.example/hls/stream.m3u8"
    );

    let connected = OffsetDateTime::now_utc() - time::Duration::minutes(2);
    let published = connected + time::Duration::minutes(2);
    serve_status(&stub, true, connected);
    let note = serde_json::json!({
        "id": "https://owncast.example/federation/notes/live-one",
        "type": "Note",
        "attributedTo": author.uri,
        "content": "<p>We're live!</p><p><a href=\"https://owncast.example/\">https://owncast.example/</a></p>",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "published": published.format(&Rfc3339).unwrap(),
        "attachment": [{
            "type": "Image",
            "mediaType": "image/gif",
            "name": "Live stream preview",
            "url": "https://owncast.example/preview.gif?us=session-one"
        }]
    });
    let stored = ingest_remote_note(&state, &author, &note).await.unwrap();
    let attachments = media::for_statuses(&pool, &[stored.id]).await.unwrap();
    let item = &attachments[&stored.id][0];
    assert_eq!(attachments[&stored.id].len(), 1);
    assert_eq!(item.kind_or_derived(), "video");
    assert_eq!(item.content_type, "application/x-mpegURL");
    assert_eq!(item.live_state.as_deref(), Some("live"));
    assert!(item.live_permanent);
    assert_eq!(
        item.hls_master_url.as_deref(),
        Some("https://owncast.example/hls/stream.m3u8")
    );
    assert_eq!(
        item.thumbnail_remote_url.as_deref(),
        Some("https://owncast.example/preview.gif?us=session-one")
    );

    // Offline is authoritative, but the same Note remains reconnectable only
    // for Owncast's short go-live suppression window.
    serve_status(&stub, false, connected);
    assert!(owncast::refresh_media(&state, item).await.unwrap());
    let ended = media::find_by_ids(&pool, &[item.id])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(ended.live_state.as_deref(), Some("ended"));
    assert!(ended.live_permanent);

    // A later, unrelated session must not turn the historical Note live. The
    // account-level player does represent that new current session.
    serve_status(&stub, true, connected + time::Duration::hours(1));
    owncast::refresh_media(&state, &ended).await.unwrap();
    let historical = media::find_by_ids(&pool, &[item.id])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(historical.live_state.as_deref(), Some("ended"));
    let account_live = owncast::profile_live_media(&state, author.id)
        .await
        .unwrap()
        .expect("new current session is playable from the account");
    assert_eq!(account_live.status_id, None);
    assert_eq!(account_live.live_state.as_deref(), Some("live"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ordinary_service_with_hls_is_not_mistaken_for_owncast(pool: PgPool) {
    let mut service = RemoteUser::new("video.example", "caster");
    service.actor.kind = "Service".into();
    let stub = StubFederation::with_users(&[&service]);
    stub.webfinger_hls.lock().unwrap().insert(
        service.acct.clone(),
        "https://video.example/hls/live.m3u8".into(),
    );
    stub.serve_page_as(
        "https://video.example/.well-known/nodeinfo",
        "https://video.example/.well-known/nodeinfo",
        "application/json",
        r#"{"links":[{"rel":"http://nodeinfo.diaspora.software/ns/schema/2.0","href":"https://video.example/nodeinfo/2.0"}]}"#,
    );
    stub.serve_page_as(
        "https://video.example/nodeinfo/2.0",
        "https://video.example/nodeinfo/2.0",
        "application/json",
        r#"{"software":{"name":"not-owncast"},"protocols":["activitypub"],"metadata":{}}"#,
    );
    let state = test_state_with(pool.clone(), stub);
    let acct: Acct = service.acct.parse().unwrap();
    let account = resolve_remote_accounts(&state, &acct, ActorClass::PersonLike)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        remote_stream_source::find_by_account(&pool, account.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn released_owncast_config_account_fallback_is_supported(pool: PgPool) {
    let mut caster = RemoteUser::new("legacy-owncast.example", "streamer");
    caster.actor.kind = "Service".into();
    let stub = StubFederation::with_users(&[&caster]);
    stub.webfinger_hls.lock().unwrap().insert(
        caster.acct.clone(),
        "https://legacy-owncast.example/hls/stream.m3u8".into(),
    );
    stub.serve_page_as(
        "https://legacy-owncast.example/.well-known/nodeinfo",
        "https://legacy-owncast.example/.well-known/nodeinfo",
        "application/json",
        r#"{"links":[{"rel":"http://nodeinfo.diaspora.software/ns/schema/2.0","href":"https://legacy-owncast.example/nodeinfo/2.0"}]}"#,
    );
    stub.serve_page_as(
        "https://legacy-owncast.example/nodeinfo/2.0",
        "https://legacy-owncast.example/nodeinfo/2.0",
        "application/json",
        r#"{"software":{"name":"Owncast","version":"0.2.5"},"protocols":["activitypub"],"metadata":{}}"#,
    );
    stub.serve_page_as(
        "https://legacy-owncast.example/api/config",
        "https://legacy-owncast.example/api/config",
        "application/json",
        r#"{"federation":{"account":"streamer@legacy-owncast.example"}}"#,
    );

    let state = test_state_with(pool.clone(), stub);
    let acct: Acct = caster.acct.parse().unwrap();
    let account = resolve_remote_accounts(&state, &acct, ActorClass::PersonLike)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let source = remote_stream_source::find_by_account(&pool, account.id)
        .await
        .unwrap()
        .expect("v0.2.5 public federation.account identifies Owncast");
    assert_eq!(
        source.hls_master_url,
        "https://legacy-owncast.example/hls/stream.m3u8"
    );
}
