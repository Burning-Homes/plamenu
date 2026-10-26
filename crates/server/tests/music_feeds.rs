mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    AppState, music_collections,
    music_feeds::{self, Format},
};
use plamenu_db::{PgPool, id};
use tower::ServiceExt;
async fn fixture(pool: PgPool) -> (AppState, i64, i64, music_collections::Collection) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let parent = music_collections::create(
        &state,
        owner,
        &music_collections::New {
            name: "Show <&> library".into(),
            description: "Feed notes".into(),
            kind: "playlist".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    (state, owner, viewer, parent)
}
async fn track(state: &AppState, owner: i64, parent: i64, title: &str, visibility: &str) -> i64 {
    let artist = id::next();
    let asset = id::next();
    let artist: i64=sqlx::query_scalar("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist & friends') ON CONFLICT(owner_id,name) DO UPDATE SET name=EXCLUDED.name RETURNING id").bind(artist).bind(owner).fetch_one(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format,description,description_type) VALUES($1,$2,$3,$4,$5,$6,$7,100,100,$8,10,'mp3','<script>bad()</script><p>Current notes</p>','text/html')").bind(asset).bind(owner).bind(title).bind(artist).bind("public").bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    let parent = music_collections::get(state, parent, Some(owner))
        .await
        .unwrap();
    music_collections::add(
        state,
        owner,
        parent.id,
        &music_collections::Add {
            remote_id: None,
            revision: parent.revision.to_string(),
            asset_id: Some(asset.to_string()),
            status_id: None,
            episode_id: None,
        },
    )
    .await
    .unwrap();
    if visibility != "public" {
        sqlx::query("UPDATE music_assets SET visibility=$2,revision=revision+1 WHERE id=$1")
            .bind(asset)
            .bind(visibility)
            .execute(&state.pool)
            .await
            .unwrap();
    }
    asset
}
async fn request(app: &axum::Router, path: &str, etag: Option<&str>) -> Response {
    let mut request = Request::builder().uri(path);
    if let Some(etag) = etag {
        request = request.header(header::IF_NONE_MATCH, etag);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
async fn bytes(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn collection_syndication_filters_before_limit_and_round_trips_both_feed_readers(
    pool: PgPool,
) {
    let (state, owner, _, parent) = fixture(pool).await;
    let public = track(&state, owner, parent.id, "Public <&> title", "public").await;
    track(&state, owner, parent.id, "Private hidden title", "private").await;
    for format in [Format::Rss, Format::Atom] {
        let rendered = music_feeds::collection(&state, parent.id, None, 1)
            .await
            .unwrap()
            .render(format, "https://plamenu.test/music/feed");
        assert!(!rendered.contains("Private hidden title"));
        assert!(!rendered.contains("<script"));
        assert!(!rendered.contains("&lt;script"));
        let parsed = plamenu::music_podcasts::feed::parse(
            rendered.as_bytes(),
            "https://plamenu.test/music/feed",
        )
        .unwrap();
        assert_eq!(parsed.title, "Show <&> library");
        assert_eq!(parsed.episodes.len(), 1);
        assert_eq!(parsed.episodes[0].title, "Public <&> title");
        assert!(
            parsed.episodes[0]
                .audio_url
                .ends_with(&format!("/music/assets/{public}/stream"))
        );
        assert_eq!(parsed.episodes[0].audio_type, "audio/mpeg");
        assert_eq!(parsed.episodes[0].audio_length, Some(100));
    }
    let owned = music_feeds::collection(&state, parent.id, Some(owner), 1)
        .await
        .unwrap()
        .render(Format::Rss, "https://plamenu.test/music/feed");
    assert!(owned.contains("Private hidden title"));
    assert!(
        music_feeds::collection(&state, parent.id, None, 101)
            .await
            .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn current_permission_and_source_changes_precede_feed_conditional_not_modified(pool: PgPool) {
    let (state, owner, _, parent) = fixture(pool).await;
    let asset = track(&state, owner, parent.id, "First title", "public").await;
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/feeds/collections/{}.rss", parent.id);
    let first = request(&app, &path, None).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()[header::VARY], "Authorization, Cookie");
    let etag = first.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(
        request(&app, &path, Some(&etag)).await.status(),
        StatusCode::NOT_MODIFIED
    );
    sqlx::query("UPDATE music_assets SET title='Changed title',revision=revision+1 WHERE id=$1")
        .bind(asset)
        .execute(&state.pool)
        .await
        .unwrap();
    let changed = request(&app, &path, Some(&etag)).await;
    assert_eq!(changed.status(), StatusCode::OK);
    assert_ne!(changed.headers()[header::ETAG], etag);
    assert!(
        String::from_utf8(bytes(changed).await)
            .unwrap()
            .contains("Changed title")
    );
    sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(asset)
        .execute(&state.pool)
        .await
        .unwrap();
    let hidden = request(&app, &path, Some(&etag)).await;
    assert_eq!(hidden.status(), StatusCode::OK);
    assert!(
        !String::from_utf8(bytes(hidden).await)
            .unwrap()
            .contains("Changed title")
    );
    sqlx::query(
        "UPDATE music_collections SET visibility='private',revision=revision+1 WHERE id=$1",
    )
    .bind(parent.id)
    .execute(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        request(&app, &path, Some("*")).await.status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, &path, None).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn podcast_collection_feed_preserves_typed_enclosures_and_repeat_identity_without_listen_events(
    pool: PgPool,
) {
    let (state, owner, _, parent) = fixture(pool).await;
    let show = id::next();
    let episode = id::next();
    sqlx::query("INSERT INTO music_podcasts(id,feed_url,title) VALUES($1,'https://podcast.example/feed','Podcast show')").bind(show).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_podcast_episodes(id,podcast_id,guid,title,audio_url,audio_type,audio_length) VALUES($1,$2,'episode-one','Episode <&> one','https://podcast.example/episode.ogg','audio/ogg',123)").bind(episode).bind(show).execute(&state.pool).await.unwrap();
    let current = music_collections::get(&state, parent.id, Some(owner))
        .await
        .unwrap();
    music_collections::add_podcast_episodes(
        &state,
        owner,
        parent.id,
        current.revision,
        show,
        &[episode, episode],
    )
    .await
    .unwrap();
    for format in [Format::Rss, Format::Atom] {
        let xml = music_feeds::collection(&state, parent.id, None, 50)
            .await
            .unwrap()
            .render(format, "https://plamenu.test/feed");
        let parsed =
            plamenu::music_podcasts::feed::parse(xml.as_bytes(), "https://plamenu.test/feed")
                .unwrap();
        assert_eq!(parsed.episodes.len(), 2);
        assert_ne!(parsed.episodes[0].guid, parsed.episodes[1].guid);
        for item in parsed.episodes {
            assert_eq!(item.title, "Episode <&> one");
            assert_eq!(item.audio_type, "audio/ogg");
            assert_eq!(item.audio_length, Some(123));
            assert!(
                item.audio_url
                    .ends_with(&format!("/music/podcasts/episodes/{episode}/audio"))
            );
        }
    }
    let listens: i64 = sqlx::query_scalar("SELECT count(*) FROM music_listen_sessions")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let played: i64 = sqlx::query_scalar("SELECT count(*) FROM music_podcast_plays")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(listens, 0);
    assert_eq!(played, 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn profile_feeds_use_native_post_privacy_and_actual_audio_mime_and_hide_retired_profiles(
    pool: PgPool,
) {
    use plamenu::{
        actions::{self, PostParams},
        music_feeds,
    };
    use plamenu_ap::activity::PostKind;
    use plamenu_db::media::{self, NewLocalMedia};
    let (state, owner, viewer, _) = fixture(pool).await;
    let mut input = NewLocalMedia::new(owner, id::next(), "episode.ogg", "audio/ogg");
    input.kind = "audio";
    input.duration = Some(9.0);
    let media = media::create_local(&state.pool, input).await.unwrap();
    media::set_file_sizes(&state.pool, media.id, 123, None)
        .await
        .unwrap();
    actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "Public audio notes",
            title: Some("Profile audio"),
            visibility: "public",
            media_ids: &[media.id],
            kind: PostKind::Audio,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "Secret profile text",
            visibility: "private",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for format in [Format::Rss, Format::Atom] {
        let feed = music_feeds::profile(&state, "alice", None, 50)
            .await
            .unwrap()
            .render(format, "https://plamenu.test/feed");
        assert!(!feed.contains("Secret profile text"));
        let parsed =
            plamenu::music_podcasts::feed::parse(feed.as_bytes(), "https://plamenu.test/feed")
                .unwrap();
        assert_eq!(parsed.episodes.len(), 1);
        assert_eq!(parsed.episodes[0].title, "Profile audio");
        assert_eq!(parsed.episodes[0].audio_type, "audio/ogg");
        assert_eq!(parsed.episodes[0].audio_length, Some(123));
    }
    let own = music_feeds::profile(&state, "alice", Some(owner), 50)
        .await
        .unwrap()
        .render(Format::Rss, "https://plamenu.test/feed");
    assert!(own.contains("Secret profile text"));
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        music_feeds::profile(&state, "alice", Some(viewer), 50)
            .await
            .is_err()
    );
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_feeds::profile(&state, "alice", None, 50)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn artist_release_feed_keeps_standalone_tracks_when_album_is_hidden_and_merges_latest_visible_releases(
    pool: PgPool,
) {
    let (state, owner, _, parent) = fixture(pool).await;
    let exposed = track(
        &state,
        owner,
        parent.id,
        "Readable under hidden album",
        "public",
    )
    .await;
    let artist: i64 = sqlx::query_scalar("SELECT artist_id FROM music_assets WHERE id=$1")
        .bind(exposed)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let hidden = id::next();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,visibility) VALUES($1,$2,$3,'Hidden album','private')").bind(hidden).bind(owner).bind(artist).execute(&state.pool).await.unwrap();
    sqlx::query("UPDATE music_assets SET album_id=$2,revision=revision+1 WHERE id=$1")
        .bind(exposed)
        .bind(hidden)
        .execute(&state.pool)
        .await
        .unwrap();
    let anonymous = music_feeds::artist(&state, artist, None, 50)
        .await
        .unwrap()
        .render(Format::Rss, "https://plamenu.test/feed");
    assert!(anonymous.contains("Readable under hidden album"));
    assert!(!anonymous.contains("Hidden album"));
    let owned = music_feeds::artist(&state, artist, Some(owner), 50)
        .await
        .unwrap()
        .render(Format::Rss, "https://plamenu.test/feed");
    assert!(owned.contains("Hidden album"));
    assert!(!owned.contains("Readable under hidden album"));
    let visible = id::next();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,visibility) VALUES($1,$2,$3,'Visible album','public')").bind(visible).bind(owner).bind(artist).execute(&state.pool).await.unwrap();
    let nested = track(&state, owner, parent.id, "Represented by album", "public").await;
    sqlx::query("UPDATE music_assets SET album_id=$2,revision=revision+1 WHERE id=$1")
        .bind(nested)
        .bind(visible)
        .execute(&state.pool)
        .await
        .unwrap();
    let newest = track(&state, owner, parent.id, "Newest standalone", "public").await;
    for format in [Format::Rss, Format::Atom] {
        let feed = music_feeds::artist(&state, artist, None, 50)
            .await
            .unwrap()
            .render(format, "https://plamenu.test/feed");
        assert!(feed.contains("Visible album"));
        assert!(feed.contains("Newest standalone"));
        assert!(!feed.contains("Represented by album"));
        let one = music_feeds::artist(&state, artist, None, 1)
            .await
            .unwrap()
            .render(format, "https://plamenu.test/feed");
        assert!(one.contains("Newest standalone"));
        assert!(!one.contains("Visible album"));
    }
    // Private newer records never consume an anonymous feed's limit.
    for index in 0..52 {
        sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,visibility) VALUES($1,$2,$3,$4,'private')").bind(id::next()).bind(owner).bind(artist).bind(format!("Hidden noise {index}")).execute(&state.pool).await.unwrap();
    }
    let feed = music_feeds::artist(&state, artist, None, 1)
        .await
        .unwrap()
        .render(Format::Rss, "https://plamenu.test/feed");
    assert!(feed.contains("Newest standalone"));
    assert!(feed.contains(&format!("/music/assets/{newest}/stream")));
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(music_feeds::artist(&state, artist, None, 50).await.is_err());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn term_feeds_merge_exact_visible_entities_and_collections_before_limits(pool: PgPool) {
    use plamenu::music_feeds::Term;
    let (state, owner, _, parent) = fixture(pool).await;
    let public = track(&state, owner, parent.id, "Ambient public track", "public").await;
    let private = track(&state, owner, parent.id, "Ambient private track", "private").await;
    for asset in [public, private] {
        sqlx::query("UPDATE music_assets SET genre='Ambient; Drone',tags=ARRAY['ambient'],revision=revision+1 WHERE id=$1").bind(asset).execute(&state.pool).await.unwrap();
    }
    let artist: i64 = sqlx::query_scalar("SELECT artist_id FROM music_assets WHERE id=$1")
        .bind(public)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE music_artists SET tags=ARRAY['ambient'],revision=revision+1 WHERE id=$1")
        .bind(artist)
        .execute(&state.pool)
        .await
        .unwrap();
    let album = id::next();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,visibility,tags,genres) VALUES($1,$2,$3,'Ambient visible album','public',ARRAY['ambient'],ARRAY['ambient'])").bind(album).bind(owner).bind(artist).execute(&state.pool).await.unwrap();
    let parent = music_collections::get(&state, parent.id, Some(owner))
        .await
        .unwrap();
    music_collections::tags(
        &state,
        owner,
        parent.id,
        &music_collections::Tags {
            revision: parent.revision.to_string(),
            tags: vec!["ambient".into()],
        },
        music_collections::TagAction::Replace,
    )
    .await
    .unwrap();
    for kind in [Term::Tag, Term::Genre] {
        let feed = music_feeds::term(&state, kind, "AMBIENT", None, 100)
            .await
            .unwrap();
        assert!(
            feed.items
                .iter()
                .any(|item| item.title.contains("Ambient public track"))
        );
        assert!(
            feed.items
                .iter()
                .any(|item| item.title.contains("Ambient visible album"))
        );
        assert!(
            !feed
                .items
                .iter()
                .any(|item| item.title.contains("Ambient private track"))
        );
        let personal = music_feeds::term(&state, kind, "ambient", Some(owner), 100)
            .await
            .unwrap();
        assert!(
            personal
                .items
                .iter()
                .any(|item| item.title.contains("Ambient private track"))
        );
        assert!(
            music_feeds::term(&state, kind, "ambi", None, 100)
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }
    let tagged = music_feeds::term(&state, Term::Tag, "#ambient", None, 100)
        .await
        .unwrap();
    assert!(
        tagged
            .items
            .iter()
            .any(|item| item.link.contains("/music/artists/"))
    );
    assert!(
        tagged
            .items
            .iter()
            .any(|item| item.link.contains("/music/collections/"))
    );
    let genre = music_feeds::term(&state, Term::Genre, "drone", None, 100)
        .await
        .unwrap();
    assert_eq!(genre.items.len(), 1);
    assert!(genre.items[0].title.contains("Ambient public track"));
    // Newer private records cannot starve a public feed.
    for index in 0..52 {
        sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,visibility,genres) VALUES($1,$2,$3,$4,'private',ARRAY['ambient'])").bind(id::next()).bind(owner).bind(artist).bind(format!("Private noise {index}")).execute(&state.pool).await.unwrap();
    }
    let latest = music_feeds::term(&state, Term::Genre, "ambient", None, 1)
        .await
        .unwrap();
    assert_eq!(latest.items.len(), 1);
    assert!(latest.items[0].title.contains("Ambient visible album"));
    sqlx::query(
        "UPDATE music_collections SET visibility='private',revision=revision+1 WHERE id=$1",
    )
    .bind(parent.id)
    .execute(&state.pool)
    .await
    .unwrap();
    assert!(
        !music_feeds::term(&state, Term::Tag, "ambient", None, 100)
            .await
            .unwrap()
            .items
            .iter()
            .any(|item| item.link.contains("/music/collections/"))
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn music_term_browse_pages_link_to_current_sources_and_advertise_both_feeds(pool: PgPool) {
    let (state, owner, _, parent) = fixture(pool).await;
    let asset = track(&state, owner, parent.id, "Genre page track", "public").await;
    sqlx::query("UPDATE music_assets SET genre='ambient',tags=ARRAY['ambient'],revision=revision+1 WHERE id=$1").bind(asset).execute(&state.pool).await.unwrap();
    let app = plamenu::build_router(state.clone());
    for path in ["/music/tags/ambient", "/music/genres/ambient"] {
        let response = request(&app, path, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(bytes(response).await).unwrap();
        assert!(html.contains("Genre page track"));
        assert!(html.contains("music-primary"));
        assert!(
            !html.contains("Advanced music tools"),
            "anonymous pages do not offer signed-in tools"
        );
        assert!(html.contains("href=\"/music\""));
        assert!(html.contains("href=\"/music/streams\""));
        assert!(html.contains("rel=\"alternate\" type=\"application/rss+xml\""));
        assert!(html.contains("rel=\"alternate\" type=\"application/atom+xml\""));
    }
    let name = "post rock + дрон/ambient";
    sqlx::query("UPDATE music_assets SET genre=$2,tags=ARRAY[$2],revision=revision+1 WHERE id=$1")
        .bind(asset)
        .bind(name)
        .execute(&state.pool)
        .await
        .unwrap();
    for kind in ["genres", "tags"] {
        let encoded = music_feeds::term_segment(name);
        assert!(encoded.contains("%20"));
        assert!(!encoded.contains('+'));
        let response = request(&app, &format!("/music/{kind}/{encoded}"), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(bytes(response).await).unwrap();
        assert!(html.contains("Genre page track"));
        assert!(html.contains(&format!("/music/feeds/{kind}/{encoded}.rss")));
        let feed = request(&app, &format!("/music/feeds/{kind}/{encoded}.rss"), None).await;
        assert_eq!(feed.status(), StatusCode::OK);
        assert!(
            String::from_utf8(bytes(feed).await)
                .unwrap()
                .contains("Genre page track")
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn tagged_posts_merge_with_native_music_and_disappear_after_block_or_delete(pool: PgPool) {
    use plamenu::actions::{self, PostParams};
    let (state, owner, viewer, parent) = fixture(pool).await;
    let asset = track(&state, owner, parent.id, "Tagged track", "public").await;
    sqlx::query("UPDATE music_assets SET tags=ARRAY['ambient'] WHERE id=$1")
        .bind(asset)
        .execute(&state.pool)
        .await
        .unwrap();
    let posted = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "Public #ambient listening notes",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "Secret #ambient listening notes",
            visibility: "private",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let xml = music_feeds::term(&state, music_feeds::Term::Tag, "ambient", Some(viewer), 100)
        .await
        .unwrap()
        .render(Format::Atom, "https://plamenu.test/feed");
    assert!(xml.contains("Public"));
    assert!(xml.contains("Tagged track"));
    assert!(!xml.contains("Secret"));
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        music_feeds::term(&state, music_feeds::Term::Tag, "ambient", Some(viewer), 100)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let status_id = posted.0.id;
    sqlx::query("UPDATE statuses SET deleted_at=now() WHERE id=$1")
        .bind(status_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let xml = music_feeds::term(&state, music_feeds::Term::Tag, "ambient", None, 100)
        .await
        .unwrap()
        .render(Format::Atom, "https://plamenu.test/feed");
    assert!(!xml.contains("listening notes"));
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn large_episode_notes_remain_bounded_valid_feeds_and_metadata_edits_change_etags(
    pool: PgPool,
) {
    let (state, owner, _, parent) = fixture(pool).await;
    let show = id::next();
    let episode = id::next();
    sqlx::query("INSERT INTO music_podcasts(id,feed_url,title) VALUES($1,'https://podcast.example/large','Large show')")
        .bind(show).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_podcast_episodes(id,podcast_id,guid,title,description,audio_url,audio_type) VALUES($1,$2,'large','Large episode',$3,'https://podcast.example/large.mp3','audio/mpeg')")
        .bind(episode).bind(show).bind(format!("<p>{}</p>", "🎵 &amp; notes ".repeat(20000)))
        .execute(&state.pool).await.unwrap();
    for _ in 0..2 {
        let current = music_collections::get(&state, parent.id, Some(owner))
            .await
            .unwrap();
        music_collections::add_podcast_episodes(
            &state,
            owner,
            parent.id,
            current.revision,
            show,
            &[episode; 50],
        )
        .await
        .unwrap();
    }
    for format in [Format::Rss, Format::Atom] {
        let xml = music_feeds::collection(&state, parent.id, None, 100)
            .await
            .unwrap()
            .render(format, "https://plamenu.test/feed");
        assert!(xml.len() < 2 * 1024 * 1024);
        assert_eq!(
            plamenu::music_podcasts::feed::parse(xml.as_bytes(), "https://plamenu.test/feed")
                .unwrap()
                .episodes
                .len(),
            100
        );
    }
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/feeds/collections/{}.atom", parent.id);
    let old = request(&app, &path, None).await;
    let etag = old.headers()[header::ETAG].to_str().unwrap().to_owned();
    let before: time::OffsetDateTime =
        sqlx::query_scalar("SELECT updated_at FROM music_podcast_episodes WHERE id=$1")
            .bind(episode)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    sqlx::query("UPDATE music_podcast_episodes SET description='<p>New notes</p>' WHERE id=$1")
        .bind(episode)
        .execute(&state.pool)
        .await
        .unwrap();
    let after: time::OffsetDateTime =
        sqlx::query_scalar("SELECT updated_at FROM music_podcast_episodes WHERE id=$1")
            .bind(episode)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(after > before);
    let changed = request(&app, &path, Some(&etag)).await;
    assert_eq!(changed.status(), StatusCode::OK);
    assert_ne!(changed.headers()[header::ETAG].to_str().unwrap(), etag);
    assert!(
        String::from_utf8(bytes(changed).await)
            .unwrap()
            .contains("New notes")
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn collection_share_feeds_preserve_exact_child_scope_and_recheck_before_304(pool: PgPool) {
    use plamenu::{
        music_saved::{Kind, Reference},
        music_shares::{self, New},
    };
    let (state, owner, _, parent) = fixture(pool).await;
    track(&state, owner, parent.id, "Shared owned track", "private").await;
    let other_owner = create_local_account(&state.pool, "carol", "Carol").await.id;
    let foreign_parent = music_collections::create(
        &state,
        other_owner,
        &music_collections::New {
            name: "Foreign library".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    let foreign = track(
        &state,
        other_owner,
        foreign_parent.id,
        "Foreign private track",
        "private",
    )
    .await;
    sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) VALUES($1,$2,$3,2)")
        .bind(id::next()).bind(parent.id).bind(foreign).execute(&state.pool).await.unwrap();
    let sibling = music_collections::create(
        &state,
        owner,
        &music_collections::New {
            name: "Private sibling".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    track(
        &state,
        owner,
        sibling.id,
        "Unrelated private track",
        "private",
    )
    .await;
    sqlx::query("UPDATE music_collections SET visibility='private',share_tracks=true,revision=revision+1 WHERE id=$1")
        .bind(parent.id).execute(&state.pool).await.unwrap();
    let link = music_shares::create(
        &state,
        owner,
        &New {
            reference: Reference {
                kind: Kind::Playlist,
                id: parent.id.to_string(),
            },
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state.clone());
    let path = format!(
        "/music/feeds/collections/{}.rss?token={}",
        parent.id, link.token
    );
    let response = request(&app, &path, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    let xml = String::from_utf8(bytes(response).await).unwrap();
    assert!(xml.contains("Shared owned track"));
    assert!(xml.contains("Current notes"));
    assert!(xml.contains("/music/share/assets/"));
    assert!(!xml.contains("Foreign private track"));
    assert!(!xml.contains("Unrelated private track"));
    assert_eq!(
        request(
            &app,
            &format!(
                "/music/feeds/collections/{}.rss?token={}",
                sibling.id, link.token
            ),
            None
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&app, &path, Some(&etag)).await.status(),
        StatusCode::NOT_MODIFIED
    );
    sqlx::query("UPDATE music_collections SET share_tracks=false,revision=revision+1 WHERE id=$1")
        .bind(parent.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let no_children = request(&app, &path, Some(&etag)).await;
    assert_eq!(no_children.status(), StatusCode::OK);
    assert!(
        !String::from_utf8(bytes(no_children).await)
            .unwrap()
            .contains("Shared owned track")
    );
    music_shares::revoke(&state, owner, link.id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(
        request(&app, &path, Some(&etag)).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn term_browse_keysets_reach_every_source_and_recheck_visibility(pool: PgPool) {
    use music_feeds::Term;
    use std::collections::HashSet;
    let (state, owner, _, parent) = fixture(pool).await;
    let template = track(&state, owner, parent.id, "Paged recording", "public").await;
    sqlx::query(
        "UPDATE music_assets SET genre='paged',tags=ARRAY['paged'],revision=revision+1 WHERE id=$1",
    )
    .bind(template)
    .execute(&state.pool)
    .await
    .unwrap();
    let mut expected = HashSet::from([format!("https://plamenu.test/music/assets/{template}")]);
    for index in 0..124 {
        let asset = id::next();
        sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,genre,tags,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) SELECT $1,owner_id,$2,artist_id,'paged',ARRAY['paged'],'public',$3,$4,100,100,$5,10,'mp3' FROM music_assets WHERE id=$6")
            .bind(asset).bind(format!("Paged {index}")).bind(format!("music{asset}.orig"))
            .bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).bind(template)
            .execute(&state.pool).await.unwrap();
        expected.insert(format!("https://plamenu.test/music/assets/{asset}"));
    }
    // Tables can contain the same numeric key: both typed sources must survive.
    sqlx::query("UPDATE music_collections SET tags=ARRAY['paged'],revision=revision+1 WHERE id=$1")
        .bind(parent.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let artist: i64 = sqlx::query_scalar("SELECT artist_id FROM music_assets WHERE id=$1")
        .bind(template)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,visibility,genres,tags) VALUES($1,$2,$3,'Paged album','public',ARRAY['paged'],ARRAY['paged'])")
        .bind(parent.id).bind(owner).bind(artist).execute(&state.pool).await.unwrap();
    for kind in [Term::Tag, Term::Genre] {
        let mut wanted = expected.clone();
        wanted.insert(format!("https://plamenu.test/music/albums/{}", parent.id));
        if matches!(kind, Term::Tag) {
            wanted.insert(format!(
                "https://plamenu.test/music/collections/{}",
                parent.id
            ));
        }
        let mut seen = HashSet::new();
        let mut before = None;
        let mut pages = 0;
        loop {
            let page = music_feeds::term_page(&state, kind, "paged", None, before.as_deref())
                .await
                .unwrap();
            assert!(page.items.len() <= 60);
            for item in page.items {
                assert!(seen.insert(item.link), "duplicate source across pages");
            }
            pages += 1;
            before = page.before;
            if before.is_none() {
                break;
            }
            assert!(pages < 10);
        }
        assert_eq!(pages, 3);
        assert_eq!(seen, wanted);
    }
    let first = music_feeds::term_page(&state, Term::Tag, "paged", None, None)
        .await
        .unwrap();
    sqlx::query("UPDATE music_assets SET visibility='private',revision=revision+1 WHERE id=$1")
        .bind(template)
        .execute(&state.pool)
        .await
        .unwrap();
    let mut before = first.before;
    while let Some(cursor) = before {
        let page = music_feeds::term_page(&state, Term::Tag, "paged", None, Some(&cursor))
            .await
            .unwrap();
        assert!(
            page.items
                .iter()
                .all(|item| !item.link.ends_with(&template.to_string()))
        );
        before = page.before;
    }
    assert!(
        music_feeds::term_page(&state, Term::Tag, "paged", None, Some("-1:0"))
            .await
            .is_err()
    );
    assert!(
        music_feeds::term_page(&state, Term::Tag, "paged", None, Some("1:5"))
            .await
            .is_err()
    );
    let app = plamenu::build_router(state);
    let response = request(&app, "/music/tags/paged", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(bytes(response).await).unwrap();
    assert!(html.contains("rel=\"next\""));
    assert!(html.contains("/music/tags/paged?before="));
    let invalid = request(&app, "/music/tags/paged?before=bad", None).await;
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
