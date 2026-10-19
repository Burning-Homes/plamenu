use super::*;
use plamenu::discovery::topics;

async fn tag(pool: &PgPool, id: i64, value: &str) {
    sqlx::query("INSERT INTO discovery_topics(entry_id,topic) VALUES($1,$2)")
        .bind(id)
        .bind(value)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn topics_and_trails_use_explicit_tags_and_recheck_viewer_policy(pool: PgPool) {
    let f = fixture(&pool).await;
    tag(&pool, f.entry, "garden").await;
    let before = effects(&pool).await;
    let page = f.get("/discover/topics").await;
    assert!(page.contains("#garden"));
    assert!(
        f.get("/discover/topics?topic=GARDEN")
            .await
            .contains("alice &lt;name&gt;")
    );
    assert!(
        !f.get("/discover/topics?topic=hello")
            .await
            .contains("alice &lt;name&gt;")
    );
    assert!(
        f.get(&format!("/discover/trail/{}", f.entry))
            .await
            .contains("#garden")
    );
    assert_eq!(effects(&pool).await, before);
    assert!(f.stub.fetches.lock().unwrap().is_empty());
    let hidden = f
        .request(
            "POST",
            &format!("/web/discover/{}/dismiss", f.entry),
            &[
                ("csrf", &f.csrf()),
                ("back", "/discover/topics?topic=garden"),
            ],
        )
        .await;
    assert_eq!(
        hidden.1["location"],
        "/discover/topics?topic=garden&notice=hidden"
    );
    assert!(!f.get("/discover/topics").await.contains("#garden"));
    assert!(
        !f.get(&format!("/discover/trail/{}", f.entry))
            .await
            .contains("alice &lt;name&gt;")
    );
    let other = create_local_account(&pool, "other", "Other").await;
    assert_eq!(
        topics::available(&f.state, other.id).await.unwrap(),
        ["garden"]
    );
    sqlx::query("UPDATE discovery_entries SET verified_at=now()-interval '2 hours',valid_until=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        topics::available(&f.state, other.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn saved_topics_are_private_csrf_protected_resettable_and_concurrently_bounded(pool: PgPool) {
    let f = fixture(&pool).await;
    let bad = f
        .request(
            "POST",
            "/web/discover/topics",
            &[("csrf", "bad"), ("action", "save"), ("topic", "garden")],
        )
        .await;
    assert_eq!(bad.0, StatusCode::FORBIDDEN);
    assert!(db::saved_topics(&pool, f.viewer).await.unwrap().is_empty());
    let good = f
        .request(
            "POST",
            "/web/discover/topics",
            &[
                ("csrf", &f.csrf()),
                ("action", "save"),
                ("topic", "#Garden"),
            ],
        )
        .await;
    assert_eq!(good.0, StatusCode::SEE_OTHER);
    assert_eq!(db::saved_topics(&pool, f.viewer).await.unwrap(), ["garden"]);
    let other = create_local_account(&pool, "other", "Other").await;
    assert!(db::saved_topics(&pool, other.id).await.unwrap().is_empty());
    let mut jobs = tokio::task::JoinSet::new();
    for n in 0..30 {
        let pool = pool.clone();
        let viewer = f.viewer;
        jobs.spawn(async move {
            db::save_topic(&pool, viewer, &format!("topic{n}"))
                .await
                .unwrap()
        });
    }
    while let Some(result) = jobs.join_next().await {
        result.unwrap();
    }
    assert_eq!(db::saved_topics(&pool, f.viewer).await.unwrap().len(), 20);
    assert!(db::save_topic(&pool, f.viewer, "garden").await.unwrap());
    db::save_topic(&pool, other.id, "private").await.unwrap();
    db::dismiss(&pool, f.viewer, f.entry).await.unwrap();
    let mut settings = db::settings(&pool).await.unwrap();
    settings.enabled = false;
    db::save_settings(&pool, &settings).await.unwrap();
    f.request(
        "POST",
        "/web/discover/topics",
        &[("csrf", &f.csrf()), ("action", "reset")],
    )
    .await;
    assert!(db::saved_topics(&pool, f.viewer).await.unwrap().is_empty());
    assert_eq!(
        db::saved_topics(&pool, other.id).await.unwrap(),
        ["private"]
    );
    assert!(db::viewer_hides(&pool, f.viewer, f.entry).await.unwrap());
    assert_eq!(
        f.request("GET", "/discover/topics?topic=%3Cscript%3E", &[])
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn topics_do_not_promote_index_only_authors_or_filtered_posts(pool: PgPool) {
    let f = fixture(&pool).await;
    seed_browser_posts(&pool).await;
    let post: i64 = sqlx::query_scalar(
        "SELECT id FROM discovery_entries WHERE kind='post' ORDER BY id LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    tag(&pool, post, "orchids").await;
    // The fixture's first post has a content warning: no topic hub leak.
    assert!(
        topics::available(&f.state, f.viewer)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        topics::page(&f.state, f.viewer, None, "orchids")
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    plamenu_db::custom_filter::create(
        &pool,
        f.viewer,
        "Plants",
        "hide",
        &["public".into()],
        None,
        &[plamenu_db::custom_filter::NewKeyword {
            keyword: "orchids".into(),
            whole_word: true,
        }],
    )
    .await
    .unwrap();
    plamenu::filters::invalidate(f.viewer);
    assert!(
        topics::eligible(&f.state, f.viewer, post)
            .await
            .unwrap()
            .is_none()
    );
    let other = create_local_account(&pool, "other", "Other").await;
    sqlx::query("UPDATE discovery_entries SET discoverable='deny' WHERE kind='profile'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        topics::eligible(&f.state, other.id, post)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        plamenu::discovery::posts::eligible(
            &f.state,
            other.id,
            post,
            plamenu::discovery::policy::Use::IndexPost
        )
        .await
        .unwrap()
        .is_some()
    );
    let mut settings = db::settings(&pool).await.unwrap();
    settings.disabled_mechanisms.push("recommendations".into());
    db::save_settings(&pool, &settings).await.unwrap();
    assert!(
        topics::page(&f.state, other.id, None, "orchids")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

async fn collection_fixture(f: &Fixture) -> (i64, i64) {
    use plamenu_db::collection::{self, NewCollection, NewCollectionItem};
    let pool = &f.state.pool;
    let owner = create_local_account(pool, "curator", "Community curator").await;
    sqlx::query("UPDATE accounts SET discoverable=true WHERE id=$1")
        .bind(owner.id)
        .execute(pool)
        .await
        .unwrap();
    let remote = plamenu::remote::refresh_remote_actor(
        &f.state,
        &RemoteUser::new("one.example", "alice").actor,
    )
    .await
    .unwrap();
    let tag_id = plamenu_db::tag::ensure(pool, "Garden").await.unwrap();
    let c = collection::create(
        pool,
        NewCollection {
            account_id: owner.id,
            name: "Garden people",
            description: "People growing things",
            language: None,
            sensitive: false,
            discoverable: true,
            local: true,
            tag_id: Some(tag_id),
            uri: None,
            url: None,
            original_number_of_items: None,
        },
    )
    .await
    .unwrap();
    let item = collection::add_item(
        pool,
        NewCollectionItem {
            item_id: plamenu_db::id::next(),
            collection_id: c.id,
            account_id: Some(remote.id),
            state: "pending",
            uri: None,
            object_uri: Some("https://one.example/users/alice"),
            activity_uri: None,
            approval_uri: None,
        },
    )
    .await
    .unwrap();
    (c.id, item.unwrap().id)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn collections_need_public_local_ownership_accepted_approval_and_visible_members(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let (collection, item) = collection_fixture(&f).await;
    let before = effects(&pool).await;
    assert!(
        topics::public_collection(&f.state, f.viewer, collection)
            .await
            .unwrap()
            .is_none()
    );
    plamenu_db::collection::set_item_state(&pool, item, "accepted", None)
        .await
        .unwrap();
    assert!(
        topics::public_collection(&f.state, f.viewer, collection)
            .await
            .unwrap()
            .is_none()
    );
    plamenu_db::collection::set_item_state(
        &pool,
        item,
        "accepted",
        Some("https://one.example/approval"),
    )
    .await
    .unwrap();
    assert_eq!(
        topics::public_collection(&f.state, f.viewer, collection)
            .await
            .unwrap()
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        topics::available(&f.state, f.viewer).await.unwrap(),
        ["garden"]
    );
    assert!(
        f.get(&format!("/discover/collections/{collection}"))
            .await
            .contains("#garden")
    );
    assert!(
        f.get("/discover/topics?topic=garden")
            .await
            .contains("Garden people")
    );
    assert!(
        f.get(&format!("/discover/collections/{collection}"))
            .await
            .contains("Community curator")
    );
    assert!(
        f.get(&format!("/discover/trail/{}", f.entry))
            .await
            .contains("Garden people")
    );
    assert_eq!(effects(&pool).await, before);
    sqlx::query("UPDATE collections SET discoverable=false WHERE id=$1")
        .bind(collection)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        topics::collections(&f.state, f.viewer, "garden", "")
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query("UPDATE collections SET discoverable=true,local=false WHERE id=$1")
        .bind(collection)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        topics::public_collection(&f.state, f.viewer, collection)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE collections SET local=true WHERE id=$1")
        .bind(collection)
        .execute(&pool)
        .await
        .unwrap();
    plamenu_db::collection::set_item_state(&pool, item, "revoked", None)
        .await
        .unwrap();
    assert!(
        topics::public_collection(&f.state, f.viewer, collection)
            .await
            .unwrap()
            .is_none()
    );
    plamenu_db::collection::set_item_state(
        &pool,
        item,
        "accepted",
        Some("https://one.example/approval"),
    )
    .await
    .unwrap();
    db::dismiss(&pool, f.viewer, f.entry).await.unwrap();
    assert!(
        topics::public_collection(&f.state, f.viewer, collection)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn topic_changes_and_domain_blocks_remove_connections_without_fetching(pool: PgPool) {
    let f = fixture(&pool).await;
    tag(&pool, f.entry, "garden").await;
    let uri = "https://one.example/users/alice";
    plamenu::discovery::profiles::verified_update(
        &f.state,
        uri,
        &serde_json::json!({"id":uri,"type":"Person","tag":[]}),
    )
    .await
    .unwrap();
    assert!(db::entry_topics(&pool, f.entry).await.unwrap().is_empty());
    tag(&pool, f.entry, "repair").await;
    plamenu_db::account_domain_block::create(&pool, f.viewer, "one.example")
        .await
        .unwrap();
    assert!(
        topics::available(&f.state, f.viewer)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        topics::eligible(&f.state, f.viewer, f.entry)
            .await
            .unwrap()
            .is_none()
    );
}
