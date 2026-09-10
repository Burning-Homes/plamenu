//! Raw first-party web-client settings (`/api/web/settings`).

use serde_json::Value;
use sqlx::PgPool;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WebSetting {
    pub id: i64,
    pub user_id: i64,
    pub data: Value,
}

/// Built-in client's live-notification behavior. Kept inside the raw web
/// settings document so third-party API preferences and first-party UI knobs
/// share Mastodon's one per-user settings row without adding account columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationPreferences {
    pub live_updates: bool,
    pub sound: bool,
    pub volume: u8,
}

impl Default for NotificationPreferences {
    fn default() -> Self {
        Self {
            // Live updates currently disrupt navigation context in the web client.
            live_updates: false,
            sound: true,
            volume: 20,
        }
    }
}

impl NotificationPreferences {
    fn from_data(data: &Value) -> Self {
        let defaults = Self::default();
        let plamenu = data.get("plamenu").and_then(Value::as_object);
        Self {
            live_updates: plamenu
                .and_then(|settings| settings.get("live_notifications"))
                .and_then(Value::as_bool)
                .unwrap_or(defaults.live_updates),
            sound: plamenu
                .and_then(|settings| settings.get("notification_sound"))
                .and_then(Value::as_bool)
                .unwrap_or(defaults.sound),
            volume: plamenu
                .and_then(|settings| settings.get("notification_volume"))
                .and_then(Value::as_u64)
                .and_then(|volume| u8::try_from(volume.min(100)).ok())
                .unwrap_or(defaults.volume),
        }
    }
}

/// Reads the built-in client's notification preferences, applying defaults
/// when the generic web settings row or its Plamenu namespace is absent.
pub async fn notification_preferences(
    pool: &PgPool,
    user_id: i64,
) -> Result<NotificationPreferences, DbError> {
    let data = sqlx::query_scalar::<_, Value>("SELECT data FROM web_settings WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(data.as_ref().map_or_else(
        NotificationPreferences::default,
        NotificationPreferences::from_data,
    ))
}

/// Updates only Plamenu's live-notification namespace, preserving every raw
/// setting a Mastodon-compatible web client may have stored in the same row.
pub async fn update_notification_preferences(
    pool: &PgPool,
    user_id: i64,
    preferences: NotificationPreferences,
) -> Result<WebSetting, DbError> {
    let patch = serde_json::json!({
        "live_notifications": preferences.live_updates,
        "notification_sound": preferences.sound,
        "notification_volume": preferences.volume,
    });
    let setting = sqlx::query_as::<_, WebSetting>(
        r"
        INSERT INTO web_settings (id, user_id, data)
        VALUES ($1, $2, jsonb_build_object('plamenu', $3::jsonb))
        ON CONFLICT (user_id) DO UPDATE
        SET data = jsonb_set(
                web_settings.data,
                '{plamenu}',
                COALESCE(web_settings.data->'plamenu', '{}'::jsonb)
                    || EXCLUDED.data->'plamenu',
                true
            ),
            updated_at = now()
        RETURNING id, user_id, data
        ",
    )
    .bind(id::next())
    .bind(user_id)
    .bind(patch)
    .fetch_one(pool)
    .await?;
    Ok(setting)
}

/// Creates or replaces the user's raw web settings JSON. Mastodon stores this
/// as one row per user in `web_settings`; the JS client sends the full state
/// blob each time, so replacement is intentional.
pub async fn upsert(pool: &PgPool, user_id: i64, data: &Value) -> Result<WebSetting, DbError> {
    let setting = sqlx::query_as!(
        WebSetting,
        r#"
        INSERT INTO web_settings (id, user_id, data)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id) DO UPDATE
        SET data = EXCLUDED.data,
            updated_at = now()
        RETURNING id, user_id, data
        "#,
        id::next(),
        user_id,
        data,
    )
    .fetch_one(pool)
    .await?;
    Ok(setting)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_notifications_require_an_explicit_preference() {
        for data in [serde_json::json!({}), serde_json::json!({"theme": "dark"})] {
            assert!(!NotificationPreferences::from_data(&data).live_updates);
        }
        let enabled = NotificationPreferences::from_data(&serde_json::json!({
            "plamenu": {"live_notifications": true}
        }));
        assert!(enabled.live_updates);
        assert!(enabled.sound);
        assert_eq!(enabled.volume, 20);
    }
}
