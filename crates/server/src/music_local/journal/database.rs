//! Typed durable filesystem intent; JSON is only its Rust/API serialization.
use super::Intent;
use crate::music_local::fs::Fingerprint;
#[derive(sqlx::Type)]
#[sqlx(type_name = "music_mount_identity")]
struct Identity {
    device: String,
    inode: String,
}
impl From<(u64, u64)> for Identity {
    fn from((device, inode): (u64, u64)) -> Self {
        Self {
            device: device.to_string(),
            inode: inode.to_string(),
        }
    }
}
impl Identity {
    fn value(self) -> Result<(u64, u64), std::num::ParseIntError> {
        Ok((self.device.parse()?, self.inode.parse()?))
    }
}
#[derive(sqlx::Type)]
#[sqlx(type_name = "music_file_fingerprint")]
struct FileFingerprint {
    device: String,
    inode: String,
    bytes: String,
    modified_seconds: i64,
    modified_nanos: i64,
}
impl From<Fingerprint> for FileFingerprint {
    fn from(value: Fingerprint) -> Self {
        Self {
            device: value.device.to_string(),
            inode: value.inode.to_string(),
            bytes: value.bytes.to_string(),
            modified_seconds: value.modified_seconds,
            modified_nanos: value.modified_nanos,
        }
    }
}
impl FileFingerprint {
    fn value(self) -> Result<Fingerprint, std::num::ParseIntError> {
        Ok(Fingerprint {
            device: self.device.parse()?,
            inode: self.inode.parse()?,
            bytes: self.bytes.parse()?,
            modified_seconds: self.modified_seconds,
            modified_nanos: self.modified_nanos,
        })
    }
}
#[derive(sqlx::Type)]
#[sqlx(type_name = "music_local_mutation_intent")]
pub(super) struct IntentRecord {
    root_id: String,
    mount: Identity,
    parent: Identity,
    source: String,
    target: String,
    before: FileFingerprint,
    after: Option<FileFingerprint>,
    operation: String,
}
impl From<&Intent> for IntentRecord {
    fn from(value: &Intent) -> Self {
        Self {
            root_id: value.root_id.clone(),
            mount: value.mount.into(),
            parent: value.parent.into(),
            source: value.source.clone(),
            target: value.target.clone(),
            before: value.before.into(),
            after: value.after.map(Into::into),
            operation: value.operation.clone(),
        }
    }
}
impl TryFrom<IntentRecord> for sqlx::types::Json<Intent> {
    type Error = std::num::ParseIntError;
    fn try_from(value: IntentRecord) -> Result<Self, Self::Error> {
        Ok(Self(Intent {
            root_id: value.root_id,
            mount: value.mount.value()?,
            parent: value.parent.value()?,
            source: value.source,
            target: value.target,
            before: value.before.value()?,
            after: value.after.map(FileFingerprint::value).transpose()?,
            operation: value.operation,
        }))
    }
}
