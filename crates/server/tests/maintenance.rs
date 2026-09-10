//! Integration tests for background maintenance jobs. Currently the stored-size
//! backfill (O5): it stats each stored file whose byte size is still unknown and
//! records it so the admin storage metrics account for media stored before
//! sizes were tracked. The sweep reads each file's length through the store's
//! streaming `open()` API rather than buffering the whole file.

mod common;

use std::sync::Arc;

use common::{create_local_account, test_state_with};
use plamenu_db::{PgPool, id, media};

/// The backfill records the stored byte length of a media row that has a file
/// on disk but no recorded `file_size`, and reports it as filled.
#[sqlx::test(migrations = "../db/migrations")]
async fn backfill_fills_missing_media_sizes(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    // A local media row is created without a size (the column this backfill
    // exists to populate), with its bytes on the store.
    let media_id = id::next();
    media::create_local(
        &pool,
        media::NewLocalMedia::new(alice.id, media_id, "pic-123.png", "image/png"),
    )
    .await
    .unwrap();
    state
        .media
        .put("pic-123.png", b"PNGDATA".to_vec())
        .await
        .unwrap();

    // The row is initially missing its size, so the storage metric reads zero
    // and the sweep has work to do.
    let missing = plamenu_db::backfill::media_missing_sizes_page(&pool, 0, 1000)
        .await
        .unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(
        plamenu_db::metrics::media_storage_bytes(&pool)
            .await
            .unwrap(),
        0
    );

    let (filled, skipped) = plamenu::maintenance::backfill_stored_sizes(&state)
        .await
        .unwrap();
    assert_eq!((filled, skipped), (1, 0));

    // The recorded size matches the stored file's real length (7 bytes) — proof
    // the streaming `open().len` path reads the same length it used to buffer.
    assert_eq!(
        plamenu_db::metrics::media_storage_bytes(&pool)
            .await
            .unwrap(),
        i64::try_from(b"PNGDATA".len()).unwrap()
    );

    // Nothing is left to backfill on a second pass.
    assert!(
        plamenu_db::backfill::media_missing_sizes_page(&pool, 0, 1000)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The sweep pages through a backlog larger than one keyset page and still
/// fills every row, and a row whose file is missing from the store is skipped
/// (counted, cursor stepped past) without stalling the walk.
#[sqlx::test(migrations = "../db/migrations")]
async fn backfill_pages_through_a_large_backlog_and_skips_unreadable(pool: PgPool) {
    // More rows than a single keyset page (500) so the walk must span several
    // pages; every stored file is one byte so total bytes equals the count.
    const ROWS: usize = 500 * 2 + 7;

    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    for i in 0..ROWS {
        let media_id = id::next();
        let name = format!("pic-{i}.png");
        media::create_local(
            &pool,
            media::NewLocalMedia::new(alice.id, media_id, &name, "image/png"),
        )
        .await
        .unwrap();
        state.media.put(&name, b"X".to_vec()).await.unwrap();
    }

    // One extra row whose bytes were never written to the store: the sweep can
    // stat neither its size, so it is skipped rather than filled or looped on.
    let orphan_id = id::next();
    media::create_local(
        &pool,
        media::NewLocalMedia::new(alice.id, orphan_id, "gone.png", "image/png"),
    )
    .await
    .unwrap();

    let (filled, skipped) = plamenu::maintenance::backfill_stored_sizes(&state)
        .await
        .unwrap();
    assert_eq!(filled, u64::try_from(ROWS).unwrap());
    assert_eq!(skipped, 1);

    // Every readable row got its size; the metric equals the byte total.
    assert_eq!(
        plamenu_db::metrics::media_storage_bytes(&pool)
            .await
            .unwrap(),
        i64::try_from(ROWS).unwrap()
    );

    // Only the orphan remains missing, and a second sweep leaves it be — no
    // infinite re-fetch of the sizeless row.
    let remaining = plamenu_db::backfill::media_missing_sizes_page(&pool, 0, 10_000)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, orphan_id);
    let (filled_again, skipped_again) = plamenu::maintenance::backfill_stored_sizes(&state)
        .await
        .unwrap();
    assert_eq!((filled_again, skipped_again), (0, 1));
}
