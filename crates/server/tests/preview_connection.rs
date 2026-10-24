//! Borrowed preview connections retain committed visibility and image-cache rules.

use plamenu_db::account::{self, NewLocalAccount};
use plamenu_db::preview_card::{self, NewPreviewCard};
use plamenu_db::{PgPool, status};
use std::time::Duration;

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn borrowed_preview_connection_keeps_commits_cache_invalidation_and_attach_conflicts(
    pool: PgPool,
) {
    let actor = account::create_local(
        &pool,
        NewLocalAccount {
            username: "owner",
            display_name: "Owner",
            note: "",
            public_key_pem: "fixture",
        },
    )
    .await
    .unwrap();
    let post = status::create_local(
        &pool,
        status::NewLocalStatus::new(actor.id, "preview", "public", None),
    )
    .await
    .unwrap();
    let url = "https://news.example/article";
    let image = "https://news.example/original.jpg";
    let card = preview_card::upsert(
        &pool,
        NewPreviewCard {
            url,
            image_url: Some(image),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    preview_card::set_image_file(&pool, card.id, "cached.avif", "image/avif", 1234)
        .await
        .unwrap();
    let cached = preview_card::find_by_url(&pool, url)
        .await
        .unwrap()
        .unwrap();
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut connection = one.acquire().await.unwrap();
        let kept = preview_card::upsert(
            &mut *connection,
            NewPreviewCard {
                url,
                title: "visible before releasing the connection",
                image_url: Some(image),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(kept.id, card.id);
        assert_eq!(kept.image_file_name, cached.image_file_name);
        assert_eq!(kept.image_content_type, cached.image_content_type);
        assert_eq!(kept.image_file_size, cached.image_file_size);
        assert_eq!(kept.image_cached_at, cached.image_cached_at);
        let observed = preview_card::find_by_url(&pool, url)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.title, kept.title);
        preview_card::attach(&mut *connection, post.id, card.id, url)
            .await
            .unwrap();
        preview_card::attach(&mut *connection, post.id, card.id, "must not overwrite")
            .await
            .unwrap();
        let original: String = sqlx::query_scalar(
            "SELECT url FROM preview_cards_statuses WHERE status_id = $1 AND preview_card_id = $2",
        )
        .bind(post.id)
        .bind(card.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(original, url);
        assert!(
            preview_card::attach(&mut *connection, -1, card.id, url)
                .await
                .is_err()
        );
        let changed = preview_card::upsert(
            &mut *connection,
            NewPreviewCard {
                url,
                title: "connection survives the failed attach",
                image_url: Some("https://news.example/replaced.jpg"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(changed.id, card.id);
        assert_eq!(changed.image_file_name, None);
        assert_eq!(changed.image_content_type, None);
        assert_eq!(changed.image_file_size, None);
        assert_eq!(changed.image_cached_at, None);
        let observed = preview_card::find_by_url(&pool, url)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.title, changed.title);
        assert_eq!(observed.image_url, changed.image_url);
    })
    .await
    .unwrap();
    let count: i64 = tokio::time::timeout(
        Duration::from_secs(5),
        sqlx::query_scalar("SELECT count(*) FROM preview_cards_statuses").fetch_one(&one),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        count, 1,
        "the worker lease is released before later pool work"
    );
    one.close().await;
}
