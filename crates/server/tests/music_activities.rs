mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    actions::{self, PostParams},
    auth::{generate_secret, hash_password, hash_secret},
    music_activity::{self, Query, Target},
};
use plamenu_db::{
    PgPool,
    account::Account,
    favourite, id, oauth,
    status::{self, NewLocalStatus},
    user,
};
use serde_json::{Value, json};
use tower::ServiceExt;
struct Fixture {
    state: AppState,
    owner: Account,
    other: Account,
    artist: i64,
    asset: i64,
    album: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let other = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let album = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Activity artist')")
        .bind(artist)
        .bind(owner.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO music_albums(id,owner_id,artist_id,name) VALUES($1,$2,$3,'Activity album')",
    )
    .bind(album)
    .bind(owner.id)
    .bind(artist)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,album_id,title,visibility,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration) VALUES($1,$2,$3,$4,'Activity track','public',$5,$6,'mp3',1,1,$7,2)").bind(asset).bind(owner.id).bind(artist).bind(album).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    state
        .media
        .put(&format!("music{asset}.orig"), vec![0])
        .await
        .unwrap();
    state
        .media
        .put(&format!("music{asset}.mp3"), vec![1])
        .await
        .unwrap();
    Fixture {
        state,
        owner,
        other,
        artist,
        asset,
        album,
    }
}
async fn target(f: &Fixture, kind: &str, id: i64) -> Target {
    music_activity::resolve(&f.state, kind, id, Some(f.owner.id))
        .await
        .unwrap()
}
async fn page(f: &Fixture, t: &Target, kind: Option<&str>) -> Vec<Value> {
    music_activity::page(
        &f.state,
        t,
        None,
        &Query {
            kind: kind.map(str::to_owned),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .items
}
async fn website(f: &Fixture, t: &Target) -> i64 {
    let source = format!("https://website.example/{}", id::next());
    let entry=plamenu::webmentions::parse::entry(&format!("<article class=h-entry><p class=p-summary>Resource review</p><a class=u-in-reply-to href='{}'>Resource</a></article>",t.uri),&url::Url::parse(&source).unwrap(),&t.uri).unwrap().unwrap();
    let mid = id::next();
    sqlx::query("INSERT INTO webmentions(id,source,target,owner_id,entry,active,state) VALUES($1,$2,$3,$4,$5,true,'done')").bind(mid).bind(source).bind(&t.uri).bind(t.owner).bind(sqlx::types::Json(entry)).execute(&f.state.pool).await.unwrap();
    mid
}
#[sqlx::test(migrations = "../db/migrations")]
async fn exact_resource_scope_keeps_track_publication_out_of_artist_and_album_feeds(pool: PgPool) {
    let f = fixture(pool).await;
    let post = plamenu::music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    let track = target(&f, "assets", f.asset).await;
    assert_eq!(
        page(&f, &track, Some("create")).await[0]["status"]["id"],
        post.id.to_string()
    );
    for (kind, oid) in [("artists", f.artist), ("albums", f.album)] {
        let t = target(&f, kind, oid).await;
        assert!(page(&f, &t, None).await.is_empty());
        website(&f, &t).await;
        assert_eq!(page(&f, &t, None).await.len(), 1);
    }
    assert_eq!(page(&f, &track, None).await.len(), 1);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn website_responses_retain_exact_context_with_reply_like_repost_quote_and_current_edits(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let t = target(&f, "artists", f.artist).await;
    let mid = website(&f, &t).await;
    for (kind, filter) in [
        ("reply", "reply"),
        ("like", "like"),
        ("repost", "announce"),
        ("quote", "quote"),
    ] {
        let post = plamenu::webmentions::interactions::publish(
            &f.state,
            &f.other,
            mid,
            kind,
            "Native response",
        )
        .await
        .unwrap();
        let stored: String =
            sqlx::query_scalar("SELECT target FROM webmention_interactions WHERE status_id=$1")
                .bind(post.id)
                .fetch_one(&f.state.pool)
                .await
                .unwrap();
        assert_eq!(stored, t.uri);
        let rows = page(&f, &t, Some(filter)).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"]["id"], post.id.to_string());
        sqlx::query("UPDATE statuses SET content='<p>Edited response</p>' WHERE id=$1")
            .bind(post.id)
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert_eq!(
            page(&f, &t, Some(filter)).await[0]["status"]["content"],
            "<p>Edited response</p>"
        );
        sqlx::query("UPDATE statuses SET deleted_at=now() WHERE id=$1")
            .bind(post.id)
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(page(&f, &t, Some(filter)).await.is_empty());
    }
    assert_eq!(page(&f, &t, Some("webmention")).await.len(), 1);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_raw_entity_reply_and_native_thread_reactions_have_current_audience_and_undo(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let t = target(&f, "artists", f.artist).await;
    let remote = RemoteUser::new("songhive.test", "author");
    let author = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    let doc = json!({"id":"https://songhive.test/activities/reply","type":"Note","attributedTo":remote.actor.id,"to":[plamenu_ap::activity::PUBLIC],"content":"Entity reply","inReplyTo":t.uri});
    let post = plamenu::ingest::ingest_remote_note(&f.state, &author, &doc)
        .await
        .unwrap();
    assert_eq!(post.in_reply_to_id, None);
    let raw: Option<String> =
        sqlx::query_scalar("SELECT in_reply_to_uri FROM statuses WHERE id=$1")
            .bind(post.id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(raw.as_deref(), Some(t.uri.as_str()));
    let local = actions::post_status(
        &f.state,
        PostParams {
            username: &f.other.username,
            text: "Follow-up",
            visibility: "public",
            in_reply_to_id: Some(post.id),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .0;
    let like = favourite::create(&f.state.pool, f.owner.id, local.id, None)
        .await
        .unwrap();
    let boost = status::create_local_reblog(&f.state.pool, f.owner.id, local.id)
        .await
        .unwrap();
    let quote = actions::post_status(
        &f.state,
        PostParams {
            username: &f.owner.username,
            text: "Quote follow-up",
            visibility: "public",
            quoted_status_id: Some(local.id),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        page(&f, &t, Some("quote")).await[0]["status"]["id"],
        quote.id.to_string()
    );
    sqlx::query("UPDATE quotes SET state='revoked' WHERE status_id=$1")
        .bind(quote.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(page(&f, &t, Some("quote")).await.is_empty());
    let rows = page(&f, &t, None).await;
    assert_eq!(rows.len(), 4);
    assert!(
        rows.iter().any(
            |r| r["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(like.id)
                && r["kind"] == "like"
        )
    );
    assert!(
        rows.iter().any(
            |r| r["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(boost.id)
                && r["kind"] == "announce"
        )
    );
    let remote_rows = music_activity::page(
        &f.state,
        &t,
        None,
        &Query {
            source: Some("remote".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(remote_rows.items.len(), 1);
    favourite::delete(&f.state.pool, f.owner.id, local.id)
        .await
        .unwrap();
    status::delete_local(&f.state.pool, boost.id, f.owner.id)
        .await
        .unwrap();
    assert_eq!(page(&f, &t, None).await.len(), 2);
    sqlx::query("UPDATE statuses SET visibility='direct' WHERE id=$1")
        .bind(local.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(page(&f, &t, None).await.len(), 1);
    sqlx::query("UPDATE statuses SET deleted_at=now() WHERE id=$1")
        .bind(post.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(page(&f, &t, None).await.is_empty());
}
#[sqlx::test(migrations = "../db/migrations")]
async fn bounded_paging_skips_hidden_prefix_without_replaying_or_exposing_rows(pool: PgPool) {
    let f = fixture(pool).await;
    let t = target(&f, "artists", f.artist).await;
    for _ in 0..61 {
        let s = status::create_local(
            &f.state.pool,
            NewLocalStatus::new(f.other.id, "Visible", "public", None),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE statuses SET in_reply_to_uri=$2 WHERE id=$1")
            .bind(s.id)
            .bind(&t.uri)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
    for _ in 0..60 {
        let s = status::create_local(
            &f.state.pool,
            NewLocalStatus::new(f.other.id, "Hidden", "direct", None),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE statuses SET in_reply_to_uri=$2 WHERE id=$1")
            .bind(s.id)
            .bind(&t.uri)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
    let first = music_activity::page(&f.state, &t, None, &Query::default())
        .await
        .unwrap();
    assert_eq!(first.items.len(), 50);
    let next = music_activity::page(
        &f.state,
        &t,
        None,
        &Query {
            before: first.before,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(next.items.len(), 11);
    assert!(next.before.is_none());
    assert!(
        !first
            .items
            .iter()
            .any(|a| next.items.iter().any(|b| a["id"] == b["id"]))
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn native_and_api_feeds_validate_scope_filters_and_containing_resource_privacy(pool: PgPool) {
    let f = fixture(pool).await;
    common::open_previews(&f.state.pool).await;
    let t = target(&f, "artists", f.artist).await;
    website(&f, &t).await;
    let uid = user::create(
        &f.state.pool,
        f.owner.id,
        Some("alice@example.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap()
    .id;
    let app = oauth::create_app(
        &f.state.pool,
        oauth::NewApp {
            name: "Activities",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: "read",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        &f.state.pool,
        &hash_secret(&token),
        app.id,
        Some(uid),
        "read:statuses",
    )
    .await
    .unwrap();
    let narrow = generate_secret();
    oauth::create_token(
        &f.state.pool,
        &hash_secret(&narrow),
        app.id,
        Some(uid),
        "read:notifications",
    )
    .await
    .unwrap();
    let router = plamenu::build_router(f.state.clone());
    let path = format!("/api/v1/music/artists/{}/activities", f.artist);
    for (url, auth, expected) in [
        (path.clone(), true, StatusCode::OK),
        (
            format!("{path}?kind=invalid"),
            true,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (path.clone(), false, StatusCode::UNAUTHORIZED),
    ] {
        let mut req = Request::get(url);
        if auth {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let r = router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), expected);
    }
    let denied = router
        .clone()
        .oneshot(
            Request::get(&path)
                .header("authorization", format!("Bearer {narrow}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let r = router
        .clone()
        .oneshot(
            Request::get(format!("/music/artists/{}/activities", f.artist))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let html =
        String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        html.contains("activity-source")
            && html.contains("activity-kind")
            && html.contains("Resource review")
    );
    assert!(!html.contains("website-reply-"));
    sqlx::query("UPDATE music_assets SET visibility='private' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_activity::resolve(&f.state, "assets", f.asset, Some(f.other.id))
            .await
            .is_err()
    );
    let radio = plamenu::music_radios::create(
        &f.state,
        f.owner.id,
        &plamenu::music_radios::New {
            name: "Private radio".into(),
            description: String::new(),
            config: String::new(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        music_activity::resolve(&f.state, "radios", radio.id, Some(f.other.id))
            .await
            .is_err()
    );
    assert!(
        music_activity::resolve(&f.state, "radios", radio.id, Some(f.owner.id))
            .await
            .is_ok()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_activity_targets_require_current_origin_proof_and_collections_use_exact_identity(
    pool: PgPool,
) {
    let mut f = fixture(pool).await;
    let remote = RemoteUser::new("songhive.test", "publisher");
    let stub = StubFederation::with_actors([remote.actor.clone()]);
    let uri = "https://songhive.test/artists/activity";
    stub.objects.lock().unwrap().insert(uri.into(),json!({"id":uri,"type":"Artist","name":"Origin artist","attributedTo":remote.actor.id,"to":[plamenu_ap::activity::PUBLIC]}));
    f.state.federation = stub;
    let resource = plamenu::music_remote::import(&f.state, f.owner.id, uri)
        .await
        .unwrap();
    assert!(
        music_activity::resolve(&f.state, "remote", resource.id, None)
            .await
            .is_err()
    );
    let t = music_activity::resolve(&f.state, "remote", resource.id, Some(f.owner.id))
        .await
        .unwrap();
    assert!(t.aliases.iter().any(|s| s == uri));
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=now() WHERE id=$1")
        .bind(resource.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_activity::resolve(&f.state, "remote", resource.id, Some(f.owner.id))
            .await
            .is_err()
    );
    for kind in ["library", "playlist"] {
        let row = plamenu::music_collections::create(
            &f.state,
            f.owner.id,
            &plamenu::music_collections::New {
                name: kind.into(),
                description: String::new(),
                kind: kind.into(),
                visibility: "public".into(),
            },
        )
        .await
        .unwrap();
        let t = target(&f, "collections", row.id).await;
        let mid = website(&f, &t).await;
        let note = plamenu::webmentions::interactions::publish(
            &f.state,
            &f.other,
            mid,
            "reply",
            "Collection discussion",
        )
        .await
        .unwrap();
        assert_eq!(
            page(&f, &t, Some("reply")).await[0]["status"]["id"],
            note.id.to_string()
        );
        sqlx::query("UPDATE music_collections SET visibility='private' WHERE id=$1")
            .bind(row.id)
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(
            music_activity::resolve(&f.state, "collections", row.id, Some(f.other.id))
                .await
                .is_err()
        );
    }
}
