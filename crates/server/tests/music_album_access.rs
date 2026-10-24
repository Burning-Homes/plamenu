mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use plamenu::{
    AppState,
    auth::{generate_secret, hash_secret},
    music_assets,
    music_entities::{self, Kind},
    music_publications,
    music_saved::{self, Kind as SavedKind, Reference},
};
use plamenu_db::{PgPool, account::Account, id, oauth, user};
use serde_json::json;
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    owner: Account,
    artist: i64,
    album: i64,
    token: String,
    app_id: i64,
    user_id: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let actor_user = user::create(
        &pool,
        owner.id,
        Some("alice@plamenu.test"),
        &common::fixture_password_hash(),
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        &pool,
        oauth::NewApp {
            name: "album-access-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".into()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        &pool,
        &hash_secret(&token),
        app.id,
        Some(actor_user.id),
        "read write",
    )
    .await
    .unwrap();
    let artist = id::next();
    let album = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Album performer')")
        .bind(artist)
        .bind(owner.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,description,release_year,tags) VALUES($1,$2,$3,'Album credits','Private album journal',2026,ARRAY['private-journal'])").bind(album).bind(owner.id).bind(artist).execute(&pool).await.unwrap();
    Fixture {
        state: test_state_with(pool, StubFederation::with_actors([])),
        owner,
        artist,
        album,
        token,
        app_id: app.id,
        user_id: actor_user.id,
    }
}
async fn scoped_token(f: &Fixture, user_id: i64, scope: &str) -> String {
    let token = generate_secret();
    oauth::create_token(
        &f.state.pool,
        &hash_secret(&token),
        f.app_id,
        Some(user_id),
        scope,
    )
    .await
    .unwrap();
    token
}
async fn request(
    f: &Fixture,
    auth: Option<(&str, &str)>,
    method: &str,
    path: &str,
    body: Body,
) -> axum::response::Response {
    let mut request = Request::builder().method(method).uri(path);
    if let Some((header, value)) = auth {
        request = request.header(header, value);
    }
    plamenu::build_router(f.state.clone())
        .oneshot(
            request
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn text(response: axum::response::Response) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn new_owned_albums_keep_private_details_when_a_track_becomes_public(pool: PgPool) {
    let mut f = fixture(pool).await;
    let created = music_entities::create(
        &f.state,
        f.owner.id,
        Kind::Album,
        &music_entities::New {
            name: "Created private album".into(),
            description: "Private creation journal".into(),
            musicbrainz_id: None,
            tags: vec!["private-creation-tag".into()],
            genres: vec![],
            release_year: Some(2026),
            artist_id: Some(f.artist.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(created.visibility, "private");
    assert!(!created.share_tracks);
    f.album = created.id;
    let track = asset(&f, "public").await;
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, None)
            .await
            .is_err()
    );
    let published = music_publications::publish(&f.state, &f.owner, track)
        .await
        .unwrap();
    let doc = plamenu::note::note_for_status(&f.state, &published, &f.owner)
        .await
        .unwrap();
    assert_eq!(doc["track"]["album"]["name"], "Created private album");
    assert!(!doc.to_string().contains("Private creation journal"));
    assert!(!doc.to_string().contains("private-creation-tag"));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn album_member_api_keeps_scopes_roster_and_removal_owner_only(pool: PgPool) {
    let f = fixture(pool).await;
    let bob = create_local_account(&f.state.pool, "bob", "Bob").await;
    let other = user::create(
        &f.state.pool,
        bob.id,
        Some("bob@plamenu.test"),
        &common::fixture_password_hash(),
    )
    .await
    .unwrap();
    let read = format!(
        "Bearer {}",
        scoped_token(&f, f.user_id, "read:statuses").await
    );
    let media = format!(
        "Bearer {}",
        scoped_token(&f, f.user_id, "write:media").await
    );
    let owner = format!("Bearer {}", f.token);
    let foreign = format!("Bearer {}", scoped_token(&f, other.id, "read write").await);
    let path = format!("/api/v1/music/albums/{}/grants/{}", f.album, bob.id);
    for (token, status) in [
        (&read, StatusCode::FORBIDDEN),
        (&media, StatusCode::FORBIDDEN),
        (&foreign, StatusCode::NOT_FOUND),
        (&owner, StatusCode::OK),
    ] {
        assert_eq!(
            request(
                &f,
                Some(("Authorization", token)),
                "PUT",
                &path,
                Body::empty()
            )
            .await
            .status(),
            status
        );
    }
    let roster = format!("/api/v1/music/albums/{}/grants", f.album);
    let response = request(
        &f,
        Some(("Authorization", &read)),
        "GET",
        &roster,
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    assert_eq!(response.headers()["Vary"], "Authorization, Cookie");
    let page: serde_json::Value = serde_json::from_str(&text(response).await).unwrap();
    assert_eq!(page["items"][0]["id"], bob.id.to_string());
    assert_eq!(
        request(
            &f,
            Some(("Authorization", &foreign)),
            "GET",
            &roster,
            Body::empty()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &f,
            Some(("Authorization", &foreign)),
            "DELETE",
            &path,
            Body::empty()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        plamenu::music_album_access::members(&f.state, f.owner.id, f.album, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        request(
            &f,
            Some(("Authorization", &owner)),
            "DELETE",
            &path,
            Body::empty()
        )
        .await
        .status(),
        StatusCode::OK
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_album_access_forms_require_csrf_and_keep_the_member_roster_private(pool: PgPool) {
    let f = fixture(pool).await;
    create_local_account(&f.state.pool, "bob", "Bob").await;
    let login = request(
        &f,
        None,
        "POST",
        "/login",
        Body::from("email=alice%40plamenu.test&password=fixture-password"),
    )
    .await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all("Set-Cookie")
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find(|value| value.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let access = format!("/web/music/albums/{}/access", f.album);
    let fields = |token: &str| {
        serde_urlencoded::to_string([
            ("csrf", token),
            ("revision", "1"),
            ("visibility", "private"),
        ])
        .unwrap()
    };
    assert_eq!(
        request(
            &f,
            Some(("Cookie", &cookie)),
            "POST",
            &access,
            Body::from(fields("invalid"))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
            .await
            .unwrap()
            .visibility,
        "source"
    );
    assert_eq!(
        request(
            &f,
            Some(("Cookie", &cookie)),
            "POST",
            &access,
            Body::from(fields(&csrf))
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let grant = format!("/web/music/albums/{}/grant", f.album);
    let fields = |token: &str, operation: &str| {
        serde_urlencoded::to_string([("csrf", token), ("member", "bob"), ("operation", operation)])
            .unwrap()
    };
    assert_eq!(
        request(
            &f,
            Some(("Cookie", &cookie)),
            "POST",
            &grant,
            Body::from(fields("invalid", "grant"))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &f,
            Some(("Cookie", &cookie)),
            "POST",
            &grant,
            Body::from(fields(&csrf, "grant"))
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let page = request(
        &f,
        Some(("Cookie", &cookie)),
        "GET",
        &format!("/music/albums/{}/access", f.album),
        Body::empty(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["Cache-Control"], "private, no-store");
    assert_eq!(page.headers()["Vary"], "Cookie");
    assert!(text(page).await.contains("Revoke access"));
    assert_eq!(
        request(
            &f,
            Some(("Cookie", &cookie)),
            "POST",
            &grant,
            Body::from(fields("invalid", "revoke"))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        plamenu::music_album_access::members(&f.state, f.owner.id, f.album, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        request(
            &f,
            Some(("Cookie", &cookie)),
            "POST",
            &grant,
            Body::from(fields(&csrf, "revoke"))
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(
        plamenu::music_album_access::members(&f.state, f.owner.id, f.album, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
async fn access(f: &Fixture, visibility: &str, share_tracks: bool) -> axum::response::Response {
    plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/music/albums/{}/access", f.album))
                .header("Authorization", format!("Bearer {}", f.token))
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"revision":"1","visibility":visibility,"share_tracks":share_tracks})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn asset(f: &Fixture, visibility: &str) -> i64 {
    let asset = id::next();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Album track',$3,$4,$5,$6,$7,100,100,$8,10,'mp3')")
        .bind(asset).bind(f.owner.id).bind(f.artist).bind(f.album).bind(visibility)
        .bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3"))
        .bind(format!("{asset:064x}")).execute(&f.state.pool).await.unwrap();
    asset
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn album_member_roster_pages_without_duplicates_and_rejects_invalid_cursors(pool: PgPool) {
    let f = fixture(pool).await;
    for index in 0..60 {
        let member =
            create_local_account(&f.state.pool, &format!("member_{index}"), "Member").await;
        sqlx::query("INSERT INTO music_album_grants(album_id,account_id) VALUES($1,$2)")
            .bind(f.album)
            .bind(member.id)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
    let first = plamenu::music_album_access::members(&f.state, f.owner.id, f.album, None)
        .await
        .unwrap();
    assert_eq!(first.items.len(), 50);
    let next = first.before.unwrap().parse().unwrap();
    let second = plamenu::music_album_access::members(&f.state, f.owner.id, f.album, Some(next))
        .await
        .unwrap();
    assert_eq!(second.items.len(), 10);
    assert!(second.before.is_none());
    let unique: std::collections::HashSet<_> = first
        .items
        .into_iter()
        .chain(second.items)
        .map(|member| member.id)
        .collect();
    assert_eq!(unique.len(), 60);
    assert!(
        plamenu::music_album_access::members(&f.state, f.owner.id, f.album, Some(-1))
            .await
            .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn grant_waiting_on_recipient_retirement_cannot_recreate_erased_membership(pool: PgPool) {
    let f = fixture(pool).await;
    let track = asset(&f, "private").await;
    let collection = plamenu::music_collections::create(
        &f.state,
        f.owner.id,
        &plamenu::music_collections::New {
            name: "Race library".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    let radio = plamenu::music_radios::create(
        &f.state,
        f.owner.id,
        &plamenu::music_radios::New {
            name: "Race radio".into(),
            description: String::new(),
            config: String::new(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    for kind in ["album", "asset", "collection", "radio"] {
        let bob = create_local_account(&f.state.pool, &format!("bob_{kind}"), "Bob").await;
        let mut retirement = f.state.pool.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *retirement)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR UPDATE")
            .bind(bob.id)
            .execute(&mut *retirement)
            .await
            .unwrap();
        let state = f.state.clone();
        let owner = f.owner.id;
        let album = f.album;
        let member = bob.id;
        let collection_id = collection.id;
        let collection_revision = collection.revision;
        let radio_id = radio.id;
        let grant = tokio::spawn(async move {
            match kind {
                "album" => {
                    plamenu::music_album_access::grant(&state, owner, album, member, true).await
                }
                "asset" => music_assets::grant(&state, owner, track, member, true).await,
                "collection" => {
                    plamenu::music_collections::grant(
                        &state,
                        owner,
                        collection_id,
                        collection_revision,
                        member,
                        true,
                    )
                    .await
                }
                "radio" => {
                    plamenu::music_radios::grant(&state, owner, radio_id, member, true).await
                }
                _ => unreachable!(),
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10),async {
            loop {
                let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE a.datname=current_database() AND $1=ANY(pg_blocking_pids(a.pid)))").bind(blocker).fetch_one(&f.state.pool).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
            .bind(bob.id)
            .execute(&mut *retirement)
            .await
            .unwrap();
        retirement.commit().await.unwrap();
        assert!(
            grant.await.unwrap().is_err(),
            "{kind} grant recreated an erased membership"
        );
        let count:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM music_album_grants WHERE account_id=$1)+(SELECT count(*) FROM music_asset_grants WHERE account_id=$1)+(SELECT count(*) FROM music_collection_grants WHERE account_id=$1)+(SELECT count(*) FROM music_radio_grants WHERE account_id=$1)").bind(member).fetch_one(&f.state.pool).await.unwrap();
        assert_eq!(count, 0, "{kind}");
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn direct_tombstone_waits_for_inflight_recipient_grant_before_cleanup(pool: PgPool) {
    let f = fixture(pool).await;
    let bob = create_local_account(&f.state.pool, "bob", "Bob").await;
    let track = asset(&f, "private").await;
    let collection = id::next();
    let radio = id::next();
    sqlx::query("INSERT INTO music_collections(id,owner_id,name,kind) VALUES($1,$2,'Race library','library')")
        .bind(collection).bind(f.owner.id).execute(&f.state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_radios(id,owner_id,name) VALUES($1,$2,'Race radio')")
        .bind(radio)
        .bind(f.owner.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let mut admitted = f.state.pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *admitted)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR KEY SHARE")
        .bind(bob.id)
        .execute(&mut *admitted)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_album_grants(album_id,account_id) VALUES($1,$2)")
        .bind(f.album)
        .bind(bob.id)
        .execute(&mut *admitted)
        .await
        .unwrap();
    for (query, target) in [
        (
            "INSERT INTO music_asset_grants(asset_id,account_id) VALUES($1,$2)",
            track,
        ),
        (
            "INSERT INTO music_collection_grants(collection_id,account_id) VALUES($1,$2)",
            collection,
        ),
        (
            "INSERT INTO music_radio_grants(radio_id,account_id) VALUES($1,$2)",
            radio,
        ),
    ] {
        sqlx::query(query)
            .bind(target)
            .bind(bob.id)
            .execute(&mut *admitted)
            .await
            .unwrap();
    }
    let state = f.state.clone();
    let member = bob.id;
    let retirement = tokio::spawn(async move {
        plamenu_db::account::mark_deleted(&state.pool, member)
            .await
            .unwrap();
    });
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE a.datname=current_database() AND $1=ANY(pg_blocking_pids(a.pid)))").bind(blocker).fetch_one(&f.state.pool).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    admitted.commit().await.unwrap();
    retirement.await.unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT (SELECT count(*) FROM music_album_grants WHERE account_id=$1)+(SELECT count(*) FROM music_asset_grants WHERE account_id=$1)+(SELECT count(*) FROM music_collection_grants WHERE account_id=$1)+(SELECT count(*) FROM music_radio_grants WHERE account_id=$1)")
            .bind(member)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn reciprocal_track_grants_do_not_deadlock_on_owner_fences(pool: PgPool) {
    let f = fixture(pool).await;
    let bob = create_local_account(&f.state.pool, "bob", "Bob").await;
    let alice_track = asset(&f, "private").await;
    let bob_track = asset(&f, "private").await;
    let bob_artist = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Bob performer')")
        .bind(bob_artist)
        .bind(bob.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE music_assets SET owner_id=$1,artist_id=$3,album_id=NULL WHERE id=$2")
        .bind(bob.id)
        .bind(bob_track)
        .bind(bob_artist)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let mut blockers = f.state.pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blockers)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM music_assets WHERE id=ANY($1) FOR UPDATE")
        .bind(vec![alice_track, bob_track])
        .execute(&mut *blockers)
        .await
        .unwrap();
    let alice_state = f.state.clone();
    let bob_state = f.state.clone();
    let alice_id = f.owner.id;
    let bob_id = bob.id;
    let alice_grant = tokio::spawn(async move {
        music_assets::grant(&alice_state, alice_id, alice_track, bob_id, true).await
    });
    let bob_grant = tokio::spawn(async move {
        music_assets::grant(&bob_state, bob_id, bob_track, alice_id, true).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity a WHERE a.datname=current_database() AND $1=ANY(pg_blocking_pids(a.pid))").bind(blocker).fetch_one(&f.state.pool).await.unwrap();
            if waiting==2 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    blockers.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        alice_grant.await.unwrap().unwrap();
        bob_grant.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    assert!(
        music_assets::get(&f.state, alice_track, Some(bob.id))
            .await
            .is_ok()
    );
    assert!(
        music_assets::get(&f.state, bob_track, Some(f.owner.id))
            .await
            .is_ok()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn blocks_suspension_and_raw_recipient_purge_revoke_album_derived_access(pool: PgPool) {
    let f = fixture(pool).await;
    let bob = create_local_account(&f.state.pool, "bob", "Bob").await;
    let track = asset(&f, "private").await;
    assert_eq!(access(&f, "private", true).await.status(), StatusCode::OK);
    plamenu::music_album_access::grant(&f.state, f.owner.id, f.album, bob.id, true)
        .await
        .unwrap();
    assert!(
        music_assets::get(&f.state, track, Some(bob.id))
            .await
            .is_ok()
    );
    plamenu_db::block::create(&f.state.pool, bob.id, f.owner.id, None)
        .await
        .unwrap();
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, Some(bob.id))
            .await
            .is_err()
    );
    assert!(
        music_assets::get(&f.state, track, Some(bob.id))
            .await
            .is_err()
    );
    plamenu_db::block::delete(&f.state.pool, bob.id, f.owner.id)
        .await
        .unwrap();
    plamenu_db::account::suspend(&f.state.pool, f.owner.id, "local")
        .await
        .unwrap();
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, Some(bob.id))
            .await
            .is_err()
    );
    assert!(
        music_assets::get(&f.state, track, Some(bob.id))
            .await
            .is_err()
    );
    plamenu_db::account::unsuspend(&f.state.pool, f.owner.id)
        .await
        .unwrap();
    assert!(
        music_assets::get(&f.state, track, Some(bob.id))
            .await
            .is_ok()
    );
    let mut tx = f.state.pool.begin().await.unwrap();
    plamenu_db::account::purge_local_data_conn(&mut tx, bob.id)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        !plamenu_db::account::is_deleted(&f.state.pool, bob.id)
            .await
            .unwrap()
    );
    assert!(
        plamenu::music_album_access::members(&f.state, f.owner.id, f.album, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        music_assets::get(&f.state, track, Some(bob.id))
            .await
            .is_err()
    );
    assert!(
        music_assets::get(&f.state, track, Some(f.owner.id))
            .await
            .is_ok()
    );
}
async fn snapshot(
    f: &Fixture,
    track: i64,
    status: i64,
) -> (serde_json::Value, serde_json::Value, serde_json::Value, i64) {
    let album = music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
        .await
        .unwrap()
        .entity(Kind::Album);
    let asset = music_assets::get(&f.state, track, Some(f.owner.id))
        .await
        .unwrap()
        .entity();
    let status=sqlx::query_scalar("SELECT to_jsonb(s) FROM statuses s WHERE id=$1 -- STUBKEEP: regression snapshot includes every stored status field").bind(status).fetch_one(&f.state.pool).await.unwrap();
    let jobs = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    (album, asset, status, jobs)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn album_privacy_and_audio_updates_roll_back_together_and_stale_revisions_are_refused(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let track = asset(&f, "public").await;
    assert_eq!(access(&f, "public", false).await.status(), StatusCode::OK);
    let published = music_publications::publish(&f.state, &f.owner, track)
        .await
        .unwrap();
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let subscriber = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    let library = plamenu::music_follows::library_uri(TEST_DOMAIN, &f.owner);
    plamenu::music_follows::incoming(&f.state,&subscriber,&json!({"id":"https://songhive.test/follow/album-rollback","type":"Follow","actor":remote.actor.id,"object":library})).await.unwrap();
    let original = snapshot(&f, track, published.id).await;
    let input = plamenu::music_album_access::Edit {
        revision: original.0["revision"].as_str().unwrap().to_owned(),
        visibility: "private".into(),
        share_tracks: false,
    };
    sqlx::query("CREATE FUNCTION reject_album_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced album delivery failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_album_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_album_delivery()").execute(&f.state.pool).await.unwrap();
    assert!(
        plamenu::music_album_access::update(&f.state, f.owner.id, f.album, &input)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f, track, published.id).await, original);
    sqlx::query("DROP TRIGGER reject_album_delivery ON delivery_jobs")
        .execute(&f.state.pool)
        .await
        .unwrap();
    let changed = plamenu::music_album_access::update(&f.state, f.owner.id, f.album, &input)
        .await
        .unwrap();
    assert_eq!(changed.visibility, "private");
    assert!(matches!(
        plamenu::music_album_access::update(&f.state, f.owner.id, f.album, &input).await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    let updated = plamenu_db::status::find_by_id(&f.state.pool, published.id)
        .await
        .unwrap()
        .unwrap();
    let doc = plamenu::note::note_for_status(&f.state, &updated, &f.owner)
        .await
        .unwrap();
    assert!(!doc.to_string().contains("Private album journal"));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn album_grants_collect_the_artist_without_implicitly_granting_private_tracks(pool: PgPool) {
    let f = fixture(pool).await;
    let bob = create_local_account(&f.state.pool, "bob", "Bob").await;
    let track = asset(&f, "private").await;
    assert_eq!(access(&f, "private", false).await.status(), StatusCode::OK);
    plamenu::music_album_access::grant(&f.state, f.owner.id, f.album, bob.id, true)
        .await
        .unwrap();
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, Some(bob.id))
            .await
            .is_ok()
    );
    assert!(
        music_assets::get(&f.state, track, Some(bob.id))
            .await
            .is_err()
    );
    music_saved::save(
        &f.state,
        bob.id,
        &Reference {
            kind: SavedKind::Album,
            id: f.album.to_string(),
        },
    )
    .await
    .unwrap();
    let collected =
        plamenu::music_mine::page(&f.state, bob.id, &plamenu::music_mine::Request::default())
            .await
            .unwrap();
    assert!(
        collected
            .items
            .iter()
            .any(|item| item.reference.kind == SavedKind::Artist
                && item.reference.id == f.artist.to_string()
                && item.membership.implicit)
    );
    let album = music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
        .await
        .unwrap();
    plamenu::music_album_access::update(
        &f.state,
        f.owner.id,
        f.album,
        &plamenu::music_album_access::Edit {
            revision: album.revision.to_string(),
            visibility: "private".into(),
            share_tracks: true,
        },
    )
    .await
    .unwrap();
    assert!(
        music_assets::get(&f.state, track, Some(bob.id))
            .await
            .is_ok()
    );
    assert!(music_assets::get(&f.state, track, None).await.is_err());
    plamenu::music_album_access::grant(&f.state, f.owner.id, f.album, bob.id, false)
        .await
        .unwrap();
    assert!(
        music_assets::get(&f.state, track, Some(bob.id))
            .await
            .is_err()
    );
    assert!(
        plamenu::music_mine::page(&f.state, bob.id, &plamenu::music_mine::Request::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        music_saved::page(&f.state, bob.id, None)
            .await
            .unwrap()
            .items[0]
            .source
            .is_none()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn source_mode_never_exposes_private_siblings_and_independent_direct_grants_survive(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let bob = create_local_account(&f.state.pool, "bob", "Bob").await;
    asset(&f, "public").await;
    let private = asset(&f, "private").await;
    assert_eq!(access(&f, "source", true).await.status(), StatusCode::OK);
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, None)
            .await
            .is_ok()
    );
    assert!(music_assets::get(&f.state, private, None).await.is_err());
    plamenu::music_album_access::grant(&f.state, f.owner.id, f.album, bob.id, true)
        .await
        .unwrap();
    assert!(
        music_assets::get(&f.state, private, Some(bob.id))
            .await
            .is_ok()
    );
    music_assets::grant(&f.state, f.owner.id, private, bob.id, true)
        .await
        .unwrap();
    plamenu::music_album_access::grant(&f.state, f.owner.id, f.album, bob.id, false)
        .await
        .unwrap();
    assert!(
        music_assets::get(&f.state, private, Some(bob.id))
            .await
            .is_ok()
    );
    music_assets::grant(&f.state, f.owner.id, private, bob.id, false)
        .await
        .unwrap();
    assert!(
        music_assets::get(&f.state, private, Some(bob.id))
            .await
            .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn empty_public_album_has_independent_metadata_without_publishing_tracks(pool: PgPool) {
    let f = fixture(pool).await;
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, None)
            .await
            .is_err()
    );
    assert_eq!(access(&f, "public", false).await.status(), StatusCode::OK);
    let album = music_entities::get(&f.state, Kind::Album, f.album, None)
        .await
        .unwrap();
    assert_eq!(album.name, "Album credits");
    let doc = music_entities::document(&f.state, Kind::Album, f.album)
        .await
        .unwrap();
    assert_eq!(doc["type"], "Album");
    assert_eq!(doc["summary"], "Private album journal");
    let stats = music_entities::statistics(&f.state, Kind::Album, f.album, None)
        .await
        .unwrap();
    assert_eq!(stats.track_count, 0);
    assert_eq!(
        music_entities::statistics(&f.state, Kind::Artist, f.artist, None)
            .await
            .unwrap()
            .album_count,
        1
    );
    assert!(
        music_entities::catalogue(&f.state, Kind::Album, None, false, "Album credits", None)
            .await
            .unwrap()
            .iter()
            .any(|row| row.id == f.album)
    );
    assert!(
        music_entities::catalogue(&f.state, Kind::Artist, None, false, "Album performer", None)
            .await
            .unwrap()
            .iter()
            .any(|row| row.id == f.artist)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_publications")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn signed_empty_album_and_artist_follows_retract_together_when_metadata_becomes_private(
    pool: PgPool,
) {
    let mut f = fixture(pool).await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    f.state = test_state_with(
        f.state.pool.clone(),
        StubFederation::with_actors([remote.actor.clone()]),
    );
    assert_eq!(access(&f, "public", false).await.status(), StatusCode::OK);
    for (position, (kind, id)) in [(Kind::Artist, f.artist), (Kind::Album, f.album)]
        .into_iter()
        .enumerate()
    {
        let target = music_entities::uri(TEST_DOMAIN, kind, id);
        let follow = json!({"id":format!("https://songhive.test/follow/empty-{position}"),"type":"Follow","actor":remote.actor.id,"object":target});
        let body = serde_json::to_vec(&follow).unwrap();
        let signed =
            remote
                .signer()
                .sign_post(TEST_DOMAIN, "/inbox", &body, std::time::SystemTime::now());
        let response = plamenu::build_router(f.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/inbox")
                    .header("host", signed.host)
                    .header("date", signed.date)
                    .header("digest", signed.digest)
                    .header("signature", signed.signature)
                    .header("Content-Type", "application/activity+json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let subscriber = plamenu_db::account::find_by_uri(&f.state.pool, &remote.actor.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            plamenu::music_follows::list(&f.state, subscriber.id)
                .await
                .unwrap()
                .len(),
            position + 1
        );
        assert!(
            plamenu_db::follow::find(&f.state.pool, subscriber.id, f.owner.id)
                .await
                .unwrap()
                .is_none()
        );
    }
    let previous = music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
        .await
        .unwrap();
    plamenu::music_album_access::update(
        &f.state,
        f.owner.id,
        f.album,
        &plamenu::music_album_access::Edit {
            revision: previous.revision.to_string(),
            visibility: "private".into(),
            share_tracks: false,
        },
    )
    .await
    .unwrap();
    let subscriber = plamenu_db::account::find_by_uri(&f.state.pool, &remote.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        plamenu::music_follows::list(&f.state, subscriber.id)
            .await
            .unwrap()
            .is_empty()
    );
    for (kind, id) in [(Kind::Artist, f.artist), (Kind::Album, f.album)] {
        assert!(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM delivery_jobs WHERE activity->>'type'='Delete' AND activity->'object'->>'id'=$1)").bind(music_entities::uri(TEST_DOMAIN,kind,id)).fetch_one(&f.state.pool).await.unwrap());
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn private_album_notes_are_redacted_while_public_track_credits_remain(pool: PgPool) {
    let f = fixture(pool).await;
    let asset = id::next();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Public track',$3,$4,'public',$5,$6,100,100,$7,10,'mp3')").bind(asset).bind(f.owner.id).bind(f.artist).bind(f.album).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&f.state.pool).await.unwrap();
    let published = music_publications::publish(&f.state, &f.owner, asset)
        .await
        .unwrap();
    assert_eq!(access(&f, "private", false).await.status(), StatusCode::OK);
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, None)
            .await
            .is_err()
    );
    assert!(
        music_entities::document(&f.state, Kind::Album, f.album)
            .await
            .is_err()
    );
    assert!(music_assets::get(&f.state, asset, None).await.is_ok());
    assert_eq!(
        music_entities::statistics(&f.state, Kind::Artist, f.artist, None)
            .await
            .unwrap()
            .album_count,
        0
    );
    let status = plamenu_db::status::find_by_id(&f.state.pool, published.id)
        .await
        .unwrap()
        .unwrap();
    let doc = plamenu::note::note_for_status(&f.state, &status, &f.owner)
        .await
        .unwrap();
    assert_eq!(doc["track"]["album"]["name"], "Album credits");
    assert!(!doc.to_string().contains("Private album journal"));
    assert!(!doc.to_string().contains("private-journal"));
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
            .await
            .is_ok()
    );
}
