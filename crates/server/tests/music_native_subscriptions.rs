mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{auth::hash_secret, music_entities::Kind};
use plamenu_db::{PgPool, id, user};
use tower::ServiceExt;
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_artist_album_and_library_controls_use_music_follows_and_return_to_the_resource(
    pool: PgPool,
) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    user::create(
        &pool,
        viewer,
        Some("bob@example.test"),
        &common::fixture_password_hash(),
    )
    .await
    .unwrap();
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let album = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name) VALUES($1,$2,$3,'Album')")
        .bind(album)
        .bind(owner)
        .bind(artist)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,album_id,title,visibility,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration) VALUES($1,$2,$3,$4,'Track','public',$5,$6,'mp3',1,1,$7,2)").bind(asset).bind(owner).bind(artist).bind(album).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    let library = plamenu::music_collections::create(
        &state,
        owner,
        &plamenu::music_collections::New {
            name: "Library".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state.clone());
    let login = app
        .clone()
        .oneshot(
            Request::post("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "email=bob%40example.test&password=fixture-password",
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
        .unwrap()
        .to_owned();
    let csrf = hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    for (path, target) in [
        (
            format!("/music/artists/{artist}"),
            plamenu::music_entities::uri(&state.config.domain, Kind::Artist, artist),
        ),
        (
            format!("/music/albums/{album}"),
            plamenu::music_entities::uri(&state.config.domain, Kind::Album, album),
        ),
        (
            format!("/music/collections/{}", library.id),
            plamenu::music_library_federation::uri(&state.config.domain, library.id),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(&path)
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("music-subscription"));
        assert!(html.contains(&target));
        for operation in ["follow", "unfollow"] {
            let form = serde_urlencoded::to_string([
                ("csrf", csrf.as_str()),
                ("url", target.as_str()),
                ("operation", operation),
                ("return_to", path.as_str()),
            ])
            .unwrap();
            let response = app
                .clone()
                .oneshot(
                    Request::post("/web/music/follows")
                        .header("cookie", &cookie)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(form))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(response.headers()["location"], path);
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM music_follows WHERE follower_id=$1 AND target_uri=$2",
            )
            .bind(viewer)
            .bind(&target)
            .fetch_one(&state.pool)
            .await
            .unwrap();
            assert_eq!(count, i64::from(operation == "follow"));
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM follows WHERE account_id=$1")
        .bind(viewer)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
