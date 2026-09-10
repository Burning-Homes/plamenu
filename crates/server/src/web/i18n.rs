//! Interface localization for the server-rendered web client.
//!
//! Fluent resources are embedded in the binary: deployments never need to
//! keep catalog files in sync with the executable. English is the source and
//! final fallback locale. A request chooses its locale from the signed-in
//! user's stored preference first, then `Accept-Language`, then English.

use std::borrow::Cow;
use std::future::Future;
use std::sync::LazyLock;

use axum::extract::FromRequestParts;
use axum::http::header;
use axum::http::request::Parts;
use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use maud::{Markup, PreEscaped};
use unic_langid::LanguageIdentifier;

use crate::state::AppState;

const EN_SOURCE: &str = include_str!("locales/en-US/main.ftl");
const RU_SOURCE: &str = include_str!("locales/ru/main.ftl");

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Language {
    #[default]
    English,
    Russian,
}

/// A negotiated interface locale. It is cheap to copy and can be carried by
/// extractors and view models without lifetimes or per-request bundle work.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Locale(Language);

impl Locale {
    /// The interface languages that actually have a catalog compiled in, as
    /// codes from the posting-language inventory. The interface-language
    /// picker offers exactly these: any other code negotiates back to English,
    /// so listing the full 200-entry inventory there would promise
    /// translations that do not exist. Adding a catalog means adding its code
    /// here and a variant to [`Language`].
    pub const AVAILABLE: &'static [&'static str] = &["en", "ru"];

    /// Resolves a signed-in preference, falling back to the request header.
    #[must_use]
    pub fn negotiate(preference: Option<&str>, accept_language: Option<&str>) -> Self {
        preference
            .and_then(language_from_tag)
            .or_else(|| accept_language.and_then(language_from_header))
            .map_or_else(Self::default, Self)
    }

    /// The locale a request with no signed-in preference asks for. Handlers
    /// that already hold the headers (sessionless sign-in and consent legs)
    /// use this instead of taking the extractor a second time.
    #[must_use]
    pub fn from_headers(headers: &axum::http::HeaderMap) -> Self {
        Self::negotiate(
            None,
            headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|value| value.to_str().ok()),
        )
    }

    /// The locale a message *to* `user_id` is written in: their stored
    /// interface preference, English when they have none. Mail and other
    /// out-of-band messages are read long after (and often far from) the
    /// request that triggered them — an admin-initiated reset is read by the
    /// account owner — so a request header would be the wrong signal.
    ///
    /// # Errors
    /// Propagates the lookup failure.
    pub async fn for_user(
        pool: &plamenu_db::PgPool,
        user_id: i64,
    ) -> Result<Self, plamenu_db::DbError> {
        let stored = plamenu_db::user::locale(pool, user_id).await?;
        Ok(Self::negotiate(stored.as_deref(), None))
    }

    /// The canonical BCP-47 tag emitted in HTML.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self.0 {
            Language::English => "en-US",
            Language::Russian => "ru",
        }
    }

    /// The HTML writing direction. Kept on the locale abstraction so adding
    /// the first RTL catalog does not require another layout migration.
    #[must_use]
    pub fn direction(self) -> &'static str {
        match self.0 {
            Language::English | Language::Russian => "ltr",
        }
    }

    /// Formats a catalog message without variables.
    #[must_use]
    pub fn text(self, id: &str) -> String {
        self.format(id, None)
    }

    /// Formats a catalog message with Fluent variables.
    #[must_use]
    pub fn text_with(self, id: &str, args: &FluentArgs<'_>) -> String {
        self.format(id, Some(args))
    }

    /// Formats a catalog message bound for a plain-text destination: an e-mail
    /// body, or a `data-*` attribute a script writes into a status line.
    /// Fluent brackets interpolated values in directional isolates, which are
    /// invisible inside rendered markup but read as stray characters
    /// everywhere else, so they are stripped here.
    #[must_use]
    pub fn plain(self, id: &str) -> String {
        strip_isolates(&self.format(id, None))
    }

    /// [`Locale::plain`] with Fluent variables.
    #[must_use]
    pub fn plain_with(self, id: &str, args: &FluentArgs<'_>) -> String {
        strip_isolates(&self.format(id, Some(args)))
    }

    /// Formats a catalog message that interpolates pre-rendered markup — an
    /// in-sentence link, typically — so the whole sentence stays one
    /// translatable unit instead of being assembled from fragments here.
    /// Values are `Markup`, which is maud's proof that they are already
    /// escaped; never hand this raw user input.
    #[must_use]
    pub fn markup(self, id: &str, parts: &[(&str, Markup)]) -> Markup {
        self.markup_with(id, &FluentArgs::new(), parts)
    }

    /// [`Locale::markup`] with additional ordinary Fluent variables alongside
    /// the markup parts — for a sentence whose plural form is selected by a
    /// number while the displayed value is pre-styled markup.
    #[must_use]
    pub fn markup_with(self, id: &str, args: &FluentArgs<'_>, parts: &[(&str, Markup)]) -> Markup {
        let mut merged = FluentArgs::new();
        for (name, value) in args.iter() {
            merged.set(name.to_owned(), value.clone());
        }
        for (name, value) in parts {
            merged.set(*name, value.0.as_str());
        }
        // Fluent brackets interpolated values in directional isolates. They
        // are harmless in text but would land inside the markup, so drop them:
        // the interpolated elements already delimit themselves.
        PreEscaped(strip_isolates(&self.text_with(id, &merged)))
    }

    fn format(self, id: &str, args: Option<&FluentArgs<'_>>) -> String {
        let preferred = bundle(self.0);
        if let Some(value) = message(preferred, id, args) {
            return value;
        }
        // A partially translated catalog remains usable while translators
        // work: missing Russian keys fall back individually to source English.
        if self.0 != Language::English
            && let Some(value) = message(&EN_BUNDLE, id, args)
        {
            tracing::warn!(locale = self.tag(), message_id = id, "missing translation");
            return value;
        }
        tracing::error!(
            locale = self.tag(),
            message_id = id,
            "unknown localization message"
        );
        id.to_owned()
    }
}

impl FromRequestParts<AppState> for Locale {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let accepted = parts
            .headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok());
        std::future::ready(Ok(Self::negotiate(None, accepted)))
    }
}

/// Drops Fluent's directional isolates (U+2068/U+2069), which bracket every
/// interpolated value.
fn strip_isolates(text: &str) -> String {
    text.replace(['\u{2068}', '\u{2069}'], "")
}

type Bundle = FluentBundle<FluentResource>;

static EN_BUNDLE: LazyLock<Bundle> = LazyLock::new(|| build_bundle("en-US", EN_SOURCE));
static RU_BUNDLE: LazyLock<Bundle> = LazyLock::new(|| build_bundle("ru", RU_SOURCE));

fn bundle(language: Language) -> &'static Bundle {
    match language {
        Language::English => &EN_BUNDLE,
        Language::Russian => &RU_BUNDLE,
    }
}

fn build_bundle(tag: &str, source: &str) -> Bundle {
    let language: LanguageIdentifier = tag.parse().expect("embedded locale tag is valid");
    let resource = FluentResource::try_new(source.to_owned())
        .unwrap_or_else(|(_, errors)| panic!("invalid embedded {tag} Fluent catalog: {errors:?}"));
    let mut bundle = Bundle::new_concurrent(vec![language]);
    bundle.add_resource(resource).unwrap_or_else(|errors| {
        panic!("duplicate messages in embedded {tag} catalog: {errors:?}")
    });
    bundle
}

fn message(bundle: &'static Bundle, id: &str, args: Option<&FluentArgs<'_>>) -> Option<String> {
    let pattern = bundle.get_message(id)?.value()?;
    let mut errors = Vec::new();
    let value: Cow<'_, str> = bundle.format_pattern(pattern, args, &mut errors);
    if !errors.is_empty() {
        tracing::error!(message_id = id, ?errors, "localization formatting failed");
    }
    Some(value.into_owned())
}

fn language_from_tag(tag: &str) -> Option<Language> {
    let parsed: LanguageIdentifier = tag.trim().replace('_', "-").parse().ok()?;
    match parsed.language.as_str() {
        "ru" => Some(Language::Russian),
        "en" => Some(Language::English),
        _ => None,
    }
}

/// Parses weighted `Accept-Language` choices. Invalid entries and `q=0`
/// exclusions are ignored; ties retain header order.
fn language_from_header(header: &str) -> Option<Language> {
    let mut choices: Vec<(usize, f32, &str)> = header
        .split(',')
        .enumerate()
        .filter_map(|(order, item)| {
            let mut parts = item.trim().split(';');
            let tag = parts.next()?.trim();
            let quality = match parts.find_map(|part| part.trim().strip_prefix("q=")) {
                Some(value) => value.parse::<f32>().ok()?,
                None => 1.0,
            };
            (quality > 0.0 && quality <= 1.0).then_some((order, quality, tag))
        })
        .collect();
    choices.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    choices
        .into_iter()
        .find_map(|(_, _, tag)| language_from_tag(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_wins_and_regional_russian_matches() {
        assert_eq!(
            Locale::negotiate(Some("en"), Some("ru-RU, en;q=0.5")).tag(),
            "en-US"
        );
        assert_eq!(Locale::negotiate(None, Some("ru-RU")).tag(), "ru");
    }

    #[test]
    fn accept_language_honours_quality_and_exclusions() {
        assert_eq!(
            Locale::negotiate(None, Some("en;q=0.4, ru;q=0.9")).tag(),
            "ru"
        );
        assert_eq!(
            Locale::negotiate(None, Some("ru;q=0, fr;q=1")).tag(),
            "en-US"
        );
        assert_eq!(
            Locale::negotiate(None, Some("ru;q=invalid, en;q=0.5")).tag(),
            "en-US"
        );
    }

    #[test]
    fn every_offered_interface_language_has_a_catalog() {
        // The picker renders these through the posting-language inventory, and
        // offering one that negotiates back to English would be a lie.
        for code in Locale::AVAILABLE {
            assert!(
                crate::languages::find(code).is_some(),
                "{code} is not in the language inventory"
            );
            let negotiated = Locale::negotiate(Some(code), None);
            assert_eq!(
                negotiated.tag().split('-').next(),
                Some(*code),
                "{code} does not negotiate to its own catalog"
            );
        }
    }

    #[test]
    fn russian_plural_rules_format_minimum_age() {
        let locale = Locale::negotiate(Some("ru"), None);
        for (years, ending) in [(1, "1 года"), (2, "2 лет"), (5, "5 лет")] {
            let mut args = FluentArgs::new();
            args.set("years", years);
            let rendered = locale
                .text_with("signup-minimum-age", &args)
                .replace(['\u{2068}', '\u{2069}'], "");
            assert!(
                rendered.contains(ending),
                "expected {ending:?} in {rendered:?}"
            );
        }
    }

    #[test]
    fn russian_catalog_has_every_source_message() {
        fn message_ids(source: &str) -> std::collections::BTreeSet<&str> {
            source
                .lines()
                .filter(|line| !line.starts_with(char::is_whitespace))
                .filter_map(|line| line.split_once('=').map(|(id, _)| id.trim()))
                .filter(|id| !id.is_empty() && !id.starts_with('-'))
                .collect()
        }

        // Parsing both resources separately catches syntax failures; comparing
        // top-level identifiers catches omissions without relying on
        // fluent-syntax's internal AST types.
        FluentResource::try_new(EN_SOURCE.to_owned()).unwrap();
        FluentResource::try_new(RU_SOURCE.to_owned()).unwrap();
        let source_ids = message_ids(EN_SOURCE);
        let translated_ids = message_ids(RU_SOURCE);
        assert_eq!(source_ids, translated_ids);
    }
}
