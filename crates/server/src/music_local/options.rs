//! Bounded scan selection applies to root-relative paths and basenames.
use crate::error::ApiError;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Deserialize, Serialize, sqlx::Type)]
#[serde(default, deny_unknown_fields)]
#[sqlx(type_name = "music_scan_options")]
pub struct Options {
    pub recursive: bool,
    pub extensions: Vec<String>,
    pub exclude: Vec<String>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            recursive: true,
            extensions: ["mp3", "flac", "ogg", "opus", "m4a", "aac", "wav"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            exclude: Vec::new(),
        }
    }
}
impl Options {
    pub(crate) fn validate(&self) -> Result<(), ApiError> {
        if self.extensions.len() > 11
            || self.exclude.len() > 32
            || self.extensions.iter().any(|extension| {
                !matches!(
                    extension
                        .trim_start_matches('.')
                        .to_ascii_lowercase()
                        .as_str(),
                    "mp3" | "flac" | "ogg" | "opus" | "m4a" | "aac" | "wav" | "mka" | "wma"
                )
            })
            || self
                .exclude
                .iter()
                .any(|pattern| pattern.len() > 255 || pattern.contains('\0'))
        {
            return Err(ApiError::Unprocessable(
                "Invalid music scan extensions or exclusions".into(),
            ));
        }
        self.selection().map(|_| ())
    }
    pub(crate) fn selection(&self) -> Result<Selection, ApiError> {
        let patterns = self
            .exclude
            .iter()
            .map(|pattern| glob(pattern))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Selection {
            recursive: self.recursive,
            extensions: self
                .extensions
                .iter()
                .map(|extension| extension.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
            patterns,
        })
    }
}
pub(crate) struct Selection {
    pub recursive: bool,
    extensions: std::collections::HashSet<String>,
    patterns: Vec<regex::Regex>,
}
impl Selection {
    pub fn excludes(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        self.patterns
            .iter()
            .any(|pattern| pattern.is_match(path) || pattern.is_match(name))
    }
    pub fn audio(&self, name: &str) -> bool {
        std::path::Path::new(name)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| self.extensions.contains(&extension.to_ascii_lowercase()))
    }
}
fn glob(pattern: &str) -> Result<regex::Regex, ApiError> {
    let mut expression = String::from("(?s)\\A");
    let mut chars = pattern.chars().peekable();
    while let Some(value) = chars.next() {
        match value {
            '*' => expression.push_str(".*"),
            '?' => expression.push('.'),
            '[' => {
                let mut class = String::new();
                let negate = chars.next_if_eq(&'!').is_some();
                let mut closed = false;
                for character in chars.by_ref() {
                    if character == ']' && !class.is_empty() {
                        closed = true;
                        break;
                    }
                    if character == '-' && !class.is_empty() {
                        class.push('-');
                    } else {
                        class.push_str(&regex::escape(&character.to_string()));
                    }
                }
                if !closed {
                    return Err(ApiError::Unprocessable(
                        "Unclosed music exclusion pattern".into(),
                    ));
                }
                expression.push('[');
                if negate {
                    expression.push('^');
                }
                expression.push_str(&class);
                expression.push(']');
            }
            value => expression.push_str(&regex::escape(&value.to_string())),
        }
    }
    expression.push_str("\\z");
    regex::RegexBuilder::new(&expression)
        .size_limit(65536)
        .build()
        .map_err(|_| ApiError::Unprocessable("Invalid music exclusion pattern".into()))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusions_match_whole_paths_or_basenames_without_interpreting_regex() {
        let options = Options {
            exclude: vec![
                "skip/*".into(),
                "demo?.[mw][pa][3v]".into(),
                "literal.(mp3)".into(),
            ],
            ..Default::default()
        };
        let selected = options.selection().unwrap();
        assert!(selected.excludes("skip/nested/recording.wav"));
        assert!(selected.excludes("album/demo1.mp3"));
        assert!(selected.excludes("album/literal.(mp3)"));
        assert!(!selected.excludes("album/recording.mp3"));
        assert!(selected.audio("MUSIC.FLAC"));
        assert!(!selected.audio("unsafe.m3u8"));
        assert!(!selected.audio("image.png"));
    }
    #[test]
    fn scan_choices_are_bounded_and_do_not_accept_manifest_extensions_or_invalid_patterns() {
        let mut options = Options::default();
        assert!(options.validate().is_ok());
        options.extensions.push("m3u8".into());
        assert!(options.validate().is_err());
        options.extensions.clear();
        options.exclude = vec!["[".into()];
        assert!(options.validate().is_err());
        options.exclude = vec!["*".repeat(256)];
        assert!(options.validate().is_err());
    }
}
