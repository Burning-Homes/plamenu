//! Operator-approved local music originals remain references to mounted files.
pub mod config;
#[cfg(unix)]
pub mod fs;
pub mod options;

#[cfg(unix)]
pub mod folders;
#[cfg(unix)]
pub mod worker;

#[cfg(unix)]
mod cover;
#[cfg(unix)]
pub mod files;
#[cfg(unix)]
pub mod journal;
pub(crate) mod playback;
#[cfg(unix)]
mod sync;
#[cfg(unix)]
mod tags;

#[cfg(unix)]
// Serde serialize_with callbacks require the borrowed field signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn serialize_id<S: serde::Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}
#[cfg(unix)]
// Serde serialize_with callbacks receive a reference to the Option field.
#[allow(clippy::ref_option)]
fn serialize_optional_id<S: serde::Serializer>(
    value: &Option<i64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => serializer.serialize_some(&value.to_string()),
        None => serializer.serialize_none(),
    }
}
