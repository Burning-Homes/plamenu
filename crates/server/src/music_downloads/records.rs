//! Typed item diagnostics are serialized only at the API boundary.
use crate::error::ApiError;
#[derive(Clone, serde::Serialize, serde::Deserialize, sqlx::Type)]
#[sqlx(type_name = "music_download_item_error")]
struct ItemError {
    index: i32,
    message: String,
}
#[derive(Clone, sqlx::Type)]
#[sqlx(transparent, no_pg_array)]
pub(super) struct Errors(Vec<ItemError>);
impl Errors {
    pub(super) fn from_value(value: serde_json::Value) -> Result<Self, ApiError> {
        serde_json::from_value(value)
            .map(Self)
            .map_err(|error| ApiError::Internal(Box::new(error)))
    }
}
impl From<Errors> for serde_json::Value {
    fn from(value: Errors) -> Self {
        serde_json::to_value(value.0).expect("Typed bounded download diagnostics")
    }
}
