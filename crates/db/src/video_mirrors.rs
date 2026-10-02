//! Private, quota-reserved HLS mirror jobs and authenticated remote offers.
//! Physical retirement stays charged until the ordinary cleanup ACK.
use crate::{DbError, PgConnection, PgPool, id};
use time::OffsetDateTime;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Mirror {
    pub id: i64,
    pub media_id: i64,
    pub status_id: i64,
    pub requested_by: Option<i64>,
    pub source_uri: String,
    pub source_master_url: String,
    pub source_updated_at: OffsetDateTime,
    pub consent_attestation: String,
    pub state: String,
    pub generation: i64,
    pub lease_until: Option<OffsetDateTime>,
    pub expires_at: OffsetDateTime,
    pub reserved_bytes: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub last_error: String,
    pub create_activity: Option<serde_json::Value>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Asset {
    pub id: i64,
    pub mirror_id: i64,
    pub generation: i64,
    pub source_url: String,
    pub public_name: String,
    pub file_name: String,
    pub bytes: i64,
    pub content_type: String,
    pub sha256: String,
}

pub struct Request<'a> {
    pub media_id: i64,
    pub status_id: i64,
    pub requested_by: i64,
    pub source_uri: &'a str,
    pub source_master_url: &'a str,
    pub source_updated_at: OffsetDateTime,
    pub consent_attestation: &'a str,
    pub expires_at: OffsetDateTime,
    pub reserved_bytes: i64,
}

const QUOTA_LOCK: i64 = 0x4d49_5252_4f52;

/// Reservation covers current generation's writes, including private
/// pre-write journals. Retired/failed/older-generation journals are additional
/// physical debt, never refunded merely because a public asset row is gone.
pub async fn charged(pool: &PgPool) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE((SELECT sum(reserved_bytes) FROM video_mirrors
          WHERE state IN ('queued','copying','ready')),0)::bigint
        + COALESCE((SELECT sum(c.mirror_bytes) FROM media_cleanup_jobs c
          WHERE c.mirror_bytes>0 AND NOT EXISTS(
            SELECT 1 FROM video_mirrors m WHERE m.id=c.mirror_id
            AND m.generation=c.mirror_generation AND m.state='copying')),0)::bigint",
    )
    .fetch_one(pool)
    .await?)
}

/// Server validates operator/domain/creator/current-publication authority
/// before calling; the same checks must run at claim, publication and serving.
pub async fn enqueue(
    pool: &PgPool,
    request: Request<'_>,
    quota: i64,
) -> Result<Option<Mirror>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(QUOTA_LOCK)
        .execute(&mut *tx)
        .await?;
    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT sum(reserved_bytes) FROM video_mirrors
          WHERE state IN ('queued','copying','ready')),0)::bigint
        + COALESCE((SELECT sum(c.mirror_bytes) FROM media_cleanup_jobs c
          WHERE c.mirror_bytes>0 AND NOT EXISTS(
            SELECT 1 FROM video_mirrors m WHERE m.id=c.mirror_id
            AND m.generation=c.mirror_generation AND m.state='copying')),0)::bigint",
    )
    .fetch_one(&mut *tx)
    .await?;
    if request.reserved_bytes <= 0
        || used
            .checked_add(request.reserved_bytes)
            .is_none_or(|total| total > quota)
    {
        return Ok(None);
    }
    let item = sqlx::query_as::<_, Mirror>(
        "INSERT INTO video_mirrors(id,media_id,status_id,requested_by,source_uri,source_master_url,
          source_updated_at,consent_attestation,expires_at,reserved_bytes)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
        ON CONFLICT(media_id) WHERE state IN ('queued','copying','ready') DO NOTHING RETURNING *",
    )
    .bind(id::next())
    .bind(request.media_id)
    .bind(request.status_id)
    .bind(request.requested_by)
    .bind(request.source_uri)
    .bind(request.source_master_url)
    .bind(request.source_updated_at)
    .bind(request.consent_attestation)
    .bind(request.expires_at)
    .bind(request.reserved_bytes)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(item)
}

/// One native writer at a time. A stale claim cannot publish into its
/// replacement generation, including after a consistent fresh-root restore.
pub async fn claim(pool: &PgPool, quota: i64) -> Result<Option<Mirror>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(QUOTA_LOCK)
        .execute(&mut *tx)
        .await?;
    let item = sqlx::query_as::<_, Mirror>(
        "SELECT * FROM video_mirrors WHERE expires_at>clock_timestamp()
        AND (state='queued' OR (state='copying' AND lease_until<=clock_timestamp()))
        AND NOT EXISTS(SELECT 1 FROM video_mirrors WHERE state='copying' AND lease_until>clock_timestamp())
        ORDER BY created_at,id LIMIT 1 FOR UPDATE SKIP LOCKED",
    ).fetch_optional(&mut *tx).await?;
    let Some(item) = item else {
        return Ok(None);
    };
    sqlx::query("DELETE FROM video_mirror_assets WHERE mirror_id=$1")
        .bind(item.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE media_cleanup_jobs SET run_at=clock_timestamp() WHERE mirror_id=$1 AND mirror_generation=$2")
        .bind(item.id).bind(item.generation).execute(&mut *tx).await?;
    // Old physical debt must not disappear into the new reservation. Commit
    // retirement first; never overwrite or reuse a previous generation's key.
    sqlx::query("UPDATE video_mirrors SET state='queued',generation=generation+1,lease_until=NULL WHERE id=$1")
        .bind(item.id).execute(&mut *tx).await?;
    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT sum(reserved_bytes) FROM video_mirrors
          WHERE state IN ('queued','copying','ready')),0)::bigint
        + COALESCE((SELECT sum(c.mirror_bytes) FROM media_cleanup_jobs c
          WHERE c.mirror_bytes>0 AND NOT EXISTS(
            SELECT 1 FROM video_mirrors m WHERE m.id=c.mirror_id
            AND m.generation=c.mirror_generation AND m.state='copying')),0)::bigint",
    )
    .fetch_one(&mut *tx)
    .await?;
    if used > quota {
        tx.commit().await?;
        return Ok(None);
    }
    let claimed = sqlx::query_as::<_, Mirror>(
        "UPDATE video_mirrors SET state='copying',
        lease_until=clock_timestamp()+interval '20 minutes',updated_at=clock_timestamp(),last_error=''
        WHERE id=$1 RETURNING *",
    ).bind(item.id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(claimed))
}

pub async fn find(pool: &PgPool, mirror: i64) -> Result<Option<Mirror>, DbError> {
    Ok(
        sqlx::query_as::<_, Mirror>("SELECT * FROM video_mirrors WHERE id=$1")
            .bind(mirror)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn assets(pool: &PgPool, mirror: i64, generation: i64) -> Result<Vec<Asset>, DbError> {
    Ok(sqlx::query_as::<_, Asset>("SELECT * FROM video_mirror_assets WHERE mirror_id=$1 AND generation=$2 ORDER BY public_name")
        .bind(mirror).bind(generation).fetch_all(pool).await?)
}

/// Caller stores the returned Create before publication and atomically
/// enqueues its signed Undo on withdrawal. Asset deletion here retains debt.
pub async fn withdraw_conn(
    conn: &mut PgConnection,
    mirror: i64,
) -> Result<Option<serde_json::Value>, DbError> {
    let activity = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "UPDATE video_mirrors SET state='withdrawn',lease_until=NULL,updated_at=clock_timestamp()
        WHERE id=$1 AND state<>'withdrawn' RETURNING create_activity",
    )
    .bind(mirror)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    sqlx::query("DELETE FROM video_mirror_assets WHERE mirror_id=$1")
        .bind(mirror)
        .execute(&mut *conn)
        .await?;
    Ok(activity)
}

pub async fn approved<'e>(
    executor: impl crate::PgExecutor<'e>,
    domain: &str,
    source: bool,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE((SELECT CASE WHEN $2 THEN allow_source ELSE allow_mirror END
        FROM video_mirror_approvals WHERE domain=$1),false)",
    )
    .bind(domain)
    .bind(source)
    .fetch_one(executor)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account, media, status};

    async fn fixture(pool: &PgPool) -> (i64, i64, i64) {
        let account = account::create_local(
            pool,
            account::NewLocalAccount {
                username: "mirroroperator",
                display_name: "",
                note: "",
                public_key_pem: "public",
            },
        )
        .await
        .unwrap();
        let status = status::create_local(
            pool,
            status::NewLocalStatus::new(account.id, "<p>video</p>", "public", None),
        )
        .await
        .unwrap();
        let media = id::next();
        media::create_local(
            pool,
            media::NewLocalMedia::new(account.id, media, "source.mp4", "video/mp4"),
        )
        .await
        .unwrap();
        (account.id, status.id, media)
    }

    async fn request(pool: &PgPool, quota: i64) -> Option<Mirror> {
        let (operator, status, media) = fixture(pool).await;
        enqueue(
            pool,
            Request {
                media_id: media,
                status_id: status,
                requested_by: operator,
                source_uri: "https://source.example/videos/1",
                source_master_url: "https://source.example/1/master.m3u8",
                source_updated_at: OffsetDateTime::now_utc(),
                consent_attestation: "",
                expires_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
                reserved_bytes: 100,
            },
            quota,
        )
        .await
        .unwrap()
    }

    async fn asset(pool: &PgPool, item: &Mirror, bytes: i64) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO video_mirror_assets(id,mirror_id,generation,source_url,public_name,file_name,bytes,content_type,sha256)
        VALUES($1,$2,$3,'https://source.example/file.mp4','file.mp4','private-mirror.mp4',$4,'video/mp4',$5)")
            .bind(id::next()).bind(item.id).bind(item.generation).bind(bytes).bind("a".repeat(64)).execute(pool).await?;
        Ok(())
    }

    #[sqlx::test]
    async fn mirror_retirement_is_charged_until_physical_ack_and_withdrawal_is_atomic(
        pool: PgPool,
    ) {
        assert!(request(&pool, 99).await.is_none());
        // Each test request uses the same operator fixture; the refusal above
        // left its source rows intact but did not reserve any mirror capacity.
        let (operator, status, media): (i64,i64,i64) = sqlx::query_as(
            "SELECT a.id,s.id,m.id FROM accounts a JOIN statuses s ON s.account_id=a.id JOIN media_attachments m ON m.account_id=a.id LIMIT 1\n-- STUBKEEP: synthetic mirror test fixture selects identities only, without public rendering"
        ).fetch_one(&pool).await.unwrap();
        let item = enqueue(
            &pool,
            Request {
                media_id: media,
                status_id: status,
                requested_by: operator,
                source_uri: "https://source.example/videos/1",
                source_master_url: "https://source.example/master.m3u8",
                source_updated_at: OffsetDateTime::now_utc(),
                consent_attestation: "",
                expires_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
                reserved_bytes: 100,
            },
            100,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(charged(&pool).await.unwrap(), 100);
        let claimed = claim(&pool, 100).await.unwrap().unwrap();
        assert_eq!(claimed.id, item.id);
        assert!(claim(&pool, 100).await.unwrap().is_none());
        asset(&pool, &claimed, 75).await.unwrap();
        assert!(
            asset(&pool, &claimed, 26)
                .await
                .unwrap_err()
                .to_string()
                .contains("reserved quota")
        );
        let mut tx = pool.begin().await.unwrap();
        withdraw_conn(&mut tx, item.id).await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(
            find(&pool, item.id).await.unwrap().unwrap().state,
            "copying"
        );
        assert_eq!(
            assets(&pool, item.id, claimed.generation)
                .await
                .unwrap()
                .len(),
            1
        );
        let mut tx = pool.begin().await.unwrap();
        withdraw_conn(&mut tx, item.id).await.unwrap();
        tx.commit().await.unwrap();
        assert!(
            asset(&pool, &claimed, 1)
                .await
                .unwrap_err()
                .to_string()
                .contains("Stale or expired")
        );
        assert_eq!(charged(&pool).await.unwrap(), 75);
        sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name='private-mirror.mp4'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(charged(&pool).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn deleting_source_preserves_physical_debt_without_dangling_mirror_fk(pool: PgPool) {
        let item = request(&pool, 100).await.unwrap();
        let claimed = claim(&pool, 100).await.unwrap().unwrap();
        asset(&pool, &claimed, 75).await.unwrap();
        sqlx::query("DELETE FROM statuses WHERE id=$1")
            .bind(item.status_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(find(&pool, item.id).await.unwrap().is_none());
        assert_eq!(charged(&pool).await.unwrap(), 75);
        let parent: Option<i64> = sqlx::query_scalar(
            "SELECT mirror_id FROM media_cleanup_jobs WHERE file_name='private-mirror.mp4'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(parent.is_none());
    }

    #[sqlx::test]
    async fn stale_claim_retirement_waits_for_cleanup_before_reusing_its_reservation(pool: PgPool) {
        let item = request(&pool, 100).await.unwrap();
        let first = claim(&pool, 100).await.unwrap().unwrap();
        asset(&pool, &first, 75).await.unwrap();
        sqlx::query("UPDATE video_mirrors SET lease_until=clock_timestamp()-interval '1 second' WHERE id=$1")
            .bind(item.id).execute(&pool).await.unwrap();
        assert!(claim(&pool, 100).await.unwrap().is_none());
        assert_eq!(find(&pool, item.id).await.unwrap().unwrap().state, "queued");
        assert_eq!(charged(&pool).await.unwrap(), 175);
        sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name='private-mirror.mp4'")
            .execute(&pool)
            .await
            .unwrap();
        let second = claim(&pool, 100).await.unwrap().unwrap();
        assert!(second.generation > first.generation);
        assert!(
            asset(&pool, &first, 1)
                .await
                .unwrap_err()
                .to_string()
                .contains("Stale or expired")
        );
        assert_eq!(charged(&pool).await.unwrap(), 100);
    }
}
