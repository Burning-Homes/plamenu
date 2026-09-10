//! Original FEP-c390 statements. Callers verify before storing them.

use crate::DbError;
use serde_json::Value;
use sqlx::PgExecutor;

pub async fn list<'e>(
    executor: impl PgExecutor<'e>,
    account_id: i64,
) -> Result<Vec<Value>, DbError> {
    let documents = sqlx::query_scalar!(
        "SELECT documents FROM account_identity_proofs WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(documents
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default())
}

pub async fn replace<'e>(
    executor: impl PgExecutor<'e>,
    account_id: i64,
    documents: &[Value],
) -> Result<(), DbError> {
    if documents.is_empty() {
        return clear(executor, account_id).await;
    }
    let value = Value::Array(documents.to_vec());
    sqlx::query!(
        "INSERT INTO account_identity_proofs (account_id, documents) VALUES ($1, $2)
         ON CONFLICT (account_id) DO UPDATE SET documents = EXCLUDED.documents",
        account_id,
        value,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Serialize local mutations with account deletion and profile changes.
pub async fn lock_local<'e>(
    executor: impl PgExecutor<'e>,
    account_id: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE id = $1 AND domain IS NULL
         AND suspended_at IS NULL AND deleted_at IS NULL FOR UPDATE",
        account_id,
    )
    .fetch_optional(executor)
    .await?
    .is_some())
}

pub async fn clear<'e>(executor: impl PgExecutor<'e>, account_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM account_identity_proofs WHERE account_id = $1",
        account_id
    )
    .execute(executor)
    .await?;
    Ok(())
}
