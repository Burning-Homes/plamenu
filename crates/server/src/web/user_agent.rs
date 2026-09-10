//! A tiny User-Agent describer for the security-settings pages. Mastodon leans
//! on the `browser` gem for "Chrome on Windows"-style labels; we only need a
//! recognizable summary of a session, so a handful of substring checks (newest
//! tokens first, so order matters where families overlap) beats a dependency.

use fluent_bundle::FluentArgs;

use super::i18n::Locale;

/// Human label for a stored User-Agent, e.g. `"Firefox on Android"`. Falls back
/// to the catalog's "unknown browser" wording when the string is missing or
/// unrecognized, and to a bare browser/platform when only one half is known.
/// Browser and platform names are product names and stay untranslated; the
/// phrase joining them comes from the catalog.
pub(crate) fn describe(ua: Option<&str>, locale: Locale) -> String {
    let unknown = || locale.text("ua-unknown-browser");
    let joined = |browser: &str, platform: &str| {
        let mut args = FluentArgs::new();
        args.set("browser", browser);
        args.set("platform", platform);
        locale.text_with("ua-browser-on-platform", &args)
    };
    let ua = ua.map(str::trim).filter(|s| !s.is_empty());
    let Some(ua) = ua else {
        return unknown();
    };
    match (browser(ua), platform(ua)) {
        (Some(b), Some(p)) => joined(b, p),
        (Some(b), None) => b.to_owned(),
        (None, Some(p)) => joined(&unknown(), p),
        (None, None) => unknown(),
    }
}

fn browser(ua: &str) -> Option<&'static str> {
    // Order matters: Edge/Opera/Chrome all carry the "Chrome" token, and most
    // WebKit browsers carry "Safari", so check the specific families first.
    if ua.contains("Edg") {
        Some("Microsoft Edge")
    } else if ua.contains("OPR") || ua.contains("Opera") {
        Some("Opera")
    } else if ua.contains("Firefox") || ua.contains("FxiOS") {
        Some("Firefox")
    } else if ua.contains("Chrome") || ua.contains("CriOS") {
        Some("Chrome")
    } else if ua.contains("Safari") {
        Some("Safari")
    } else if ua.contains("Mastodon") || ua.contains("Tusky") || ua.contains("Ivory") {
        Some("Mastodon app")
    } else if ua.contains("curl") {
        Some("curl")
    } else {
        None
    }
}

fn platform(ua: &str) -> Option<&'static str> {
    // iOS before macOS: iPhone/iPad UAs also say "Mac OS X".
    if ua.contains("Android") {
        Some("Android")
    } else if ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iOS") {
        Some("iOS")
    } else if ua.contains("Windows") {
        Some("Windows")
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        Some("macOS")
    } else if ua.contains("Linux") {
        Some("Linux")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Locale, describe};

    /// Fluent brackets interpolated values in invisible bidi isolates; the
    /// assertions here read the visible text.
    fn label(ua: Option<&str>) -> String {
        describe(ua, Locale::default()).replace(['\u{2068}', '\u{2069}'], "")
    }

    #[test]
    fn recognizes_common_agents() {
        assert_eq!(
            label(Some(
                "Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0"
            )),
            "Firefox on Linux"
        );
        assert_eq!(
            label(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/125.0 Safari/537.36 Edg/125.0"
            )),
            "Microsoft Edge on Windows"
        );
        assert_eq!(
            label(Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
                 AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1"
            )),
            "Safari on iOS"
        );
    }

    #[test]
    fn falls_back_gracefully() {
        assert_eq!(label(None), "Unknown browser");
        assert_eq!(label(Some("")), "Unknown browser");
        assert_eq!(label(Some("something weird")), "Unknown browser");
    }

    #[test]
    fn russian_joins_browser_and_platform_without_english_glue() {
        let label = describe(
            Some("Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0"),
            Locale::negotiate(Some("ru"), None),
        )
        .replace(['\u{2068}', '\u{2069}'], "");
        assert_eq!(label, "Firefox, Linux");
    }
}
