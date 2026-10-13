//! Operators approve mount roots and each destructive filesystem capability.
use crate::config::ConfigError;
use serde::Deserialize;
use std::path::PathBuf;
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub id: String,
    pub label: String,
    pub path: PathBuf,
    #[serde(default)]
    pub allow_tag_writeback: bool,
    #[serde(default)]
    pub allow_rename: bool,
    #[serde(default)]
    pub allow_delete: bool,
}
impl std::fmt::Debug for RootConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MusicRoot")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("path", &"[PRIVATE]")
            .field("allow_tag_writeback", &self.allow_tag_writeback)
            .field("allow_rename", &self.allow_rename)
            .field("allow_delete", &self.allow_delete)
            .finish()
    }
}
pub(crate) fn validate(roots: Vec<RootConfig>) -> Result<Vec<RootConfig>, ConfigError> {
    let mut ids = std::collections::HashSet::new();
    if roots.len() > 32 {
        return Err(ConfigError::Invalid(
            "music_local_roots",
            "Configure up to 32 music roots".into(),
        ));
    }
    for root in &roots {
        if root.id.is_empty()
            || root.id.len() > 64
            || !root
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            || !ids.insert(&root.id)
            || root.label.trim().is_empty()
            || root.label.chars().count() > 100
            || root.label.contains('\0')
            || !root.path.is_absolute()
            || root.path.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(ConfigError::Invalid(
                "music_local_roots",
                "Music roots require unique ids, labels and absolute operator paths".into(),
            ));
        }
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> RootConfig {
        RootConfig {
            id: "archive".into(),
            label: "Archive".into(),
            path: PathBuf::from("/srv/private-music"),
            allow_tag_writeback: false,
            allow_rename: false,
            allow_delete: false,
        }
    }
    #[test]
    fn roots_are_operator_paths_with_unique_names_and_private_debug_output() {
        let entry = root();
        assert!(validate(vec![entry.clone()]).is_ok());
        assert!(!format!("{entry:?}").contains("/srv/private-music"));
        assert!(validate(vec![entry.clone(), entry]).is_err());
        let mut entry = root();
        entry.path = PathBuf::from("relative");
        assert!(validate(vec![entry]).is_err());
        let mut entry = root();
        entry.id = "../music".into();
        assert!(validate(vec![entry]).is_err());
    }
}
