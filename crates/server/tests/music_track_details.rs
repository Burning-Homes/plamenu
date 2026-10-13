mod common;
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use plamenu::{
    AppState,
    actions::{self, EditParams},
    compose::PostFormat,
    music_assets,
    music_entities::{self, Kind},
    music_publications,
    music_track_details::{self, Album, Details},
};
use plamenu_db::{PgPool, account::Account, id};
struct Fixture {
    state: AppState,
    owner: Account,
    bob: Account,
    source: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let source = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Primary performer')")
        .bind(artist)
        .bind(owner.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format,release_year,genre) VALUES($1,$2,'Track notes',$3,'public',$4,$5,100,100,$6,10,'mp3',2026,'Ambient')").bind(source).bind(owner.id).bind(artist).bind(format!("music{source}.orig")).bind(format!("music{source}.mp3")).bind(format!("{source:064x}")).execute(&state.pool).await.unwrap();
    Fixture {
        state,
        owner,
        bob,
        source,
    }
}
fn details(asset: &music_assets::Asset, text: &str) -> Details {
    Details {
        revision: asset.revision.to_string(),
        description: text.into(),
        content_type: None,
        tags: vec![" Evening ".into(), "evening".into()],
        extra_artists: vec![" Guest Performer ".into(), "guest performer".into()],
    }
}
async fn album(f: &Fixture, owner: i64, name: &str) -> music_entities::Entity {
    let artist = music_entities::create(
        &f.state,
        owner,
        Kind::Artist,
        &music_entities::New {
            name: name.into(),
            description: String::new(),
            musicbrainz_id: None,
            tags: vec![],
            genres: vec![],
            release_year: None,
            artist_id: None,
        },
    )
    .await
    .unwrap();
    music_entities::create(
        &f.state,
        owner,
        Kind::Album,
        &music_entities::New {
            name: name.into(),
            description: "An existing owned compilation album".into(),
            musicbrainz_id: None,
            tags: vec!["Selection".into()],
            genres: vec!["Classical".into()],
            release_year: Some(2024),
            artist_id: Some(artist.id.to_string()),
        },
    )
    .await
    .unwrap()
}
fn selection(asset: &music_assets::Asset, album: Option<i64>) -> Album {
    Album {
        revision: asset.revision.to_string(),
        album_id: album.map(|id| id.to_string()),
    }
}
#[sqlx::test(migrations = "../db/migrations")]
async fn notes_credits_and_rich_caption_remain_in_sync_through_edits_and_republishing(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let original = music_assets::get(&f.state, f.source, None).await.unwrap();
    let accounts_before: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    let mut input = details(&original, "A **Markdown** track description.");
    input.content_type = Some("text/markdown".into());
    let stored = music_track_details::update(&f.state, f.owner.id, f.source, &input)
        .await
        .unwrap();
    assert_eq!(stored.tags, vec!["evening"]);
    assert_eq!(stored.extra_artists, vec!["Guest Performer"]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM accounts")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        accounts_before,
        "Additional artist names must never manufacture accounts"
    );
    assert_eq!(
        music_assets::catalogue(&f.state, None, false, "Guest Performer", None)
            .await
            .unwrap()[0]
            .id,
        f.source
    );
    let publication = music_publications::publish(&f.state, &f.owner, f.source)
        .await
        .unwrap();
    let source = plamenu_db::status::source_of(&f.state.pool, publication.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.text, input.description);
    assert_eq!(source.content_type, "text/markdown");
    assert!(publication.content.contains("<strong>Markdown</strong>"));
    let document = plamenu::note::note_for_status(&f.state, &publication, &f.owner)
        .await
        .unwrap();
    assert_eq!(document["track"]["summary"], document["content"]);
    assert_eq!(document["track"]["tag"][0]["name"], "#evening");
    let edited = actions::edit_status(
        &f.state,
        &f.owner,
        publication.id,
        EditParams {
            text: Some(
                "<p>New caption <strong>keeps formatting</strong></p><script>alert(1)</script>",
            ),
            content_type: Some(PostFormat::Html),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!edited.content.contains("<script"));
    let source = plamenu_db::status::source_of(&f.state.pool, publication.id)
        .await
        .unwrap()
        .unwrap();
    let stored = music_assets::get(&f.state, f.source, None).await.unwrap();
    assert_eq!(stored.description, source.text);
    assert_eq!(stored.description_type, "text/html");
    assert!(stored.revision > original.revision);
    let metadata = music_assets::Edit {
        title: "New title only".into(),
        visibility: "public".into(),
        music: stored.metadata(),
        revision: Some(stored.revision.to_string()),
    };
    music_assets::update(&f.state, f.owner.id, f.source, &metadata)
        .await
        .unwrap();
    let after = plamenu_db::status::source_of(&f.state.pool, publication.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.text, source.text);
    assert_eq!(after.content_type, source.content_type);
    let stored = music_assets::get(&f.state, f.source, None).await.unwrap();
    let mut same = details(&stored, &stored.description);
    same.tags = stored.tags.clone();
    same.extra_artists = stored.extra_artists.clone();
    assert_eq!(
        music_track_details::update(&f.state, f.owner.id, f.source, &same)
            .await
            .unwrap()
            .revision,
        stored.revision
    );
    music_publications::withdraw(&f.state, &f.owner, f.source)
        .await
        .unwrap();
    let again = music_publications::publish(&f.state, &f.owner, f.source)
        .await
        .unwrap();
    assert_ne!(again.id, publication.id);
    assert_eq!(again.content, edited.content);
    assert_eq!(
        plamenu_db::status::source_of(&f.state.pool, again.id)
            .await
            .unwrap()
            .unwrap()
            .content_type,
        "text/html"
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn exact_album_assignment_keeps_performer_overrides_and_current_compilation_access(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let own = album(&f, f.owner.id, "Compilation artist").await;
    let foreign = album(&f, f.bob.id, "Other owner's album").await;
    let before = music_assets::get(&f.state, f.source, None).await.unwrap();
    assert!(
        music_track_details::set_album(
            &f.state,
            f.owner.id,
            f.source,
            &selection(&before, Some(foreign.id))
        )
        .await
        .is_err()
    );
    let updated = music_track_details::set_album(
        &f.state,
        f.owner.id,
        f.source,
        &selection(&before, Some(own.id)),
    )
    .await
    .unwrap();
    assert_eq!(updated.album_id, Some(own.id));
    assert_eq!(updated.artist_id, before.artist_id);
    assert_eq!(updated.release_year, Some(2026));
    assert_eq!(updated.genre, "Ambient");
    assert_eq!(
        music_entities::statistics(&f.state, Kind::Artist, own.artist_id.unwrap(), None)
            .await
            .unwrap()
            .track_count,
        1
    );
    assert_eq!(
        music_assets::catalogue(&f.state, None, false, "Compilation artist", None)
            .await
            .unwrap()[0]
            .id,
        f.source
    );
    assert!(
        music_track_details::set_album(&f.state, f.owner.id, f.source, &selection(&before, None))
            .await
            .is_err()
    );
    let publication = music_publications::publish(&f.state, &f.owner, f.source)
        .await
        .unwrap();
    let document = plamenu::note::note_for_status(&f.state, &publication, &f.owner)
        .await
        .unwrap();
    assert_eq!(
        document["track"]["artists"][0]["id"],
        music_entities::uri(TEST_DOMAIN, Kind::Artist, before.artist_id)
    );
    assert_eq!(
        document["track"]["album"]["artists"][0]["id"],
        music_entities::uri(TEST_DOMAIN, Kind::Artist, own.artist_id.unwrap())
    );
    let mut metadata = updated.metadata();
    metadata.artist = "A corrected primary performer".into();
    let updated = music_assets::update(
        &f.state,
        f.owner.id,
        f.source,
        &music_assets::Edit {
            title: updated.title.clone(),
            visibility: "public".into(),
            music: metadata,
            revision: Some(updated.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        updated.album_id,
        Some(own.id),
        "Correcting the performer cannot recreate the compilation album"
    );
    let private = music_assets::update(
        &f.state,
        f.owner.id,
        f.source,
        &music_assets::Edit {
            title: updated.title.clone(),
            visibility: "private".into(),
            music: updated.metadata(),
            revision: Some(updated.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert!(
        music_entities::get(&f.state, Kind::Artist, own.artist_id.unwrap(), None)
            .await
            .is_err()
    );
    music_assets::grant(&f.state, f.owner.id, f.source, f.bob.id, true)
        .await
        .unwrap();
    assert!(
        music_entities::get(
            &f.state,
            Kind::Artist,
            own.artist_id.unwrap(),
            Some(f.bob.id)
        )
        .await
        .is_ok()
    );
    music_assets::grant(&f.state, f.owner.id, f.source, f.bob.id, false)
        .await
        .unwrap();
    assert!(
        music_entities::get(
            &f.state,
            Kind::Artist,
            own.artist_id.unwrap(),
            Some(f.bob.id)
        )
        .await
        .is_err()
    );
    let detached =
        music_track_details::set_album(&f.state, f.owner.id, f.source, &selection(&private, None))
            .await
            .unwrap();
    assert_eq!(detached.album_id, None);
    assert_eq!(detached.genre, "Ambient");
}
#[sqlx::test(migrations = "../db/migrations")]
async fn details_and_album_assignment_roll_back_on_outbox_failure(pool: PgPool) {
    let f = fixture(pool).await;
    let own = album(&f, f.owner.id, "A new album").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let follower = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&f.state.pool, follower.id, f.owner.id, None)
        .await
        .unwrap();
    let publication = music_publications::publish(&f.state, &f.owner, f.source)
        .await
        .unwrap();
    let before = music_assets::get(&f.state, f.source, None).await.unwrap();
    let jobs = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("CREATE FUNCTION reject_track_details() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced track details outbox failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_track_details BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_track_details()").execute(&f.state.pool).await.unwrap();
    assert!(
        music_track_details::update(
            &f.state,
            f.owner.id,
            f.source,
            &details(&before, "Must roll back")
        )
        .await
        .is_err()
    );
    assert!(
        music_track_details::set_album(
            &f.state,
            f.owner.id,
            f.source,
            &selection(&before, Some(own.id))
        )
        .await
        .is_err()
    );
    assert!(
        actions::edit_status(
            &f.state,
            &f.owner,
            publication.id,
            EditParams {
                text: Some("Reverse caption must roll back"),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let after = music_assets::get(&f.state, f.source, None).await.unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.description, before.description);
    assert_eq!(after.album_id, None);
    assert!(after.tags.is_empty());
    assert_eq!(
        plamenu_db::status::source_of(&f.state.pool, publication.id)
            .await
            .unwrap()
            .unwrap()
            .text,
        ""
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_jobs")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        jobs
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn stale_foreign_and_unbounded_details_are_rejected_without_mutating_sources(pool: PgPool) {
    let f = fixture(pool).await;
    let before = music_assets::get(&f.state, f.source, None).await.unwrap();
    let mut input = details(&before, "A new note");
    assert!(
        music_track_details::update(&f.state, f.bob.id, f.source, &input)
            .await
            .is_err()
    );
    input.extra_artists = vec!["Too many credits".into(); 21];
    assert!(
        music_track_details::update(&f.state, f.owner.id, f.source, &input)
            .await
            .is_err()
    );
    input.extra_artists = vec!["Hidden\ncontrol".into()];
    assert!(
        music_track_details::update(&f.state, f.owner.id, f.source, &input)
            .await
            .is_err()
    );
    input = details(&before, &"a".repeat(50_001));
    assert!(
        music_track_details::update(&f.state, f.owner.id, f.source, &input)
            .await
            .is_err()
    );
    input = details(&before, "A new note");
    input.content_type = Some("application/javascript".into());
    assert!(
        music_track_details::update(&f.state, f.owner.id, f.source, &input)
            .await
            .is_err()
    );
    assert_eq!(
        music_assets::get(&f.state, f.source, None)
            .await
            .unwrap()
            .revision,
        before.revision
    );
    input = details(&before, "A new note");
    music_track_details::update(&f.state, f.owner.id, f.source, &input)
        .await
        .unwrap();
    assert!(
        music_track_details::update(&f.state, f.owner.id, f.source, &input)
            .await
            .is_err()
    );
    assert_eq!(
        music_assets::get(&f.state, f.source, None)
            .await
            .unwrap()
            .description,
        "A new note"
    );
}
