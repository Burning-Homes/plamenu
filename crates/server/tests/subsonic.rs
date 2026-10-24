mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    auth::hash_password,
    music_clients::{self, Request as ClientRequest},
    music_collections,
};
use plamenu_db::{PgPool, id, user};
use serde_json::Value;
use tower::ServiceExt;
struct Fixture {
    state: AppState,
    owner: i64,
    artist: i64,
    album: i64,
    track: i64,
    hidden: i64,
    password: String,
    manage: String,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let bob = create_local_account(&pool, "bob", "Bob").await.id;
    for (account, email) in [(owner, "alice@plamenu.test"), (bob, "bob@plamenu.test")] {
        user::create(
            &pool,
            account,
            Some(email),
            &hash_password("fixture-password").unwrap(),
        )
        .await
        .unwrap();
    }
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let album = id::next();
    let track = id::next();
    let hidden = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name,description) VALUES($1,$2,'The Performer','Biography <safe>')").bind(artist).bind(owner).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,description,release_year,genres) VALUES($1,$2,$3,'An album','Album notes',2026,ARRAY['Jazz'])").bind(album).bind(owner).bind(artist).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format,release_year,genre,track_number) VALUES($1,$2,'Track & <title>',$3,$4,'private',$5,$6,100,100,$7,12.5,'mp3',2026,'Jazz',2)").bind(track).bind(owner).bind(artist).bind(album).bind(format!("music{track}.orig")).bind(format!("music{track}.mp3")).bind(format!("{track:064x}")).execute(&state.pool).await.unwrap();
    let other_artist = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Hidden performer')")
        .bind(other_artist)
        .bind(bob)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Hidden song',$3,'private',$4,$5,100,100,$6,10,'mp3')").bind(hidden).bind(bob).bind(other_artist).bind(format!("music{hidden}.orig")).bind(format!("music{hidden}.mp3")).bind(format!("{hidden:064x}")).execute(&state.pool).await.unwrap();
    let password = music_clients::create(
        &state,
        owner,
        None,
        &ClientRequest {
            label: "Read client".into(),
            days: 90,
            can_manage: false,
            broadcast_playing: false,
        },
    )
    .await
    .unwrap()
    .password
    .clone();
    let manage = music_clients::create(
        &state,
        owner,
        None,
        &ClientRequest {
            label: "Managing client".into(),
            days: 90,
            can_manage: true,
            broadcast_playing: false,
        },
    )
    .await
    .unwrap()
    .password
    .clone();
    Fixture {
        state,
        owner,
        artist,
        album,
        track,
        hidden,
        password,
        manage,
    }
}
async fn response(
    app: &axum::Router,
    method: &str,
    pairs: &[(&str, String)],
    post: bool,
) -> axum::response::Response {
    let body = serde_urlencoded::to_string(pairs).unwrap();
    let uri = if post {
        format!("/rest/{method}.view")
    } else {
        format!("/rest/{method}.view?{body}")
    };
    app.clone()
        .oneshot(
            Request::builder()
                .method(if post { "POST" } else { "GET" })
                .uri(uri)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(if post {
                    Body::from(body)
                } else {
                    Body::empty()
                })
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn read(response: axum::response::Response) -> String {
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
async fn call(app: &axum::Router, method: &str, password: &str, extra: &[(&str, String)]) -> Value {
    let mut pairs = vec![
        ("u", "alice".into()),
        ("apiKey", password.into()),
        ("f", "json".into()),
        ("v", "1.16.1".into()),
    ];
    pairs.extend_from_slice(extra);
    let response = response(app, method, &pairs, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = read(response).await;
    serde_json::from_str::<Value>(&body).unwrap()["subsonic-response"].clone()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn protocol_formats_version_forms_authentication_and_reference_noops_are_compatible(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let app = plamenu::build_router(f.state.clone());
    for (extra, code) in [
        (&[("u", "alice".into())][..], 10),
        (&[("u", "alice".into()), ("p", "bad".into())][..], 40),
    ] {
        let response = response(&app, "ping", extra, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["cache-control"]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
        let xml = read(response).await;
        let document = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(document.root_element().attribute("status"), Some("failed"));
        assert_eq!(
            document
                .descendants()
                .find(|node| node.has_tag_name("error"))
                .unwrap()
                .attribute("code")
                .unwrap(),
            code.to_string()
        );
    }
    let mut pairs = vec![("u", "alice".into()), ("apiKey", f.password.clone())];
    let xml = read(response(&app, "ping", &pairs, false).await).await;
    let document = roxmltree::Document::parse(&xml).unwrap();
    assert_eq!(document.root_element().attribute("status"), Some("ok"));
    assert_eq!(
        document.root_element().tag_name().namespace(),
        Some("http://subsonic.org/restapi")
    );
    pairs.extend([("f", "jsonp".into()), ("callback", "client.done".into())]);
    let valid = read(response(&app, "ping", &pairs, true).await).await;
    assert!(valid.starts_with("client.done("));
    pairs.push(("callback", "evil();alert(1)//".into()));
    let safe = read(response(&app, "ping", &pairs, false).await).await;
    assert!(serde_json::from_str::<Value>(&safe).is_ok());
    assert!(!safe.contains("alert(1)"));
    let newer = call(&app, "ping", &f.password, &[("v", "2.0.0".into())]).await;
    assert_eq!(newer["error"]["code"], 30);
    for method in [
        "ping",
        "getLicense",
        "getOpenSubsonicExtensions",
        "setRating",
        "savePlayQueue",
        "getPlayQueue",
        "getVideos",
        "getBookmarks",
        "getLyrics",
        "getLyricsBySongId",
    ] {
        let value = call(&app, method, &f.password, &[]).await;
        assert_eq!(value["status"], "ok", "{method}: {value}");
    }
    assert_eq!(
        call(&app, "unknown", &f.password, &[]).await["error"]["code"],
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn catalogue_methods_use_current_private_tracks_metadata_album_order_and_folder_membership(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let app = plamenu::build_router(f.state.clone());
    let folder = music_collections::create(
        &f.state,
        f.owner,
        &music_collections::New {
            name: "Folder".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    music_collections::add(
        &f.state,
        f.owner,
        folder.id,
        &music_collections::Add {
            asset_id: Some(f.track.to_string()),
            status_id: None,
            episode_id: None,
            remote_id: None,
            revision: folder.revision.to_string(),
        },
    )
    .await
    .unwrap();
    let folders = call(&app, "getMusicFolders", &f.password, &[]).await;
    assert_eq!(
        folders["musicFolders"]["musicFolder"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for (method, extra, key) in [
        ("getIndexes", vec![], "indexes"),
        ("getArtists", vec![], "artists"),
        ("getArtist", vec![("id", f.artist.to_string())], "artist"),
        ("getAlbum", vec![("id", f.album.to_string())], "album"),
        ("getSong", vec![("id", f.track.to_string())], "song"),
        (
            "getMusicDirectory",
            vec![("id", f.artist.to_string())],
            "directory",
        ),
        (
            "getArtistInfo",
            vec![("id", f.artist.to_string())],
            "artistInfo",
        ),
        (
            "getArtistInfo2",
            vec![("id", f.artist.to_string())],
            "artistInfo2",
        ),
        (
            "getAlbumInfo",
            vec![("id", f.album.to_string())],
            "albumInfo",
        ),
        (
            "getAlbumInfo2",
            vec![("id", f.album.to_string())],
            "albumInfo2",
        ),
        ("getGenres", vec![], "genres"),
        (
            "getSongsByGenre",
            vec![("genre", "Jazz".into())],
            "songsByGenre",
        ),
        ("getRandomSongs", vec![], "randomSongs"),
        ("search", vec![("query", "Track".into())], "searchResult"),
        ("search2", vec![("query", "Track".into())], "searchResult2"),
        ("search3", vec![("query", "Track".into())], "searchResult3"),
        (
            "getAlbumList",
            vec![
                ("type", "byYear".into()),
                ("fromYear", "2025".into()),
                ("toYear", "2027".into()),
            ],
            "albumList",
        ),
        (
            "getAlbumList2",
            vec![("type", "byGenre".into()), ("genre", "Jazz".into())],
            "albumList2",
        ),
    ] {
        let value = call(&app, method, &f.password, &extra).await;
        assert_eq!(value["status"], "ok", "{method}: {value}");
        assert!(value.get(key).is_some(), "{method}: {value}");
        assert!(!value.to_string().contains("Hidden"), "{method}");
    }
    let legacy = call(
        &app,
        "search",
        &f.password,
        &[("artist", "Performer".into())],
    )
    .await;
    assert_eq!(
        legacy["searchResult"]["match"][0]["id"],
        f.artist.to_string()
    );
    assert_eq!(legacy["searchResult"]["match"][0]["isDir"], true);
    let song = call(&app, "getSong", &f.password, &[("id", f.track.to_string())]).await;
    assert_eq!(song["song"]["duration"], 12);
    assert_eq!(song["song"]["albumId"], f.album.to_string());
    assert!(!song.to_string().contains(".orig"));
    assert!(!song.to_string().contains("file_name"));
    let songs = call(
        &app,
        "getSongsByGenre",
        &f.password,
        &[
            ("genre", "Jazz".into()),
            ("musicFolderId", folder.id.to_string()),
        ],
    )
    .await;
    assert_eq!(songs["songsByGenre"]["song"].as_array().unwrap().len(), 1);
    let hidden = call(
        &app,
        "getSong",
        &f.password,
        &[("id", f.hidden.to_string())],
    )
    .await;
    assert_eq!(hidden["error"]["code"], 70);
    assert!(!hidden.to_string().contains("Hidden"));
    sqlx::query("UPDATE music_assets SET title='Current title' WHERE id=$1")
        .bind(f.track)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        call(&app, "getSong", &f.password, &[("id", f.track.to_string())]).await["song"]["title"],
        "Current title"
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn protocol_playlist_crud_preserves_repeats_current_indices_scope_and_native_revision(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let app = plamenu::build_router(f.state.clone());
    let input = [
        ("name", "Client playlist".into()),
        ("songId", f.track.to_string()),
        ("songId", f.track.to_string()),
        ("songId", f.hidden.to_string()),
    ];
    assert_eq!(
        call(&app, "createPlaylist", &f.password, &input).await["error"]["code"],
        50
    );
    let created = call(&app, "createPlaylist", &f.manage, &input).await;
    assert_eq!(created["status"], "ok");
    let playlist = created["playlist"]["id"].as_str().unwrap();
    assert_eq!(created["playlist"]["entry"].as_array().unwrap().len(), 2);
    let row = music_collections::get(&f.state, playlist.parse().unwrap(), Some(f.owner))
        .await
        .unwrap();
    assert_eq!(row.name, "Client playlist");
    let list = call(&app, "getPlaylists", &f.password, &[]).await;
    assert_eq!(list["playlists"]["playlist"].as_array().unwrap().len(), 1);
    let updated = call(
        &app,
        "updatePlaylist",
        &f.manage,
        &[
            ("playlistId", playlist.into()),
            ("comment", "Updated comment".into()),
            ("songIndexToRemove", "0".into()),
            ("songIdToAdd", f.track.to_string()),
        ],
    )
    .await;
    assert_eq!(updated["status"], "ok");
    let current = call(&app, "getPlaylist", &f.password, &[("id", playlist.into())]).await;
    assert_eq!(current["playlist"]["comment"], "Updated comment");
    assert_eq!(current["playlist"]["entry"].as_array().unwrap().len(), 2);
    let new = music_collections::get(&f.state, row.id, Some(f.owner))
        .await
        .unwrap();
    assert!(new.revision > row.revision);
    let replaced = call(
        &app,
        "createPlaylist",
        &f.manage,
        &[
            ("playlistId", playlist.into()),
            ("songId", f.hidden.to_string()),
        ],
    )
    .await;
    assert_eq!(replaced["status"], "ok");
    assert_eq!(replaced["playlist"]["songCount"], 0);
    assert_eq!(
        call(
            &app,
            "deletePlaylist",
            &f.password,
            &[("id", playlist.into())]
        )
        .await["error"]["code"],
        50
    );
    assert_eq!(
        call(
            &app,
            "deletePlaylist",
            &f.manage,
            &[("id", playlist.into())]
        )
        .await["status"],
        "ok"
    );
    assert_eq!(
        call(&app, "getPlaylist", &f.manage, &[("id", playlist.into())]).await["error"]["code"],
        70
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn stars_and_explicit_completed_plays_reuse_native_favourites_opt_out_and_replay_keys(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let app = plamenu::build_router(f.state.clone());
    let songs = [
        ("id", f.track.to_string()),
        ("id", f.track.to_string()),
        ("id", f.hidden.to_string()),
    ];
    assert_eq!(
        call(&app, "star", &f.password, &songs).await["error"]["code"],
        50
    );
    assert_eq!(call(&app, "star", &f.manage, &songs).await["status"], "ok");
    for method in ["getStarred", "getStarred2"] {
        let key = if method == "getStarred" {
            "starred"
        } else {
            "starred2"
        };
        let value = call(&app, method, &f.password, &[]).await;
        assert_eq!(value[key]["song"].as_array().unwrap().len(), 1);
        assert_eq!(value[key]["song"][0]["id"], f.track.to_string());
        assert!(value[key]["song"][0]["starred"].is_string());
    }
    let song = call(&app, "getSong", &f.password, &[("id", f.track.to_string())]).await;
    assert!(song["song"]["starred"].is_string());
    assert!(song["song"]["created"].is_string());
    assert_eq!(song["song"]["playCount"], 0);
    assert_eq!(
        call(&app, "unstar", &f.manage, &songs).await["status"],
        "ok"
    );
    assert_eq!(
        call(&app, "getStarred2", &f.password, &[]).await["starred2"]["song"],
        serde_json::json!([])
    );
    let time = ((time::OffsetDateTime::now_utc() - time::Duration::hours(1)).unix_timestamp()
        * 1000)
        .to_string();
    let submission = [("id", f.track.to_string()), ("time", time)];
    assert_eq!(
        call(&app, "scrobble", &f.password, &submission).await["error"]["code"],
        50
    );
    for _ in 0..2 {
        assert_eq!(
            call(&app, "scrobble", &f.manage, &submission).await["status"],
            "ok"
        );
    }
    let history = plamenu::music_listening::history(&f.state, f.owner, None)
        .await
        .unwrap();
    assert_eq!(history.items.len(), 1);
    assert!(history.items[0].counted_at.contains('T'));
    let prefs = plamenu::music_listening::preferences(&f.state, f.owner)
        .await
        .unwrap();
    plamenu::music_listening::set_preferences(
        &f.state,
        f.owner,
        &plamenu::music_listening::PreferenceChange {
            revision: prefs.revision.to_string(),
            history_enabled: false,
            minimum_seconds: 30,
            minimum_percent: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        call(&app, "scrobble", &f.manage, &[("id", f.track.to_string())]).await["status"],
        "ok"
    );
    assert_eq!(
        plamenu::music_listening::history(&f.state, f.owner, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    plamenu::music_listening::erase(&f.state, f.owner)
        .await
        .unwrap();
    assert!(
        plamenu::music_listening::history(&f.state, f.owner, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn now_playing_is_explicit_private_by_default_opt_in_current_and_ttl_bounded(pool: PgPool) {
    let f = fixture(pool).await;
    let app = plamenu::build_router(f.state.clone());
    let bob: i64 =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username='bob' AND domain IS NULL")
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    let listener = music_clients::create(
        &f.state,
        bob,
        None,
        &ClientRequest {
            label: "Listener".into(),
            days: 90,
            can_manage: false,
            broadcast_playing: false,
        },
    )
    .await
    .unwrap();
    let report = [
        ("id", f.track.to_string()),
        ("c", "Music client".into()),
        ("submission", "false".into()),
    ];
    assert_eq!(
        call(&app, "scrobble", &f.manage, &report).await["status"],
        "ok"
    );
    let own = call(&app, "getNowPlaying", &f.password, &[]).await;
    assert_eq!(own["nowPlaying"]["entry"].as_array().unwrap().len(), 1);
    assert_eq!(own["nowPlaying"]["entry"][0]["playerName"], "Music client");
    let mut pairs = vec![
        ("u", "bob".into()),
        ("apiKey", listener.password.clone()),
        ("f", "json".into()),
    ];
    let foreign: Value =
        serde_json::from_str(&read(response(&app, "getNowPlaying", &pairs, false).await).await)
            .unwrap();
    assert_eq!(
        foreign["subsonic-response"]["nowPlaying"]["entry"],
        serde_json::json!([])
    );
    assert!(
        plamenu::music_listening::history(&f.state, f.owner, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let sharing = music_clients::create(
        &f.state,
        f.owner,
        None,
        &ClientRequest {
            label: "Sharing client".into(),
            days: 90,
            can_manage: true,
            broadcast_playing: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        call(&app, "nowPlaying", &sharing.password, &report).await["status"],
        "ok"
    );
    // Consent cannot widen a private track's source permission.
    let foreign: Value =
        serde_json::from_str(&read(response(&app, "getNowPlaying", &pairs, false).await).await)
            .unwrap();
    assert_eq!(
        foreign["subsonic-response"]["nowPlaying"]["entry"],
        serde_json::json!([])
    );
    plamenu::music_assets::grant(&f.state, f.owner, f.track, bob, true)
        .await
        .unwrap();
    let foreign: Value =
        serde_json::from_str(&read(response(&app, "getNowPlaying", &pairs, false).await).await)
            .unwrap();
    assert_eq!(
        foreign["subsonic-response"]["nowPlaying"]["entry"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    sqlx::query("UPDATE music_client_playing SET reported_at=now()-interval '11 minutes' WHERE account_id=$1").bind(f.owner).execute(&f.state.pool).await.unwrap();
    assert_eq!(
        call(&app, "getNowPlaying", &f.password, &[]).await["nowPlaying"]["entry"],
        serde_json::json!([])
    );
    assert_eq!(
        call(&app, "nowPlaying", &sharing.password, &report).await["status"],
        "ok"
    );
    music_clients::revoke(&f.state, f.owner, sharing.key.id)
        .await
        .unwrap();
    pairs.retain(|(name, _)| *name != "f");
    pairs.push(("f", "json".into()));
    let foreign: Value =
        serde_json::from_str(&read(response(&app, "getNowPlaying", &pairs, false).await).await)
            .unwrap();
    assert_eq!(
        foreign["subsonic-response"]["nowPlaying"]["entry"],
        serde_json::json!([])
    );
}
fn wave() -> Vec<u8> {
    let pcm = vec![0u8; 88200];
    let len = u32::try_from(pcm.len()).unwrap();
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((len + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(44100u32.to_le_bytes());
    bytes.extend(88200u32.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(len.to_le_bytes());
    bytes.extend(pcm);
    bytes
}
async fn binary(
    app: &axum::Router,
    method: &str,
    password: &str,
    id: &str,
    http: &str,
    range: Option<&str>,
) -> axum::response::Response {
    let pairs = [
        ("u", "alice"),
        ("apiKey", password),
        ("id", id),
        ("format", "raw"),
    ];
    let query = serde_urlencoded::to_string(pairs).unwrap();
    let mut req = Request::builder()
        .method(http)
        .uri(format!("/rest/{method}.view?{query}"));
    if let Some(range) = range {
        req = req.header("Range", range);
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn binary_stream_download_art_and_avatar_ranges_never_count_plays_and_stop_on_revocation(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let app = plamenu::build_router(f.state.clone());
    let bytes = wave();
    f.state
        .media
        .put(&format!("music{}.orig", f.track), bytes.clone())
        .await
        .unwrap();
    sqlx::query("UPDATE music_assets SET original_format='wav',original_bytes=$2 WHERE id=$1")
        .bind(f.track)
        .bind(i64::try_from(bytes.len()).unwrap())
        .execute(&f.state.pool)
        .await
        .unwrap();
    let streamed = binary(
        &app,
        "stream",
        &f.password,
        &f.track.to_string(),
        "GET",
        Some("bytes=0-3"),
    )
    .await;
    assert_eq!(streamed.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(streamed.headers()["Content-Type"], "audio/wav");
    assert_eq!(read(streamed).await, "RIFF");
    let download = binary(
        &app,
        "download",
        &f.password,
        &f.track.to_string(),
        "GET",
        None,
    )
    .await;
    assert!(
        download.headers()["Content-Disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    assert_eq!(
        download
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        bytes
    );
    let head = binary(
        &app,
        "stream",
        &f.password,
        &f.track.to_string(),
        "HEAD",
        None,
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    assert!(
        head.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    assert!(
        plamenu::music_listening::history(&f.state, f.owner, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let artwork = id::next();
    let key = format!("musicart{artwork}.png");
    f.state
        .media
        .put(&key, b"artwork fixture".to_vec())
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_artworks(id,owner_id,asset_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,'image',$4,'image/png',15,10,10,$5)").bind(artwork).bind(f.owner).bind(f.track).bind(&key).bind(id::next()).execute(&f.state.pool).await.unwrap();
    let art = binary(
        &app,
        "getCoverArt",
        &f.password,
        &format!("tr-{}", f.track),
        "GET",
        None,
    )
    .await;
    assert_eq!(art.status(), StatusCode::OK);
    assert_eq!(read(art).await, "artwork fixture");
    f.state
        .media
        .put("avatar-client-fixture.webp", b"avatar fixture".to_vec())
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET avatar_file_name='avatar-client-fixture.webp' WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let avatar = response(
        &app,
        "getAvatar",
        &[
            ("u", "alice".into()),
            ("apiKey", f.password.clone()),
            ("username", "alice".into()),
        ],
        false,
    )
    .await;
    assert_eq!(avatar.status(), StatusCode::OK);
    assert_eq!(read(avatar).await, "avatar fixture");
    let stream = binary(
        &app,
        "stream",
        &f.password,
        &f.track.to_string(),
        "GET",
        None,
    )
    .await;
    let keys = music_clients::list(&f.state, f.owner).await.unwrap();
    let key = keys.iter().find(|key| key.label == "Read client").unwrap();
    music_clients::revoke(&f.state, f.owner, key.id)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
    assert!(stream.into_body().collect().await.is_err());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn client_transcoding_uses_real_native_worker_codec_role_cap_and_head_without_admission(
    pool: PgPool,
) {
    let mut f = fixture(pool).await;
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        plamenu::storage::LocalDiskStore::new(root.path().join("store")).unwrap(),
    );
    f.state.media = store;
    let cfg = std::sync::Arc::make_mut(&mut f.state.config);
    cfg.media_dir = root.path().join("store");
    cfg.music.min_free_mib = 0;
    cfg.music.max_rendition_mib = 1;
    cfg.music.rendition_scratch_mib = 4;
    let bytes = wave();
    f.state
        .media
        .put(&format!("music{}.orig", f.track), bytes.clone())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE music_assets SET original_format='wav',original_bytes=$2,duration=1 WHERE id=$1",
    )
    .bind(f.track)
    .bind(i64::try_from(bytes.len()).unwrap())
    .execute(&f.state.pool)
    .await
    .unwrap();
    let query = serde_urlencoded::to_string([
        ("u", "alice"),
        ("apiKey", f.password.as_str()),
        ("id", &f.track.to_string()),
        ("format", "mp3"),
        ("maxBitRate", "320"),
    ])
    .unwrap();
    let app = plamenu::build_router(f.state.clone());
    let uri = format!("/rest/stream.view?{query}");
    let head = app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_renditions")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    let worker_state = f.state.clone();
    let worker = tokio::spawn(async move {
        let id = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let queued: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM music_renditions WHERE state='queued' ORDER BY id LIMIT 1",
                )
                .fetch_optional(&worker_state.pool)
                .await
                .unwrap();
                if let Some(id) = queued {
                    break id;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        plamenu::music_renditions::run_one(&worker_state, id, env!("CARGO_BIN_EXE_plamenu").into())
            .await
            .unwrap()
    });
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["Content-Type"], "audio/mpeg");
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        response.into_body().collect(),
    )
    .await
    .unwrap()
    .unwrap()
    .to_bytes();
    assert!(worker.await.unwrap());
    assert!(body.starts_with(b"ID3"));
    let file = root.path().join("received.mp3");
    std::fs::write(&file, &body).unwrap();
    let decoded = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(&file)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let rate: Option<i32> = sqlx::query_scalar("SELECT actual_bitrate FROM music_renditions")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(rate, Some(192));
    assert!(
        plamenu::music_listening::history(&f.state, f.owner, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn now_playing_limits_playable_entries_after_a_large_private_prefix_and_rechecks_own_expiry(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let hash = hash_password("fixture-password").unwrap();
    for n in 0..11 {
        let account = create_local_account(&f.state.pool, &format!("playing{n}"), "Player")
            .await
            .id;
        user::create(
            &f.state.pool,
            account,
            Some(&format!("playing{n}@example.test")),
            &hash,
        )
        .await
        .unwrap();
        let key = music_clients::create(
            &f.state,
            account,
            None,
            &ClientRequest {
                label: "Opted-in player".into(),
                days: 90,
                can_manage: true,
                broadcast_playing: true,
            },
        )
        .await
        .unwrap();
        let artist = id::next();
        sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Private artist')")
            .bind(artist)
            .bind(account)
            .execute(&f.state.pool)
            .await
            .unwrap();
        let assets = (0..50).map(|_| id::next()).collect::<Vec<_>>();
        sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) SELECT id,$2,$3,'Private track','private','music'||id||'.orig','music'||id||'.mp3',1,1,lpad(to_hex(id),64,'0'),1,'mp3' FROM unnest($1::bigint[]) id").bind(&assets).bind(account).bind(artist).execute(&f.state.pool).await.unwrap();
        sqlx::query("INSERT INTO music_client_playing(account_id,credential_id,asset_id) SELECT $1,$2,id FROM unnest($3::bigint[]) id").bind(account).bind(key.key.id).bind(&assets).execute(&f.state.pool).await.unwrap();
    }
    let key = music_clients::create(
        &f.state,
        f.owner,
        None,
        &ClientRequest {
            label: "Expiring report".into(),
            days: 90,
            can_manage: true,
            broadcast_playing: true,
        },
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO music_client_playing(account_id,credential_id,asset_id,reported_at) VALUES($1,$2,$3,now()-interval '1 minute')").bind(f.owner).bind(key.key.id).bind(f.track).execute(&f.state.pool).await.unwrap();
    let app = plamenu::build_router(f.state.clone());
    let current = call(&app, "getNowPlaying", &f.password, &[]).await;
    assert_eq!(current["nowPlaying"]["entry"].as_array().unwrap().len(), 1);
    assert_eq!(
        current["nowPlaying"]["entry"][0]["title"],
        "Track & <title>"
    );
    sqlx::query("UPDATE music_client_keys SET created_at=now()-interval '2 days',expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(key.key.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        call(&app, "getNowPlaying", &f.password, &[]).await["nowPlaying"]["entry"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
