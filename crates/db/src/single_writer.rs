//! Server and CLI writer-lifetime enforcement.
//!
//! Plamenu's snowflake ID allocator is process-local (see [`crate::id`]): two
//! processes in the same ID lane share no sequence state and can mint the same
//! ID during the same millisecond. The server and CLI have disjoint lanes;
//! separate locks admit one serving process and one CLI command per database.
//!
//! Rather than leave that as documentation an operator can miss, a running
//! server claims a session-scoped `PostgreSQL` *advisory* lock on its own
//! dedicated connection for its whole lifetime. A second server refuses to
//! start while the first holds it. The lock is tied to the connection's
//! session, so it is released automatically if the holder crashes or exits —
//! there is nothing to clean up by hand.

use sqlx::{Connection, PgConnection};

use crate::DbError;

/// The advisory-lock key every Plamenu server contends for. Arbitrary but
/// stable: the ASCII bytes `PLAMENU1` packed big-endian into a `bigint`. Every
/// server must use the same value; the high bit is clear, so it is a positive
/// `i64`.
const SINGLE_WRITER_LOCK_KEY: i64 = 0x504C_414D_454E_5531;
const CLI_WRITER_LOCK_KEY: i64 = 0x504C_414D_454E_5532;

/// Proof that this process holds the single-writer advisory lock. Holds the
/// dedicated connection the lock lives on; dropping it (or exiting the process)
/// ends that session and releases the lock. Call [`SingleWriterLock::release`]
/// for a deterministic, awaited release at graceful shutdown.
pub struct SingleWriterLock {
    conn: PgConnection,
    key: i64,
}

impl SingleWriterLock {
    /// Watches the original session without reconnecting. Losing it invalidates
    /// exclusivity: callers must terminate the writer, not reacquire silently.
    /// Polls every second and treats a five-second unresponsive session as lost.
    pub async fn monitor(&mut self) -> Result<(), DbError> {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            tokio::time::timeout(std::time::Duration::from_secs(5), self.conn.ping())
                .await
                .map_err(|_| {
                    sqlx::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "single-writer lock session is unresponsive",
                    ))
                })??;
        }
    }

    /// Releases the lock explicitly and closes its connection. Dropping the
    /// value releases the lock too (the session ends), but this awaits the
    /// release so a clean shutdown can hand the lock to a successor at once.
    pub async fn release(self) -> Result<(), DbError> {
        let mut conn = self.conn;
        // Returns whether a lock was released; we hold it, so ignore the value.
        sqlx::query_scalar!("SELECT pg_advisory_unlock($1)", self.key)
            .fetch_one(&mut conn)
            .await?;
        conn.close().await?;
        Ok(())
    }
}

/// Opens a dedicated connection and claims the single-writer lock on it,
/// returning the held lock or [`DbError::SingleWriterHeld`] when another writer
/// already holds it. Uses its own connection rather than the shared pool so the
/// long-held lock never occupies a request/worker connection.
pub async fn acquire_single_writer_lock(database_url: &str) -> Result<SingleWriterLock, DbError> {
    let conn = PgConnection::connect(database_url).await?;
    acquire_single_writer_lock_on(conn).await
}

/// Serializes CLI commands without excluding the serving process. The command
/// must select the CLI ID lane and monitor this session before initialization.
pub async fn acquire_cli_writer_lock(database_url: &str) -> Result<SingleWriterLock, DbError> {
    let conn = PgConnection::connect(database_url).await?;
    acquire_writer_lock_on(conn, CLI_WRITER_LOCK_KEY).await
}

/// The core claim, taking an already-opened dedicated connection so tests can
/// point it at an isolated database.
pub(crate) async fn acquire_single_writer_lock_on(
    conn: PgConnection,
) -> Result<SingleWriterLock, DbError> {
    acquire_writer_lock_on(conn, SINGLE_WRITER_LOCK_KEY).await
}

async fn acquire_writer_lock_on(
    mut conn: PgConnection,
    key: i64,
) -> Result<SingleWriterLock, DbError> {
    let acquired = sqlx::query_scalar!(r#"SELECT pg_try_advisory_lock($1) AS "acquired!""#, key)
        .fetch_one(&mut conn)
        .await?;
    if acquired {
        Ok(SingleWriterLock { conn, key })
    } else if key == CLI_WRITER_LOCK_KEY {
        Err(DbError::CliWriterHeld)
    } else {
        Err(DbError::SingleWriterHeld)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn cli_commands_are_serialized_while_the_server_keeps_its_lock(pool: PgPool) {
        let options = (*pool.connect_options()).clone();
        let connect = || PgConnection::connect_with(&options);
        let server = acquire_single_writer_lock_on(connect().await.unwrap())
            .await
            .unwrap();
        let cli = acquire_writer_lock_on(connect().await.unwrap(), CLI_WRITER_LOCK_KEY)
            .await
            .unwrap();
        assert!(matches!(
            acquire_writer_lock_on(connect().await.unwrap(), CLI_WRITER_LOCK_KEY).await,
            Err(DbError::CliWriterHeld)
        ));
        assert!(matches!(
            acquire_single_writer_lock_on(connect().await.unwrap()).await,
            Err(DbError::SingleWriterHeld)
        ));
        cli.release().await.unwrap();
        let successor = acquire_writer_lock_on(connect().await.unwrap(), CLI_WRITER_LOCK_KEY)
            .await
            .unwrap();
        assert!(matches!(
            acquire_single_writer_lock_on(connect().await.unwrap()).await,
            Err(DbError::SingleWriterHeld)
        ));
        successor.release().await.unwrap();
        server.release().await.unwrap();
    }

    /// A second writer is refused while the first holds the lock, and the lock
    /// becomes available again once the first releases it.
    #[sqlx::test(migrations = "./migrations")]
    async fn refuses_a_second_writer_and_frees_on_release(pool: PgPool) {
        let options = (*pool.connect_options()).clone();
        let connect = || {
            let options = options.clone();
            async move { PgConnection::connect_with(&options).await.unwrap() }
        };

        let first = acquire_single_writer_lock_on(connect().await)
            .await
            .expect("the first writer acquires the single-writer lock");

        // A second process, on its own session, cannot take the lock.
        let refused = acquire_single_writer_lock_on(connect().await).await;
        assert!(
            matches!(refused, Err(DbError::SingleWriterHeld)),
            "a second writer must be refused while the first holds the lock",
        );

        // Releasing the first hands the lock to the next writer.
        first.release().await.unwrap();
        let second = acquire_single_writer_lock_on(connect().await)
            .await
            .expect("the lock is available once the first writer releases it");
        second.release().await.unwrap();
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn monitor_reports_lost_session_without_reacquiring(pool: PgPool) {
        let options = (*pool.connect_options()).clone();
        let mut conn = PgConnection::connect_with(&options).await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        let mut first = acquire_single_writer_lock_on(conn).await.unwrap();
        let stopped: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1, 1000)")
            .bind(pid)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(stopped);
        let error = tokio::time::timeout(std::time::Duration::from_secs(6), first.monitor())
            .await
            .expect("loss detection must be bounded");
        assert!(error.is_err());
        // The old guard never reconnects and steals the successor's lock.
        let second =
            acquire_single_writer_lock_on(PgConnection::connect_with(&options).await.unwrap())
                .await
                .unwrap();
        assert!(first.monitor().await.is_err());
        assert!(matches!(
            acquire_single_writer_lock_on(PgConnection::connect_with(&options).await.unwrap())
                .await,
            Err(DbError::SingleWriterHeld)
        ));
        second.release().await.unwrap();
    }
}
