//! Durable 32-bit aliases for the Lemmy compatibility API.
//!
//! Plamenu's native IDs are Mastodon-style 64-bit snowflakes. Mastodon renders
//! those as opaque strings, but Lemmy's wire types are signed 32-bit JSON
//! numbers. This table-backed namespace is the only lossless bridge: native
//! IDs and canonical `ActivityPub` URLs remain unchanged, while every `/api/v3`
//! entity receives a stable positive `i32` alias.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;

use crate::DbError;

/// Entity namespaces whose native IDs can appear on Lemmy's wire surface.
///
/// The discriminants are persisted by migration 0045 and must never be
/// renumbered or reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i16)]
pub enum Kind {
    Account = 1,
    User = 2,
    Status = 3,
    Report = 4,
    Registration = 5,
    CustomEmoji = 6,
    AdminAction = 7,
    Media = 8,
    Notification = 9,
    PrivateMessage = 10,
}

impl Kind {
    const fn as_i16(self) -> i16 {
        self as i16
    }
}

/// Return stable Lemmy aliases for all `native_ids` in one entity namespace.
///
/// The warm path is one query regardless of input cardinality. A cold or
/// partially cold path uses one set-based insert and one final set-based read
/// in a transaction; `ON CONFLICT` makes concurrent first exposure converge on
/// the same durable aliases. Duplicates in `native_ids` are intentionally
/// collapsed in the returned map.
pub async fn aliases_for(
    pool: &PgPool,
    kind: Kind,
    native_ids: &[i64],
) -> Result<HashMap<i64, i32>, DbError> {
    let mut ids = native_ids
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let existing = select_aliases(pool, kind, &ids).await?;
    if existing.len() == ids.len() {
        return Ok(existing.into_iter().collect());
    }

    let existing_ids = existing
        .iter()
        .map(|(native, _)| *native)
        .collect::<HashSet<_>>();
    let missing = ids
        .iter()
        .copied()
        .filter(|id| !existing_ids.contains(id))
        .collect::<Vec<_>>();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        r"
        INSERT INTO lemmy_id_aliases (kind, plamenu_id)
        SELECT $1, id
        FROM unnest($2::bigint[]) AS input(id)
        ON CONFLICT (kind, plamenu_id) DO NOTHING
        ",
    )
    .bind(kind.as_i16())
    .bind(&missing)
    .execute(&mut *transaction)
    .await?;
    let rows = sqlx::query_as::<_, (i64, i32)>(
        r"
        SELECT plamenu_id, lemmy_id
        FROM lemmy_id_aliases
        WHERE kind = $1 AND plamenu_id = ANY($2)
        ",
    )
    .bind(kind.as_i16())
    .bind(&ids)
    .fetch_all(&mut *transaction)
    .await?;
    transaction.commit().await?;
    debug_assert_eq!(rows.len(), ids.len());
    Ok(rows.into_iter().collect())
}

async fn select_aliases(
    pool: &PgPool,
    kind: Kind,
    native_ids: &[i64],
) -> Result<Vec<(i64, i32)>, DbError> {
    sqlx::query_as(
        r"
        SELECT plamenu_id, lemmy_id
        FROM lemmy_id_aliases
        WHERE kind = $1 AND plamenu_id = ANY($2)
        ",
    )
    .bind(kind.as_i16())
    .bind(native_ids)
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

/// Allocate or retrieve one stable alias.
pub async fn alias_for(pool: &PgPool, kind: Kind, native_id: i64) -> Result<i32, DbError> {
    aliases_for(pool, kind, &[native_id])
        .await?
        .remove(&native_id)
        .ok_or_else(|| DbError::from(sqlx::Error::RowNotFound))
}

/// Resolve a client-supplied Lemmy alias back to a native Plamenu ID.
pub async fn resolve(pool: &PgPool, kind: Kind, lemmy_id: i32) -> Result<Option<i64>, DbError> {
    sqlx::query_scalar(
        r"
        SELECT plamenu_id
        FROM lemmy_id_aliases
        WHERE kind = $1 AND lemmy_id = $2
        ",
    )
    .bind(kind.as_i16())
    .bind(lemmy_id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::from)
}

/// Resolve a batch of client-supplied aliases in one query.
pub async fn resolve_many(
    pool: &PgPool,
    kind: Kind,
    lemmy_ids: &[i32],
) -> Result<HashMap<i32, i64>, DbError> {
    let mut ids = lemmy_ids
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (i32, i64)>(
        r"
        SELECT lemmy_id, plamenu_id
        FROM lemmy_id_aliases
        WHERE kind = $1 AND lemmy_id = ANY($2)
        ",
    )
    .bind(kind.as_i16())
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::{Kind, alias_for, aliases_for, resolve, resolve_many};
    use crate::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn aliases_are_stable_typed_i32_values(pool: PgPool) {
        let native = 117_081_485_687_195_812_i64;
        let post = alias_for(&pool, Kind::Status, native).await.unwrap();
        let same_post = alias_for(&pool, Kind::Status, native).await.unwrap();
        let account = alias_for(&pool, Kind::Account, native).await.unwrap();

        assert!(post > 0);
        assert_eq!(post, same_post);
        assert_ne!(post, account, "entity namespaces must not alias each other");
        assert_eq!(
            resolve(&pool, Kind::Status, post).await.unwrap(),
            Some(native)
        );
        assert_eq!(resolve(&pool, Kind::Account, post).await.unwrap(), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn batch_allocation_deduplicates_and_round_trips(pool: PgPool) {
        let ids = [
            117_081_485_687_195_812,
            117_081_485_687_195_813,
            117_081_485_687_195_812,
            117_081_485_687_195_814,
        ];
        let aliases = aliases_for(&pool, Kind::Status, &ids).await.unwrap();
        assert_eq!(aliases.len(), 3);
        for native in ids {
            let alias = aliases[&native];
            assert_eq!(
                resolve(&pool, Kind::Status, alias).await.unwrap(),
                Some(native)
            );
        }
        let reverse = resolve_many(
            &pool,
            Kind::Status,
            &aliases.values().copied().collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        assert_eq!(reverse.len(), 3);
        assert!(reverse.values().all(|native| aliases.contains_key(native)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_first_exposure_converges(pool: PgPool) {
        let native = 117_081_485_687_195_812_i64;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(12));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..12 {
            let pool = pool.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                alias_for(&pool, Kind::Status, native).await.unwrap()
            });
        }
        let mut aliases = Vec::new();
        while let Some(alias) = tasks.join_next().await {
            aliases.push(alias.unwrap());
        }
        assert!(aliases.iter().all(|alias| *alias == aliases[0]));
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM lemmy_id_aliases WHERE kind = 3 AND plamenu_id = $1",
        )
        .bind(native)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 1);
    }
}
