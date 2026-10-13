mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{StubFederation, create_local_account, test_state_with_store};
use http_body_util::BodyExt;
use plamenu::{
    AppState, music_collections,
    storage::{MediaStore, MemoryStore},
};
use plamenu_db::{PgPool, id};
use std::sync::Arc;
use tower::ServiceExt;
#[sqlx::test(migrations = "../db/migrations")]
async fn embed_codec_controls_clamp_anonymous_quality_and_preserve_readonly_frames(pool: PgPool) {
    let (state, _owner, _artist, track) = fixture(pool).await;
    let app = plamenu::build_router(state.clone());
    for kind in ["audio", "script", "iframe"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/music/embed-code/assets/{track}?format={kind}&codec=opus&bitrate=320"
                    ))
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
        assert!(html.contains("Native track audio format"));
        assert!(html.contains("Embedded playback uses the anonymous bitrate limit"));
        assert!(html.contains(if kind == "script" {
            "data-codec=&quot;opus&quot;"
        } else if kind == "iframe" {
            "codec=opus"
        } else {
            "format=opus"
        }));
        assert!(!html.contains("bitrate=320"));
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/music/embed/assets/{track}?codec=opus&bitrate=320"
                ))
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
    assert!(html.contains(&format!(
        "/music/embed/assets/{track}/audio?format=opus&amp;bitrate=192"
    )));
    assert!(!html.contains("name=\"csrf\""));
    assert!(!html.contains("autoplay="));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/music/embed-code/assets/{track}?codec=invalid"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    sqlx::query("UPDATE music_assets SET visibility='private' WHERE id=$1")
        .bind(track)
        .execute(&state.pool)
        .await
        .unwrap();
    let response=app.oneshot(Request::builder().uri(format!("/music/embed/assets/{track}?codec=opus&bitrate=320&token=owner-cookie-cannot-elevate")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
async fn fixture(pool: PgPool) -> (AppState, i64, i64, i64) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let media = Arc::new(MemoryStore::default());
    let state = test_state_with_store(pool, StubFederation::with_actors([]), media.clone());
    let artist = id::next();
    let track = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Embed artist')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,$3,'Public <safe> track','public',$4,$5,32,32,$6,9,'mp3')")
        .bind(track).bind(owner).bind(artist).bind(format!("music{track}.orig")).bind(format!("music{track}.mp3"))
        .bind(format!("{track:064x}")).execute(&state.pool).await.unwrap();
    media
        .put(
            &format!("music{track}.mp3"),
            b"0123456789abcdefghijklmnopqrstuv".to_vec(),
        )
        .await
        .unwrap();
    (state, owner, artist, track)
}
async fn request(app: &axum::Router, path: &str, headers: &[(&str, &str)]) -> Response {
    let mut request = Request::builder().uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
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
fn frame_policy(response: &Response, allowed: bool) {
    let csp = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok());
    if allowed {
        assert!(csp.is_some_and(|csp| csp.contains("frame-ancestors *")));
        assert!(!response.headers().contains_key(header::X_FRAME_OPTIONS));
    } else {
        assert!(!csp.is_some_and(|csp| csp.contains("frame-ancestors *")));
        if response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
        {
            assert!(csp.is_some_and(|csp| csp.contains("frame-ancestors 'none'")));
            assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
        }
    }
}
#[sqlx::test(migrations = "../db/migrations")]
async fn public_music_frames_and_snippets_revoke_metadata_and_byte_ranges_before_cached_responses(
    pool: PgPool,
) {
    let (state, _, artist, track) = fixture(pool).await;
    let app = plamenu::build_router(state.clone());
    let frame = format!("/music/embed/assets/{track}");
    let response = request(&app, &frame, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    frame_policy(&response, true);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let html = text(response).await;
    assert!(html.contains("Public &lt;safe&gt; track"));
    assert!(!html.contains("name=\"csrf\""));
    assert!(html.contains(&format!("/music/embed/assets/{track}/audio")));
    let artist_page = request(&app, &format!("/music/embed/artists/{artist}"), &[]).await;
    assert_eq!(artist_page.status(), StatusCode::OK);
    frame_policy(&artist_page, true);
    for format in ["audio", "markdown", "script", "iframe"] {
        let response = request(
            &app,
            &format!("/music/embed-code/assets/{track}?format={format}"),
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        frame_policy(&response, false);
        let html = text(response).await;
        assert!(html.contains("id=\"music-embed-code\""));
        assert!(html.contains("Public &lt;safe&gt; track"));
    }
    let audio = format!("/music/embed/assets/{track}/audio");
    let response = request(&app, &audio, &[("Range", "bytes=2-5")]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(text(response).await, "2345");
    sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(track)
        .execute(&state.pool)
        .await
        .unwrap();
    for path in [
        frame,
        audio,
        format!("/music/embed-code/assets/{track}"),
        format!("/music/embed/artists/{artist}"),
    ] {
        let response = request(
            &app,
            &path,
            &[("If-None-Match", "*"), ("Range", "bytes=2-5")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        frame_policy(&response, false);
    }
}
#[sqlx::test(migrations = "../db/migrations")]
async fn public_collection_widgets_keep_playlist_repeats_and_hide_inaccessible_entries_before_cap(
    pool: PgPool,
) {
    let (state, owner, _, track) = fixture(pool).await;
    let parent = music_collections::create(
        &state,
        owner,
        &music_collections::New {
            name: "Embed playlist".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    for position in 1..=251 {
        sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) VALUES($1,$2,$3,$4)")
            .bind(id::next()).bind(parent.id).bind(track).bind(position).execute(&state.pool).await.unwrap();
    }
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/embed/collections/{}", parent.id);
    let response = request(&app, &path, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert_eq!(html.matches("<audio ").count(), 250);
    assert!(html.contains("remaining tracks"));
    assert_eq!(
        request(
            &app,
            &format!("/music/embed-code/collections/{}?format=html", parent.id),
            &[]
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    sqlx::query("DELETE FROM music_collection_entries WHERE collection_id=$1 AND position>2")
        .bind(parent.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let html = text(request(&app, &path, &[]).await).await;
    assert_eq!(html.matches("<audio ").count(), 2);
    assert_eq!(
        request(
            &app,
            &format!("/music/embed-code/collections/{}?format=html", parent.id),
            &[]
        )
        .await
        .status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(track)
        .execute(&state.pool)
        .await
        .unwrap();
    let html = text(request(&app, &path, &[]).await).await;
    assert!(!html.contains("Public &lt;safe&gt; track"));
    assert!(!html.contains("<audio "));
    sqlx::query(
        "UPDATE music_collections SET visibility='private',revision=revision+1 WHERE id=$1",
    )
    .bind(parent.id)
    .execute(&state.pool)
    .await
    .unwrap();
    let response = request(&app, &path, &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    frame_policy(&response, false);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn private_music_widgets_ignore_valid_owner_authentication_and_share_capabilities(
    pool: PgPool,
) {
    use plamenu::{
        auth::{generate_secret, hash_password, hash_secret},
        music_saved::{Kind, Reference},
        music_shares::{self, New},
    };
    use plamenu_db::{oauth, user};
    let (state, owner, _, track) = fixture(pool).await;
    let user = user::create(
        &state.pool,
        owner,
        Some("alice@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let application = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "embed-tests",
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
        &state.pool,
        &hash_secret(&token),
        application.id,
        Some(user.id),
        "read write",
    )
    .await
    .unwrap();
    sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(track)
        .execute(&state.pool)
        .await
        .unwrap();
    let share = music_shares::create(
        &state,
        owner,
        &New {
            reference: Reference {
                kind: Kind::Track,
                id: track.to_string(),
            },
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state);
    let auth = format!("Bearer {token}");
    assert_eq!(
        request(
            &app,
            &format!("/api/v1/music/assets/{track}"),
            &[("Authorization", &auth)]
        )
        .await
        .status(),
        StatusCode::OK
    );
    for path in [
        format!("/music/embed/assets/{track}?token={}", share.token),
        format!("/music/embed/assets/{track}/audio?token={}", share.token),
        format!("/music/embed-code/assets/{track}?token={}", share.token),
    ] {
        let response = request(&app, &path, &[("Authorization", &auth)]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        frame_policy(&response, false);
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn music_oembed_uses_exact_local_identity_and_root_artwork_revokes_with_public_source(
    pool: PgPool,
) {
    let (state, owner, artist, _) = fixture(pool).await;
    let album = id::next();
    let image = id::next();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,visibility) VALUES($1,$2,$3,'Public cover album','public')")
        .bind(album).bind(owner).bind(artist).execute(&state.pool).await.unwrap();
    let file = format!("musicart{image}.png");
    sqlx::query("INSERT INTO music_artworks(id,owner_id,album_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,'cover',$4,'image/png',4,1,1,$5)")
        .bind(image).bind(owner).bind(album).bind(&file).bind(id::next()).execute(&state.pool).await.unwrap();
    state.media.put(&file, b"test".to_vec()).await.unwrap();
    let app = plamenu::build_router(state.clone());
    let frame = format!("/music/embed/albums/{album}");
    let response = request(&app, &frame, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        text(response)
            .await
            .contains(&format!("/music/embed/artwork/{image}"))
    );
    let artwork = format!("/music/embed/artwork/{image}");
    assert_eq!(request(&app, &artwork, &[]).await.status(), StatusCode::OK);
    let own = format!("https://plamenu.test/music/albums/{album}");
    let path = format!(
        "/api/oembed?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("url", &own)
            .append_pair("maxwidth", "9000")
            .finish()
    );
    let response = request(&app, &path, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&text(response).await).unwrap();
    assert_eq!(json["type"], "rich");
    assert_eq!(json["width"], 800);
    assert_eq!(json["title"], "Public cover album");
    assert!(json["html"].as_str().unwrap().contains(&frame));
    for raw in [
        format!("https://elsewhere.test/music/albums/{album}"),
        format!("https://plamenu.test/music/albums/{album}?token=ignored"),
        format!("https://user@plamenu.test/music/albums/{album}"),
    ] {
        let path = format!(
            "/api/oembed?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("url", &raw)
                .finish()
        );
        assert_eq!(
            request(&app, &path, &[]).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    sqlx::query("UPDATE music_albums SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(album)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, &path, &[]).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&app, &artwork, &[("If-None-Match", "*")])
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = request(&app, &frame, &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    frame_policy(&response, false);
    let body = text(response).await;
    assert!(body.contains("Music is unavailable"));
    assert!(!body.contains("Public cover album"));
}

async fn owner_cookie(state: &AppState, owner: i64) -> String {
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    let user = user::create(
        &state.pool,
        owner,
        Some("alice@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let application = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "native-embed-test",
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
        &state.pool,
        &hash_secret(&token),
        application.id,
        Some(user.id),
        "read write",
    )
    .await
    .unwrap();
    format!("__Host-plamenu_session={token}")
}
#[sqlx::test(migrations = "../db/migrations")]
async fn native_entity_embed_links_follow_public_access_including_legacy_album_visibility(
    pool: PgPool,
) {
    let (state, owner, artist, track) = fixture(pool).await;
    let cookie = owner_cookie(&state, owner).await;
    sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(track)
        .execute(&state.pool)
        .await
        .unwrap();
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/artists/{artist}");
    let response = request(&app, &path, &[("Cookie", &cookie)]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !text(response)
            .await
            .contains(&format!("/music/embed-code/artists/{artist}"))
    );
    let album = id::next();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,visibility) VALUES($1,$2,$3,'Legacy album','source')")
        .bind(album).bind(owner).bind(artist).execute(&state.pool).await.unwrap();
    sqlx::query(
        "UPDATE music_assets SET visibility='public',album_id=$2,revision=revision+1 WHERE id=$1",
    )
    .bind(track)
    .bind(album)
    .execute(&state.pool)
    .await
    .unwrap();
    let response = request(&app, &path, &[("Cookie", &cookie)]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        text(response)
            .await
            .contains(&format!("/music/embed-code/artists/{artist}"))
    );
    let response = request(
        &app,
        &format!("/music/albums/{album}"),
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        text(response)
            .await
            .contains(&format!("/music/embed-code/albums/{album}"))
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn music_embed_media_cors_exposes_public_ranges_without_credentials_or_private_access(
    pool: PgPool,
) {
    let (state, _, _, track) = fixture(pool).await;
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/embed/assets/{track}/audio");
    let response = request(
        &app,
        &path,
        &[
            ("Origin", "https://publisher.example"),
            ("Range", "bytes=0-3"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
    );
    assert!(
        response.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS]
            .to_str()
            .unwrap()
            .contains("Content-Range")
    );
    assert_eq!(text(response).await, "0123");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri(&path)
                .header(header::ORIGIN, "https://publisher.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "range")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
        "GET, HEAD, OPTIONS"
    );
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
    );
    sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(track)
        .execute(&state.pool)
        .await
        .unwrap();
    let response = request(
        &app,
        &path,
        &[
            ("Origin", "https://publisher.example"),
            ("If-None-Match", "*"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert!(!text(response).await.contains("Public <safe> track"));
}
