//! Native UTC notification schedules and seen-only retention.
use crate::config::ConfigError;
use serde::Deserialize;
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    pub retention_days: i32,
    pub purge_hour: u8,
    pub digest_hour: u8,
}
impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            retention_days: 90,
            purge_hour: 3,
            digest_hour: 8,
        }
    }
}
impl NotificationsConfig {
    pub fn validate(self) -> Result<Self, ConfigError> {
        if !(1..=365_000).contains(&self.retention_days) {
            return Err(ConfigError::Invalid(
                "notifications.retention_days",
                "must be between 1 and 365000 days".into(),
            ));
        }
        if self.purge_hour > 23 || self.digest_hour > 23 {
            return Err(ConfigError::Invalid(
                "notifications",
                "purge_hour and digest_hour must be UTC hours 0–23".into(),
            ));
        }
        Ok(self)
    }
}
