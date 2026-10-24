//! Notification tests share one default binary; individual targets remain selectable.

#[path = "notification_delivery.rs"]
mod notification_delivery;
#[path = "notification_filtering.rs"]
mod notification_filtering;
#[path = "notification_inbox.rs"]
mod notification_inbox;
#[path = "notification_retention.rs"]
mod notification_retention;
#[path = "notifications_v2.rs"]
mod notifications_v2;
