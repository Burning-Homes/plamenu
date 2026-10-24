mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState,
    music_collections::{self, New, TagAction, Tags},
};
use plamenu_db::PgPool;
async fn fixture(pool: PgPool) -> (AppState, i64, i64, music_collections::Collection) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let collection = music_collections::create(
        &state,
        owner,
        &New {
            name: "Tags library".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    (state, owner, viewer, collection)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn collection_tags_normalize_merge_remove_and_keep_repeated_changes_idempotent(pool: PgPool) {
    let (state, owner, _, mut current) = fixture(pool).await;
    current = music_collections::tags(
        &state,
        owner,
        current.id,
        &Tags {
            revision: current.revision.to_string(),
            tags: vec![" #Rock ".into(), "rock".into(), "Ambient".into()],
        },
        TagAction::Replace,
    )
    .await
    .unwrap();
    assert_eq!(current.tags, vec!["rock", "ambient"]);
    let repeated = music_collections::tags(
        &state,
        owner,
        current.id,
        &Tags {
            revision: current.revision.to_string(),
            tags: vec!["ROCK".into()],
        },
        TagAction::Add,
    )
    .await
    .unwrap();
    assert_eq!(repeated.revision, current.revision);
    current = music_collections::tags(
        &state,
        owner,
        current.id,
        &Tags {
            revision: current.revision.to_string(),
            tags: Vec::new(),
        },
        TagAction::Remove("#ROCK".into()),
    )
    .await
    .unwrap();
    assert_eq!(current.tags, vec!["ambient"]);
    let tags = (0..50)
        .map(|index| format!("genre_{index}"))
        .collect::<Vec<_>>();
    current = music_collections::tags(
        &state,
        owner,
        current.id,
        &Tags {
            revision: current.revision.to_string(),
            tags: tags.clone(),
        },
        TagAction::Replace,
    )
    .await
    .unwrap();
    let repeated = music_collections::tags(
        &state,
        owner,
        current.id,
        &Tags {
            revision: current.revision.to_string(),
            tags: tags.clone(),
        },
        TagAction::Add,
    )
    .await
    .unwrap();
    assert_eq!(repeated.revision, current.revision);
    assert!(
        music_collections::tags(
            &state,
            owner,
            current.id,
            &Tags {
                revision: current.revision.to_string(),
                tags: vec!["new_genre".into()]
            },
            TagAction::Add
        )
        .await
        .is_err()
    );
    assert_eq!(
        music_collections::get(&state, current.id, Some(owner))
            .await
            .unwrap()
            .tags,
        tags
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn collection_tag_edits_require_owner_current_revision_and_valid_bounds(pool: PgPool) {
    let (state, owner, viewer, current) = fixture(pool).await;
    music_collections::grant(&state, owner, current.id, current.revision, viewer, true)
        .await
        .unwrap();
    let current = music_collections::get(&state, current.id, Some(owner))
        .await
        .unwrap();
    for (actor, revision, tags) in [
        (viewer, current.revision, vec!["viewer_tag".into()]),
        (owner, current.revision - 1, vec!["stale".into()]),
        (owner, current.revision, vec!["#".into()]),
        (owner, current.revision, vec!["x".repeat(101)]),
        (owner, current.revision, vec!["bad\0tag".into()]),
    ] {
        assert!(
            music_collections::tags(
                &state,
                actor,
                current.id,
                &Tags {
                    revision: revision.to_string(),
                    tags
                },
                TagAction::Replace
            )
            .await
            .is_err()
        );
        assert!(
            music_collections::get(&state, current.id, Some(owner))
                .await
                .unwrap()
                .tags
                .is_empty()
        );
    }
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_collections::tags(
            &state,
            owner,
            current.id,
            &Tags {
                revision: current.revision.to_string(),
                tags: vec!["retired".into()]
            },
            TagAction::Replace
        )
        .await
        .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn collection_tag_search_filters_acl_before_paging_and_my_music_uses_current_tags(
    pool: PgPool,
) {
    let (state, owner, _, mut private) = fixture(pool).await;
    private = music_collections::tags(
        &state,
        owner,
        private.id,
        &Tags {
            revision: private.revision.to_string(),
            tags: vec!["find_me".into()],
        },
        TagAction::Replace,
    )
    .await
    .unwrap();
    let public = music_collections::create(
        &state,
        owner,
        &New {
            name: "Public tag playlist".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    music_collections::tags(
        &state,
        owner,
        public.id,
        &Tags {
            revision: public.revision.to_string(),
            tags: vec!["find_me".into()],
        },
        TagAction::Replace,
    )
    .await
    .unwrap();
    for index in 0..62 {
        music_collections::create(
            &state,
            owner,
            &New {
                name: format!("Noise {index}"),
                description: String::new(),
                kind: "playlist".into(),
                visibility: "public".into(),
            },
        )
        .await
        .unwrap();
    }
    let found = music_collections::search(&state, None, false, None, "FIND_ME", None)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, public.id);
    let personal = plamenu::music_mine::page(
        &state,
        owner,
        &plamenu::music_mine::Request {
            q: "find_me".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(personal.items.len(), 2);
    music_collections::tags(
        &state,
        owner,
        private.id,
        &Tags {
            revision: private.revision.to_string(),
            tags: Vec::new(),
        },
        TagAction::Replace,
    )
    .await
    .unwrap();
    let personal = plamenu::music_mine::page(
        &state,
        owner,
        &plamenu::music_mine::Request {
            q: "find_me".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(personal.items.len(), 1);
}
