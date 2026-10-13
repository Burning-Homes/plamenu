mod common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{StubFederation, create_local_account, test_state_with_store};
use http_body_util::BodyExt;
use plamenu::{
    AppState, music_assets, music_collections, music_entities,
    music_saved::{Kind, Reference},
    music_shares::{self, Created, New},
    storage::{MediaStore, MemoryStore},
};
use plamenu_db::{PgPool, account::Account, id};
use std::sync::Arc;
use tower::ServiceExt;
struct Fixture {
    state: AppState,
    owner: Account,
    viewer: Account,
    artist: i64,
    album: i64,
    track: i64,
    collection: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let viewer = create_local_account(&pool, "bob", "Bob").await;
    let media = Arc::new(MemoryStore::default());
    let state = test_state_with_store(pool, StubFederation::with_actors([]), media.clone());
    let artist = id::next();
    let album = id::next();
    let track = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name,description) VALUES($1,$2,'Performer','Artist journal')").bind(artist).bind(owner.id).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,description,visibility) VALUES($1,$2,$3,'Album','Private album journal','private')").bind(album).bind(owner.id).bind(artist).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Private track',$3,$4,'private',$5,$6,32,32,$7,10,'mp3')").bind(track).bind(owner.id).bind(artist).bind(album).bind(format!("music{track}.orig")).bind(format!("music{track}.mp3")).bind(format!("{track:064x}")).execute(&state.pool).await.unwrap();
    for file in [format!("music{track}.orig"), format!("music{track}.mp3")] {
        media
            .put(&file, b"0123456789abcdefghijklmnopqrstuv".to_vec())
            .await
            .unwrap();
    }
    let collection = music_collections::create(
        &state,
        owner.id,
        &music_collections::New {
            name: "Library".into(),
            kind: "library".into(),
            description: String::new(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap()
    .id;
    Fixture {
        state,
        owner,
        viewer,
        artist,
        album,
        track,
        collection,
    }
}
fn reference(kind: Kind, id: i64) -> Reference {
    Reference {
        kind,
        id: id.to_string(),
    }
}
async fn link(f: &Fixture, kind: Kind, id: i64) -> Created {
    music_shares::create(
        &f.state,
        f.owner.id,
        &New {
            reference: reference(kind, id),
            expires_at: None,
        },
    )
    .await
    .unwrap()
}
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    fields: &[(&str, &str)],
) -> Response {
    let mut request = Request::builder().uri(path).method(method);
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    app.clone()
        .oneshot(
            request
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn text(response: Response) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn exact_link_serves_ranges_and_revocation_closes_cached_urls_without_publication(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let l = link(&f, Kind::Track, f.track).await;
    let hash: String = sqlx::query_scalar("SELECT token_hash FROM music_share_links WHERE id=$1")
        .bind(l.id.parse::<i64>().unwrap())
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(hash, plamenu::auth::hash_secret(&l.token));
    assert_ne!(hash, l.token);
    let app = plamenu::build_router(f.state.clone());
    let preview = request(
        &app,
        "GET",
        &format!("/music/share?token={}", l.token),
        &[],
        &[],
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    assert_eq!(preview.headers()[header::REFERRER_POLICY], "no-referrer");
    let html = text(preview).await;
    assert!(html.contains("Private track"));
    assert!(!html.contains("Private album journal"));
    let json_response = request(
        &app,
        "GET",
        &format!("/music/share?token={}", l.token),
        &[("Accept", "application/json")],
        &[],
    )
    .await;
    assert_eq!(json_response.status(), StatusCode::OK);
    assert_eq!(
        json_response.headers()[header::CONTENT_TYPE],
        "application/json"
    );
    let preview: serde_json::Value = serde_json::from_str(&text(json_response).await).unwrap();
    assert_eq!(preview["source"]["title"], "Private track");
    assert!(preview["source"].get("owner_id").is_none());
    assert!(preview["source"].get("config").is_none());
    assert_eq!(
        preview["tracks"]["items"][0]["stream_url"],
        format!("/music/share/assets/{}/stream?token={}", f.track, l.token)
    );
    let stream = format!("/music/share/assets/{}/stream?token={}", f.track, l.token);
    let r = request(&app, "GET", &stream, &[("Range", "bytes=0-15")], &[]).await;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.headers()[header::CACHE_CONTROL], "private, no-store");
    assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes 0-15/32");
    assert_eq!(text(r).await, "0123456789abcdef");
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/music/assets/{}/stream", f.track),
            &[],
            &[]
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!(
                "/music/share/assets/{}/stream?token={}",
                f.track + 1,
                l.token
            ),
            &[],
            &[]
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        music_shares::create(
            &f.state,
            f.viewer.id,
            &New {
                reference: reference(Kind::Track, f.track),
                expires_at: None
            }
        )
        .await
        .is_err()
    );
    assert!(
        music_shares::revoke(&f.state, f.viewer.id, l.id.parse().unwrap())
            .await
            .is_err()
    );
    music_shares::revoke(&f.state, f.owner.id, l.id.parse().unwrap())
        .await
        .unwrap();
    for path in [&stream, &format!("/music/share?token={}", l.token)] {
        let r = request(&app, "GET", path, &[], &[]).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "private, no-store");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_publications")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn parent_links_require_explicit_owned_audio_sharing_and_artist_links_do_not_grant_tracks(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let album = link(&f, Kind::Album, f.album).await;
    let artist = link(&f, Kind::Artist, f.artist).await;
    let a = music_shares::resolve(&f.state, &album.token, None)
        .await
        .unwrap();
    assert_eq!(
        music_shares::source(&f.state, &a).await.unwrap()["description"],
        "Private album journal"
    );
    assert!(
        music_shares::tracks(&f.state, &a, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let ar = music_shares::resolve(&f.state, &artist.token, None)
        .await
        .unwrap();
    assert!(
        music_shares::asset(&f.state, &ar, f.track, None)
            .await
            .is_err()
    );
    let current = music_entities::get(
        &f.state,
        music_entities::Kind::Album,
        f.album,
        Some(f.owner.id),
    )
    .await
    .unwrap();
    plamenu::music_album_access::update(
        &f.state,
        f.owner.id,
        f.album,
        &plamenu::music_album_access::Edit {
            revision: current.revision.to_string(),
            visibility: "private".into(),
            share_tracks: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        music_shares::tracks(&f.state, &a, None, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert!(music_assets::get(&f.state, f.track, None).await.is_err());
    assert!(
        music_shares::asset(&f.state, &ar, f.track, None)
            .await
            .is_err()
    );
    let direct = link(&f, Kind::Track, f.track).await;
    music_shares::revoke(&f.state, f.owner.id, album.id.parse().unwrap())
        .await
        .unwrap();
    assert!(
        music_shares::resolve(&f.state, &album.token, None)
            .await
            .is_err()
    );
    assert!(
        music_shares::resolve(&f.state, &direct.token, None)
            .await
            .is_ok()
    );
    let collection = music_collections::get(&f.state, f.collection, Some(f.owner.id))
        .await
        .unwrap();
    music_collections::add(
        &f.state,
        f.owner.id,
        f.collection,
        &music_collections::Add {
            remote_id: None,
            episode_id: None,
            revision: collection.revision.to_string(),
            asset_id: Some(f.track.to_string()),
            status_id: None,
        },
    )
    .await
    .unwrap();
    let l = link(&f, Kind::Library, f.collection).await;
    let c = music_shares::resolve(&f.state, &l.token, None)
        .await
        .unwrap();
    assert!(
        music_shares::asset(&f.state, &c, f.track, None)
            .await
            .is_err()
    );
    let current = music_collections::get(&f.state, f.collection, Some(f.owner.id))
        .await
        .unwrap();
    music_collections::set_contents_sharing(
        &f.state,
        f.owner.id,
        f.collection,
        current.revision,
        true,
    )
    .await
    .unwrap();
    assert!(
        music_shares::asset(&f.state, &c, f.track, None)
            .await
            .is_ok()
    );
    let current = music_collections::get(&f.state, f.collection, Some(f.owner.id))
        .await
        .unwrap();
    music_collections::set_contents_sharing(
        &f.state,
        f.owner.id,
        f.collection,
        current.revision,
        false,
    )
    .await
    .unwrap();
    assert!(
        music_shares::asset(&f.state, &c, f.track, None)
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn expiration_blocks_suspension_and_source_erasure_revoke_the_same_capability(pool: PgPool) {
    let f = fixture(pool).await;
    let l = link(&f, Kind::Track, f.track).await;
    plamenu_db::block::create(&f.state.pool, f.viewer.id, f.owner.id, None)
        .await
        .unwrap();
    assert!(
        music_shares::resolve(&f.state, &l.token, Some(f.viewer.id))
            .await
            .is_err()
    );
    plamenu_db::account::suspend(&f.state.pool, f.owner.id, "local")
        .await
        .unwrap();
    assert!(
        music_shares::resolve(&f.state, &l.token, None)
            .await
            .is_err()
    );
    sqlx::query("UPDATE accounts SET suspended_at=NULL WHERE id=$1")
        .bind(f.owner.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_shares::resolve(&f.state, &l.token, None)
            .await
            .is_ok()
    );
    sqlx::query("UPDATE music_share_links SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(l.id.parse::<i64>().unwrap())
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_shares::resolve(&f.state, &l.token, None)
            .await
            .is_err()
    );
    assert!(
        music_shares::page(&f.state, f.owner.id, None, false)
            .await
            .unwrap()
            .items[0]
            .expired
    );
    let live = link(&f, Kind::Album, f.album).await;
    plamenu_db::account::mark_deleted(&f.state.pool, f.owner.id)
        .await
        .unwrap();
    assert!(
        music_shares::resolve(&f.state, &live.token, None)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_share_links WHERE owner_id=$1")
        .bind(f.owner.id)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn sharing_overview_pages_ties_and_grants_without_secrets_or_cross_owner_data(pool: PgPool) {
    let f = fixture(pool).await;
    let mut secrets = Vec::new();
    for _ in 0..55 {
        secrets.push(link(&f, Kind::Track, f.track).await);
    }
    music_assets::grant(&f.state, f.owner.id, f.track, f.viewer.id, true)
        .await
        .unwrap();
    plamenu::music_album_access::grant(&f.state, f.owner.id, f.album, f.viewer.id, true)
        .await
        .unwrap();
    sqlx::query("UPDATE music_share_links SET created_at='2026-01-01T00:00:00Z' WHERE owner_id=$1")
        .bind(f.owner.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let first = music_shares::page(&f.state, f.owner.id, None, false)
        .await
        .unwrap();
    assert_eq!(first.items.len(), 50);
    let second = music_shares::page(&f.state, f.owner.id, first.before.as_deref(), false)
        .await
        .unwrap();
    assert_eq!(second.items.len(), 7);
    assert!(second.before.is_none());
    let refs = first
        .items
        .iter()
        .chain(&second.items)
        .map(|v| {
            format!(
                "{}:{}:{}",
                v.category,
                serde_json::to_string(&v.reference.kind).unwrap(),
                v.id
            )
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(refs.len(), 57);
    let json = serde_json::to_string(&first).unwrap();
    for secret in &secrets {
        assert!(!json.contains(&secret.token));
    }
    assert!(
        music_shares::page(&f.state, f.viewer.id, None, true)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        music_shares::page(&f.state, f.owner.id, Some("invalid"), true)
            .await
            .is_err()
    );
    music_shares::revoke_grant(
        &f.state,
        f.owner.id,
        &reference(Kind::Track, f.track),
        f.viewer.id,
    )
    .await
    .unwrap();
    assert!(
        music_assets::get(&f.state, f.track, Some(f.viewer.id))
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn native_create_and_revoke_forms_require_csrf_and_show_raw_link_only_once(pool: PgPool) {
    let f = fixture(pool).await;
    plamenu_db::user::create(
        &f.state.pool,
        f.owner.id,
        Some("alice@plamenu.test"),
        &plamenu::auth::hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let app = plamenu::build_router(f.state.clone());
    let login = request(
        &app,
        "POST",
        "/login",
        &[],
        &[
            ("email", "alice@plamenu.test"),
            ("password", "fixture-password"),
        ],
    )
    .await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .find(|v| v.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let id = f.track.to_string();
    let new = request(
        &app,
        "GET",
        &format!("/music/shares/new?kind=track&id={id}"),
        &[("Cookie", &cookie)],
        &[],
    )
    .await;
    assert_eq!(new.status(), StatusCode::OK);
    assert!(text(new).await.contains("Create share link"));
    let fields = [
        ("csrf", "invalid"),
        ("kind", "track"),
        ("id", id.as_str()),
        ("expires", "week"),
    ];
    assert_eq!(
        request(
            &app,
            "POST",
            "/web/music/share-links",
            &[("Cookie", &cookie)],
            &fields
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let fields = [
        ("csrf", csrf.as_str()),
        ("kind", "track"),
        ("id", id.as_str()),
        ("expires", "week"),
    ];
    let created = request(
        &app,
        "POST",
        "/web/music/share-links",
        &[("Cookie", &cookie)],
        &fields,
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(created.headers()[header::REFERRER_POLICY], "no-referrer");
    let html = text(created).await;
    assert!(html.contains("Copy this link now"));
    let raw = html
        .split("/music/share?token=")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(raw.len(), 43);
    let manager = request(&app, "GET", "/music/shares", &[("Cookie", &cookie)], &[]).await;
    let html = text(manager).await;
    assert!(!html.contains(&raw));
    assert!(html.contains("Private track"));
    let link: i64 = sqlx::query_scalar("SELECT id FROM music_share_links WHERE owner_id=$1")
        .bind(f.owner.id)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    let link = link.to_string();
    assert_eq!(
        request(
            &app,
            "POST",
            "/web/music/shares/revoke",
            &[("Cookie", &cookie)],
            &[("csrf", "invalid"), ("category", "url"), ("link_id", &link)]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/web/music/shares/revoke",
            &[("Cookie", &cookie)],
            &[("csrf", &csrf), ("category", "url"), ("link_id", &link)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        request(&app, "GET", &format!("/music/share?token={raw}"), &[], &[])
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

async fn share_notifications(f: &Fixture) -> Vec<plamenu_db::notification::Notification> {
    sqlx::query_as(
        "SELECT * FROM notifications WHERE account_id=$1 AND kind='music.share' ORDER BY id",
    )
    .bind(f.viewer.id)
    .fetch_all(&f.state.pool)
    .await
    .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn new_grants_notify_once_use_current_metadata_and_revoke_their_event(pool: PgPool) {
    let f = fixture(pool).await;
    music_assets::grant(&f.state, f.owner.id, f.track, f.viewer.id, true)
        .await
        .unwrap();
    music_assets::grant(&f.state, f.owner.id, f.track, f.viewer.id, true)
        .await
        .unwrap();
    let rows = share_notifications(&f).await;
    assert_eq!(rows.len(), 1);
    let first = plamenu::entities::render_notifications(
        &f.state.pool,
        common::TEST_DOMAIN,
        &rows,
        f.viewer.id,
    )
    .await
    .unwrap();
    assert_eq!(first[0]["type"], "music.share");
    assert_eq!(first[0]["music_resource"]["title"], "Private track");
    assert!(first[0].get("status").is_none());
    sqlx::query("UPDATE music_assets SET title='Current shared title' WHERE id=$1")
        .bind(f.track)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let current = plamenu::entities::render_notifications(
        &f.state.pool,
        common::TEST_DOMAIN,
        &rows,
        f.viewer.id,
    )
    .await
    .unwrap();
    assert_eq!(
        current[0]["music_resource"]["title"],
        "Current shared title"
    );
    plamenu_db::block::create(&f.state.pool, f.viewer.id, f.owner.id, None)
        .await
        .unwrap();
    let hidden = plamenu::entities::render_notifications(
        &f.state.pool,
        common::TEST_DOMAIN,
        &rows,
        f.viewer.id,
    )
    .await
    .unwrap();
    assert!(hidden[0]["music_resource"].is_null());
    music_assets::grant(&f.state, f.owner.id, f.track, f.viewer.id, false)
        .await
        .unwrap();
    assert!(share_notifications(&f).await.is_empty());
    music_assets::grant(&f.state, f.owner.id, f.track, f.owner.id, true)
        .await
        .unwrap();
    let own: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id=$1 AND kind='music.share'",
    )
    .bind(f.owner.id)
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(own, 0);
    let statuses: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(statuses, 0);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn notification_failure_rolls_back_the_grant_and_container_revision(pool: PgPool) {
    let f = fixture(pool).await;
    let current = music_collections::get(&f.state, f.collection, Some(f.owner.id))
        .await
        .unwrap();
    sqlx::query("CREATE FUNCTION reject_share_notification() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='music.share' THEN RAISE EXCEPTION 'forced notification failure'; END IF; RETURN NEW; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_share_notification BEFORE INSERT ON notifications FOR EACH ROW EXECUTE FUNCTION reject_share_notification()").execute(&f.state.pool).await.unwrap();
    assert!(
        music_collections::grant(
            &f.state,
            f.owner.id,
            f.collection,
            current.revision,
            f.viewer.id,
            true
        )
        .await
        .is_err()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM music_collection_grants WHERE collection_id=$1")
            .bind(f.collection)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    let after = music_collections::get(&f.state, f.collection, Some(f.owner.id))
        .await
        .unwrap();
    assert_eq!(after.revision, current.revision);
    assert!(share_notifications(&f).await.is_empty());
    sqlx::query("DROP TRIGGER reject_share_notification ON notifications")
        .execute(&f.state.pool)
        .await
        .unwrap();
    music_collections::grant(
        &f.state,
        f.owner.id,
        f.collection,
        current.revision,
        f.viewer.id,
        true,
    )
    .await
    .unwrap();
    assert_eq!(share_notifications(&f).await.len(), 1);
    plamenu_db::account::purge_local_data(&f.state.pool, f.viewer.id)
        .await
        .unwrap();
    assert!(share_notifications(&f).await.is_empty());
    let grants: i64 =
        sqlx::query_scalar("SELECT count(*) FROM music_collection_grants WHERE account_id=$1")
            .bind(f.viewer.id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(grants, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn playlist_capability_keeps_repeats_and_never_delegates_a_foreign_private_grant(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let foreign = id::next();
    let foreign_artist = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Foreign performer')")
        .bind(foreign_artist)
        .bind(f.viewer.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Foreign private',$6,'private',$3,$4,32,32,$5,10,'mp3')")
        .bind(foreign).bind(f.viewer.id).bind(format!("music{foreign}.orig")).bind(format!("music{foreign}.mp3")).bind(format!("{foreign:064x}")).bind(foreign_artist).execute(&f.state.pool).await.unwrap();
    music_assets::grant(&f.state, f.viewer.id, foreign, f.owner.id, true)
        .await
        .unwrap();
    let playlist = music_collections::create(
        &f.state,
        f.owner.id,
        &music_collections::New {
            name: "Playlist".into(),
            kind: "playlist".into(),
            description: String::new(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap()
    .id;
    for asset in [f.track, foreign, f.track] {
        let current = music_collections::get(&f.state, playlist, Some(f.owner.id))
            .await
            .unwrap();
        music_collections::add(
            &f.state,
            f.owner.id,
            playlist,
            &music_collections::Add {
                remote_id: None,
                episode_id: None,
                asset_id: Some(asset.to_string()),
                status_id: None,
                revision: current.revision.to_string(),
            },
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE music_collections SET share_tracks=true WHERE id=$1")
        .bind(playlist)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let link = link(&f, Kind::Playlist, playlist).await;
    let scope = music_shares::resolve(&f.state, &link.token, None)
        .await
        .unwrap();
    let page = music_shares::tracks(&f.state, &scope, None, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0]["id"], f.track.to_string());
    assert_eq!(page.items[1]["id"], f.track.to_string());
    assert_ne!(page.items[0]["entry_id"], page.items[1]["entry_id"]);
    assert!(
        music_shares::asset(&f.state, &scope, foreign, None)
            .await
            .is_err()
    );
    let member = music_shares::tracks(&f.state, &scope, Some(f.viewer.id), None)
        .await
        .unwrap();
    assert_eq!(member.items.len(), 3);
    assert_eq!(member.items[1]["id"], foreign.to_string());
    let current = music_collections::get(&f.state, playlist, Some(f.owner.id))
        .await
        .unwrap();
    let entry = page.items[0]["entry_id"].as_str().unwrap().parse().unwrap();
    music_collections::remove(&f.state, f.owner.id, playlist, current.revision, entry)
        .await
        .unwrap();
    let remaining = music_shares::tracks(&f.state, &scope, None, None)
        .await
        .unwrap();
    assert_eq!(remaining.items.len(), 1);
    assert_eq!(remaining.items[0]["entry_id"], page.items[1]["entry_id"]);
    let current = music_collections::get(&f.state, playlist, Some(f.owner.id))
        .await
        .unwrap();
    let entry = remaining.items[0]["entry_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    music_collections::remove(&f.state, f.owner.id, playlist, current.revision, entry)
        .await
        .unwrap();
    assert!(
        music_shares::asset(&f.state, &scope, f.track, None)
            .await
            .is_err()
    );
    assert!(
        music_shares::tracks(&f.state, &scope, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn album_library_and_radio_grants_have_current_recipient_notifications_without_radio_config(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    plamenu::music_album_access::grant(&f.state, f.owner.id, f.album, f.viewer.id, true)
        .await
        .unwrap();
    let current = music_collections::get(&f.state, f.collection, Some(f.owner.id))
        .await
        .unwrap();
    music_collections::grant(
        &f.state,
        f.owner.id,
        f.collection,
        current.revision,
        f.viewer.id,
        true,
    )
    .await
    .unwrap();
    let radio = plamenu::music_radios::create(
        &f.state,
        f.owner.id,
        &plamenu::music_radios::New {
            name: "Shared radio".into(),
            description: "Radio notes".into(),
            config: "Private generation configuration".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    plamenu::music_radios::grant(&f.state, f.owner.id, radio.id, f.viewer.id, true)
        .await
        .unwrap();
    plamenu::music_radios::grant(&f.state, f.owner.id, radio.id, f.viewer.id, true)
        .await
        .unwrap();
    let rows = share_notifications(&f).await;
    assert_eq!(rows.len(), 3);
    let rendered = plamenu::entities::render_notifications(
        &f.state.pool,
        common::TEST_DOMAIN,
        &rows,
        f.viewer.id,
    )
    .await
    .unwrap();
    let kinds = rendered
        .iter()
        .map(|r| r["music_resource"]["kind"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        kinds,
        std::collections::HashSet::from(["album", "library", "radio"])
    );
    assert!(
        !serde_json::to_string(&rendered)
            .unwrap()
            .contains("Private generation configuration")
    );
    let radio_link = link(&f, Kind::Radio, radio.id).await;
    let scope = music_shares::resolve(&f.state, &radio_link.token, None)
        .await
        .unwrap();
    assert!(
        music_shares::source(&f.state, &scope)
            .await
            .unwrap()
            .get("config")
            .is_none()
    );
    for (kind, target) in [
        (Kind::Album, f.album),
        (Kind::Library, f.collection),
        (Kind::Radio, radio.id),
    ] {
        music_shares::revoke_grant(&f.state, f.owner.id, &reference(kind, target), f.viewer.id)
            .await
            .unwrap();
    }
    assert!(share_notifications(&f).await.is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn share_api_requires_read_and_media_scopes_and_redacts_listed_tokens(pool: PgPool) {
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    let f = fixture(pool).await;
    let user = user::create(
        &f.state.pool,
        f.owner.id,
        Some("alice@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let application = oauth::create_app(
        &f.state.pool,
        oauth::NewApp {
            name: "share-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".into()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let mut tokens = Vec::new();
    for scope in ["read:statuses", "write:media", "write:statuses"] {
        let token = generate_secret();
        oauth::create_token(
            &f.state.pool,
            &hash_secret(&token),
            application.id,
            Some(user.id),
            scope,
        )
        .await
        .unwrap();
        tokens.push(format!("Bearer {token}"));
    }
    let app = plamenu::build_router(f.state.clone());
    for (auth, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(tokens[0].as_str()), StatusCode::OK),
        (Some(tokens[1].as_str()), StatusCode::FORBIDDEN),
    ] {
        let headers = auth.map(|a| vec![("Authorization", a)]).unwrap_or_default();
        assert_eq!(
            request(&app, "GET", "/api/v1/music/shares", &headers, &[])
                .await
                .status(),
            status
        );
    }
    let fields = [("reference[kind]", "track"), ("reference[id]", "")];
    for token in [&tokens[0], &tokens[2]] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/music/share-links",
                &[("Authorization", token)],
                &fields
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/music/share-links")
                .header("Authorization", &tokens[1])
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({"reference":{"kind":"track","id":f.track.to_string()}})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(
        created.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let created: serde_json::Value = serde_json::from_str(&text(created).await).unwrap();
    let path = format!(
        "/api/v1/music/share-links/{}",
        created["id"].as_str().unwrap()
    );
    assert_eq!(
        request(&app, "DELETE", &path, &[("Authorization", &tokens[0])], &[])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let listed = text(
        request(
            &app,
            "GET",
            "/api/v1/music/shares",
            &[("Authorization", &tokens[0])],
            &[],
        )
        .await,
    )
    .await;
    assert!(!listed.contains(created["token"].as_str().unwrap()));
    assert_eq!(
        request(&app, "DELETE", &path, &[("Authorization", &tokens[1])], &[])
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/music/share?token={}", created["token"].as_str().unwrap()),
            &[("Accept", "application/json")],
            &[]
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn capability_artwork_tracks_current_membership_and_revocation(pool: PgPool) {
    let f = fixture(pool).await;
    let art = id::next();
    let unrelated = id::next();
    for (image, album) in [(art, Some(f.album)), (unrelated, None)] {
        let other_album = if let Some(album) = album {
            album
        } else {
            let album = id::next();
            sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name) VALUES($1,$2,$3,'Unrelated album')").bind(album).bind(f.owner.id).bind(f.artist).execute(&f.state.pool).await.unwrap();
            album
        };
        let file = format!("musicart{image}.png");
        sqlx::query("INSERT INTO music_artworks(id,owner_id,album_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,'cover',$4,'image/png',8,1,1,$5)").bind(image).bind(f.owner.id).bind(other_album).bind(&file).bind(id::next()).execute(&f.state.pool).await.unwrap();
        f.state
            .media
            .put(&file, b"artbytes".to_vec())
            .await
            .unwrap();
    }
    let collection = music_collections::get(&f.state, f.collection, Some(f.owner.id))
        .await
        .unwrap();
    music_collections::add(
        &f.state,
        f.owner.id,
        f.collection,
        &music_collections::Add {
            remote_id: None,
            episode_id: None,
            revision: collection.revision.to_string(),
            asset_id: Some(f.track.to_string()),
            status_id: None,
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE music_collections SET share_tracks=true WHERE id=$1")
        .bind(f.collection)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let link = link(&f, Kind::Library, f.collection).await;
    let app = plamenu::build_router(f.state.clone());
    let path = format!("/music/share/artwork/{art}?token={}", link.token);
    let response = request(&app, "GET", &path, &[], &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(text(response).await, "artbytes");
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/music/share/artwork/{unrelated}?token={}", link.token),
            &[],
            &[]
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE music_collections SET share_tracks=false WHERE id=$1")
        .bind(f.collection)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", &path, &[], &[]).await.status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE music_collections SET share_tracks=true WHERE id=$1")
        .bind(f.collection)
        .execute(&f.state.pool)
        .await
        .unwrap();
    music_shares::revoke(&f.state, f.owner.id, link.id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", &path, &[], &[]).await.status(),
        StatusCode::NOT_FOUND
    );
}
