//! A borrowed physical connection must keep each attachment transaction atomic.

use plamenu_db::account::{self, NewLocalAccount};
use plamenu_db::media::{self, NewRemoteMedia, NewRendition};
use plamenu_db::{PgPool, status};

fn remote(actor: i64, post: i64, url: &'static str) -> NewRemoteMedia<'static> {
    NewRemoteMedia {
        account_id: actor,
        status_id: post,
        remote_url: url,
        content_type: "application/x-mpegURL",
        description: Some("original"),
        hls_master_url: Some("https://remote.test/master.m3u8"),
        renditions: vec![NewRendition {
            height: 720,
            width: Some(1280),
            frame_rate: Some(30),
            size_bytes: Some(100),
            origin_url: url,
            is_audio: false,
        }],
        ..Default::default()
    }
}

async fn counts(pool: &PgPool) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM media_attachments),
                (SELECT count(*) FROM media_processing_jobs),
                (SELECT count(*) FROM media_renditions)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn held_connection_keeps_per_attachment_commits_and_recovers_after_rendition_failure(
    pool: PgPool,
) {
    let account = account::create_local(
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
        status::NewLocalStatus::new(account.id, "media", "public", None),
    )
    .await
    .unwrap();
    sqlx::raw_sql(
        "CREATE FUNCTION fail_rendition() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.origin_url = 'https://remote.test/fail.mp4' THEN
           RAISE EXCEPTION 'injected rendition failure'; END IF; RETURN NEW; END $$;
         CREATE TRIGGER fail_rendition BEFORE INSERT ON media_renditions
         FOR EACH ROW EXECUTE FUNCTION fail_rendition()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    media::create_remote(
        &mut *connection,
        remote(account.id, post.id, "https://remote.test/first.mp4"),
    )
    .await
    .unwrap();
    // A different pool connection must already see this committed item.
    assert_eq!(counts(&pool).await, (1, 1, 1));
    let error = media::create_remote(
        &mut *connection,
        remote(account.id, post.id, "https://remote.test/fail.mp4"),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("injected rendition failure"));
    assert_eq!(counts(&pool).await, (1, 1, 1));
    // Rolling back the failed ladder must leave the borrowed connection usable.
    media::create_remote(
        &mut *connection,
        remote(account.id, post.id, "https://remote.test/next.mp4"),
    )
    .await
    .unwrap();
    assert_eq!(counts(&pool).await, (2, 2, 2));
    let mut duplicate = remote(account.id, post.id, "https://remote.test/next.mp4");
    duplicate.description = Some("must not replace the original");
    duplicate.renditions[0].origin_url = "https://remote.test/different.mp4";
    media::create_remote(&pool, duplicate).await.unwrap();
    assert_eq!(counts(&pool).await, (2, 2, 2));
    let description: String = sqlx::query_scalar(
        "SELECT description FROM media_attachments WHERE remote_url='https://remote.test/next.mp4'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(description, "original");
}
