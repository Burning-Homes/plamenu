use serde_json::{Map, Value};
use sqlx::PgPool;

use crate::DbError;

pub async fn preferences(pool: &PgPool, user_id: i64) -> Result<Map<String, Value>, DbError> {
    let value: Option<Value> =
        sqlx::query_scalar("SELECT preferences FROM lemmy_user_preferences WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(value
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default())
}

/// Merge supplied fields, matching Lemmy's optional PATCH-like settings form.
pub async fn merge_preferences(
    pool: &PgPool,
    user_id: i64,
    preferences: &Map<String, Value>,
) -> Result<(), DbError> {
    sqlx::query(
        r"
        INSERT INTO lemmy_user_preferences (user_id, preferences)
        VALUES ($1, $2)
        ON CONFLICT (user_id) DO UPDATE
        SET preferences = lemmy_user_preferences.preferences || EXCLUDED.preferences,
            updated_at = now()
        ",
    )
    .bind(user_id)
    .bind(Value::Object(preferences.clone()))
    .execute(pool)
    .await?;
    Ok(())
}
