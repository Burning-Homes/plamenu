//! The base HTML shell every web page is rendered into.
//!
//! One function — `shell` — wraps page-specific `content` in the document
//! head, the app chrome (a sidebar on desktop, a bottom tab bar on mobile) and
//! the asset links. Keeping it the single entry point means the chrome, the
//! stylesheet version and the responsive structure live in exactly one place.

use maud::{DOCTYPE, Markup, PreEscaped, html};

use super::assets::ASSET_VERSION;
use super::i18n::Locale;
use super::session::WebUser;
use super::view::icon;

/// The `og:image` of a page: an absolute URL plus the optional dimensions and
/// alt text preview cards can use before (or instead of) fetching the file.
pub struct MetaImage {
    pub url: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// `og:image:alt` — the attachment's description, when it has one.
    pub alt: Option<String>,
}

impl MetaImage {
    /// An avatar rendition: the processing pipeline caps avatars at 400×400
    /// (`AVATAR_MAX_EDGE`), the same fixed size Mastodon advertises.
    pub fn avatar(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            width: Some(400),
            height: Some(400),
            alt: None,
        }
    }
}

/// Machine-readable metadata a page hands to its shell: the Open Graph tags
/// link previews are built from, the canonical URL and the robots directive.
/// The default is an empty slot — chrome pages (settings, notifications)
/// render no preview block at all, like Mastodon's non-public routes.
#[derive(Default)]
pub struct PageMeta {
    /// `og:title`. Rendering of the whole Open Graph block is keyed on this:
    /// no title, no `og:*`/`twitter:card` tags.
    pub og_title: Option<String>,
    /// `og:type` — `profile` for accounts, `article` for statuses, `website`
    /// for instance-level pages.
    pub og_kind: Option<&'static str>,
    /// `meta name=description` + `og:description`.
    pub description: Option<String>,
    /// `rel=canonical` + `og:url`. For remote content this is the origin
    /// page, so search engines index the author's server, not our mirror.
    pub canonical: Option<String>,
    /// `og:site_name` — the operator's site title.
    pub site_name: Option<String>,
    /// `og:image` (+ dimensions/alt).
    pub image: Option<MetaImage>,
    /// `og:published_time` — the focused status' creation time.
    pub published_time: Option<String>,
    /// `og:locale` — the focused status' language.
    pub locale: Option<String>,
    /// `profile:username` — the subject's full `user@domain` handle.
    pub profile_username: Option<String>,
    /// `twitter:card summary_large_image` instead of `summary` — set when the
    /// image is status media rather than an avatar or a site thumbnail.
    pub large_card: bool,
    /// `rel=alternate application/activity+json` — the subject's AP id, so
    /// fetchers can hop from the HTML page to the object (Mastodon does the
    /// same on profile and status pages).
    pub alternate: Option<String>,
    /// `rel=alternate application/atom+xml` — a local account's Atom feed,
    /// so RSS/Atom readers autodiscover it from the profile page (Mastodon does
    /// the same with its per-account RSS feed).
    pub feed_url: Option<String>,
    /// `noindex, noarchive` — the page subject opted out of search-engine
    /// indexing (Mastodon's `user_prefers_noindex?` header tag).
    pub noindex: bool,
}

impl PageMeta {
    fn og(property: &str, content: &str) -> Markup {
        html! { meta property=(property) content=(content); }
    }

    /// The head tags for this metadata. Tag-for-tag what Mastodon's
    /// `_og` partials and `opengraph` helper emit, so preview scrapers that
    /// already understand Mastodon pages understand ours.
    fn render(&self) -> Markup {
        html! {
            @if self.noindex {
                meta name="robots" content="noindex, noarchive";
            }
            @if let Some(canonical) = &self.canonical {
                link rel="canonical" href=(canonical);
            }
            @if let Some(alternate) = &self.alternate {
                link rel="alternate" type="application/activity+json" href=(alternate);
            }
            @if let Some(feed) = &self.feed_url {
                link rel="alternate" type="application/atom+xml" title="Atom feed" href=(feed);
            }
            @if let Some(description) = &self.description {
                meta name="description" content=(description);
                (Self::og("og:description", description))
            }
            @if let Some(title) = &self.og_title {
                @if let Some(site_name) = &self.site_name {
                    (Self::og("og:site_name", site_name))
                }
                (Self::og("og:type", self.og_kind.unwrap_or("website")))
                (Self::og("og:title", title))
                @if let Some(canonical) = &self.canonical {
                    (Self::og("og:url", canonical))
                }
                @if let Some(time) = &self.published_time {
                    (Self::og("og:published_time", time))
                }
                @if let Some(locale) = &self.locale {
                    (Self::og("og:locale", locale))
                }
                @if let Some(username) = &self.profile_username {
                    (Self::og("profile:username", username))
                }
                @if let Some(image) = &self.image {
                    (Self::og("og:image", &image.url))
                    @if let Some(width) = image.width {
                        (Self::og("og:image:width", &width.to_string()))
                    }
                    @if let Some(height) = image.height {
                        (Self::og("og:image:height", &height.to_string()))
                    }
                    @if let Some(alt) = &image.alt {
                        (Self::og("og:image:alt", alt))
                    }
                }
                (Self::og("twitter:card", if self.large_card { "summary_large_image" } else { "summary" }))
            }
        }
    }
}

/// The shared document `<head>`: charset, viewport, theme hint, the page
/// metadata and the versioned asset links. Both shells render into the same
/// The one inline script the server ever emits: flips the root class before
/// first paint (unlike the deferred bundle) so JS-only UI can hide itself
/// without a flash. The content-security policy admits it by sha256
/// (`crate::security_headers`), computed from this constant — so the script
/// and the policy cannot drift apart, and any *second* inline script added
/// anywhere will violate the policy rather than silently widen it.
pub(crate) const BOOTSTRAP_SCRIPT: &str = "document.documentElement.className='js'";

/// head so the metadata and cache-busting live in one place.
fn head(title: &str, meta: &PageMeta) -> Markup {
    html! {
        head {
            // Kept to this one statement — see `BOOTSTRAP_SCRIPT`.
            script { (PreEscaped(BOOTSTRAP_SCRIPT)) }
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover";
            meta name="color-scheme" content="light dark";
            meta name="application-name" content="Plamenu";
            meta name="mobile-web-app-capable" content="yes";
            meta name="apple-mobile-web-app-capable" content="yes";
            meta name="apple-mobile-web-app-title" content="Plamenu";
            meta name="apple-mobile-web-app-status-bar-style" content="default";
            // Tints the browser chrome to the page background; the values
            // mirror the `--bg` token pair in app.css — keep them in sync.
            meta name="theme-color" media="(prefers-color-scheme: light)" content="#f4f6f9";
            meta name="theme-color" media="(prefers-color-scheme: dark)" content="#0f1419";
            title { (title) " — Plamenu" }
            link rel="manifest" href="/manifest.webmanifest";
            link rel="apple-touch-icon" sizes="180x180" href="/pwa/apple-touch-icon.png";
            // The operator's favicon upload; the route serves an embedded
            // brand-mark default until one exists, so this never 404s.
            link rel="icon" type="image/png" sizes="48x48" href="/favicon.ico";
            (meta.render())
            // Content-hashed URL lets the asset stay `immutable` in cache yet
            // refresh whenever the bundled CSS/JS actually changes.
            link rel="stylesheet" href={ "/assets/app.css?v=" (*ASSET_VERSION) };
            // Operator-supplied overrides from the admin settings; served
            // empty until any are written.
            link rel="stylesheet" href="/custom.css";
            script defer src={ "/assets/app.js?v=" (*ASSET_VERSION) } {}
        }
    }
}

/// Which anonymous-access features the navigation should advertise. Only
/// consulted when the visitor is signed out — a logged-out person shouldn't see
/// an "Explore" or "Search" link that just bounces them to the login page when
/// the instance keeps those behind auth.
#[derive(Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools, reason = "one flag per nav entry")]
pub struct AnonNav {
    /// The Live-feeds public timelines: at least one scope's preview is on.
    pub feeds: bool,
    /// The Trending pages are browsable without signing in.
    pub trends: bool,
    /// The People directory is browsable without signing in.
    pub people: bool,
    /// The Groups directory is browsable without signing in.
    pub groups: bool,
    /// Search is usable without signing in.
    pub search: bool,
}

impl AnonNav {
    /// Anonymous nav with everything on — used for the signed-in shells, where
    /// these flags are never read.
    fn all() -> Self {
        Self {
            feeds: true,
            trends: true,
            people: true,
            groups: true,
            search: true,
        }
    }
}

/// Wraps `content` in the full page chrome for an **always signed-in** page
/// (home, notifications, compose, settings, admin). `user` drives the account
/// block; the anonymous-access flags are irrelevant here.
pub fn shell(title: &str, user: Option<&WebUser>, content: &Markup) -> Markup {
    render(title, user, AnonNav::all(), content, &PageMeta::default())
}

/// Wraps `content` for a page that renders for both signed-in and anonymous
/// visitors, with an explicitly negotiated anonymous locale. `anon` gates
/// which links a logged-out visitor sees; signed-in pages normally obtain the
/// locale from [`WebUser`], entry flows use the request's `Accept-Language`
/// before an account exists.
pub fn shell_visitor_localized(
    title: &str,
    user: Option<&WebUser>,
    anon: AnonNav,
    content: &Markup,
    locale: Locale,
) -> Markup {
    render_with_locale(title, user, anon, content, &PageMeta::default(), locale)
}

/// [`shell_visitor_localized`] for pages with a shareable subject (profile, thread,
/// the instance itself), with a request-negotiated anonymous locale: `meta`
/// carries the link-preview tags, canonical URL and robots directive for
/// that subject.
pub fn shell_visitor_subject_localized(
    title: &str,
    user: Option<&WebUser>,
    anon: AnonNav,
    content: &Markup,
    meta: &PageMeta,
    locale: Locale,
) -> Markup {
    render_with_locale(title, user, anon, content, meta, locale)
}

fn render(
    title: &str,
    user: Option<&WebUser>,
    anon: AnonNav,
    content: &Markup,
    meta: &PageMeta,
) -> Markup {
    let locale = user.map_or_else(Locale::default, |user| user.locale);
    render_with_locale(title, user, anon, content, meta, locale)
}

fn render_with_locale(
    title: &str,
    user: Option<&WebUser>,
    anon: AnonNav,
    content: &Markup,
    meta: &PageMeta,
    locale: Locale,
) -> Markup {
    let live_notifications = user.is_some_and(|user| user.live_notifications);
    let notification_sound = user.is_some_and(|user| user.notification_sound);
    let notification_volume = user.map_or(0, |user| user.notification_volume);
    html! {
        (DOCTYPE)
        // `no-js` is swapped to `js` synchronously below so JS-only affordances
        // (e.g. the composer's collapsible groups) can style themselves before
        // first paint without a flash. The class is absent entirely when the
        // script is blocked, leaving the no-JS fallbacks in force.
        html lang=(locale.tag()) dir=(locale.direction()) class="no-js" {
            (head(title, meta))
            body data-live-notifications=(live_notifications)
                data-notification-sound=(notification_sound)
                data-notification-volume=(notification_volume) {
                a.skip-link href="#main-content" { (locale.text("nav-skip-to-content")) }
                // The tab bar is `position: fixed`, so its slot in the flow
                // doesn't matter visually — but its slot in the *source* does.
                // The browser paints as it parses, so a bar emitted after the
                // timeline only appears once the last status has been parsed:
                // on a long page that reads as the chrome flickering in late.
                // Emitted first, it lands in the very first paint. (It also
                // puts the mobile nav ahead of the content in reading and tab
                // order, matching the desktop sidebar above.)
                (tabbar(user, anon, locale))
                div.app-shell {
                    (sidebar(user, anon, locale))
                    main.app-main id="main-content" tabindex="-1" { (content) }
                }
            }
        }
    }
}

/// A chrome-free shell: no sidebar, no tab bar, just the centered content.
///
/// Used for self-contained flows that are typically opened in a popup or
/// embedded frame — the OAuth consent screen — where app navigation would be
/// a distraction (or an escape hatch out of the flow).
pub fn focused_shell(title: &str, content: &Markup) -> Markup {
    focused_shell_localized(title, content, Locale::default())
}

/// [`focused_shell`] with an explicitly negotiated locale.
pub fn focused_shell_localized(title: &str, content: &Markup, locale: Locale) -> Markup {
    html! {
        (DOCTYPE)
        // `no-js` is swapped to `js` synchronously below so JS-only affordances
        // (e.g. the composer's collapsible groups) can style themselves before
        // first paint without a flash. The class is absent entirely when the
        // script is blocked, leaving the no-JS fallbacks in force.
        html lang=(locale.tag()) dir=(locale.direction()) class="no-js" {
            (head(title, &PageMeta::default()))
            body {
                main.focused-main { (content) }
            }
        }
    }
}

/// A single navigation entry: an icon plus a label.
fn nav_link(href: &str, name: &str, label: &str) -> Markup {
    html! {
        a.nav-link href=(href) {
            (icon(name))
            span.nav-link__label { (label) }
        }
    }
}

/// An icon wearing a dot while something unread awaits. The notifications
/// bell, the Private-mentions at-sign and the drawer burger all share the one
/// indicator so they never disagree.
fn dotted_icon(name: &str, unread: bool) -> Markup {
    html! {
        span.bell {
            (icon(name))
            @if unread { span.bell__dot {} }
        }
    }
}

/// The notifications bell, wearing a dot while unread notifications await.
/// Shared by the sidebar/drawer nav entry and the mobile tab bar so the two
/// indicators never disagree.
fn bell_icon(unread: bool) -> Markup {
    dotted_icon("bell", unread)
}

/// The signed-in primary navigation links. Shared verbatim by the desktop
/// sidebar and the mobile drawer so the two menus never drift apart.
fn primary_nav(user: &WebUser) -> Markup {
    let locale = user.locale;
    html! {
        (nav_link("/", "home", &locale.text("nav-home")))
        (nav_link("/public", "feeds", &locale.text("nav-live-feeds")))
        (nav_link("/search", "search", &locale.text("nav-search")))
        a.nav-link href="/notifications" data-notification-nav {
            (bell_icon(user.unread_notifications))
            span.nav-link__label {
                (locale.text("nav-notifications"))
                @if user.unread_notifications {
                    span.visually-hidden { " (" (locale.text("nav-new")) ")" }
                }
            }
        }
        a.nav-link href="/conversations" {
            (dotted_icon("mention", user.unread_conversations))
            span.nav-link__label {
                (locale.text("nav-private-mentions"))
                @if user.unread_conversations {
                    span.visually-hidden { " (" (locale.text("nav-new")) ")" }
                }
            }
        }
        (nav_link("/bookmarks", "bookmark", &locale.text("nav-bookmarks")))
        (nav_link("/favourites", "favourite", &locale.text("nav-favourites")))
        (nav_link("/lists", "list", &locale.text("nav-lists")))
        (nav_link("/groups", "group", &locale.text("nav-groups")))
        (nav_link("/webxdc", "apps", &locale.text("nav-webxdc")))
        (nav_link("/people", "profile", &locale.text("nav-people")))
        (nav_link("/explore", "explore", &locale.text("nav-trending")))
        (nav_link("/settings", "settings", &locale.text("nav-settings")))
        @if user.is_staff {
            (nav_link("/admin", "shield", &locale.text("nav-admin")))
        }
    }
}

/// The signed-out navigation links. Shared by the desktop sidebar and the
/// mobile drawer, like `primary_nav` for the signed-in chrome. Each discovery
/// surface follows its own anonymous-access flag, so a link only appears when
/// the page behind it actually opens. Rules and Staff are always-public
/// pages, so they always appear.
fn anon_primary_nav(anon: AnonNav, locale: Locale) -> Markup {
    html! {
        (nav_link("/", "home", &locale.text("nav-home")))
        @if anon.trends { (nav_link("/explore", "explore", &locale.text("nav-trending"))) }
        @if anon.feeds { (nav_link("/public", "feeds", &locale.text("nav-live-feeds"))) }
        @if anon.people { (nav_link("/people", "profile", &locale.text("nav-people"))) }
        @if anon.groups { (nav_link("/groups", "group", &locale.text("nav-groups"))) }
        @if anon.search { (nav_link("/search", "search", &locale.text("nav-search"))) }
        (nav_link("/rules", "list", &locale.text("nav-rules")))
        (nav_link("/staff", "shield", &locale.text("nav-staff")))
    }
}

/// The signed-in account's avatar as a root-relative URL. A session always
/// belongs to a local account, so its upload is served straight from
/// `/media/{file}`; an account that never set one falls back to the same
/// `/static/missing.png` placeholder the API entity hands out, so the row can
/// never render a broken image.
fn avatar_src(user: &WebUser) -> String {
    match user.current.account.avatar_file_name.as_deref() {
        Some(file) => format!("/media/{file}"),
        None => "/static/missing.png".to_owned(),
    }
}

/// The account row — avatar, profile link plus the sign-out form. Shared by the
/// desktop sidebar and the mobile drawer.
fn account_block(user: &WebUser) -> Markup {
    let locale = user.locale;
    html! {
        div.account {
            a.account__link href={ "/@" (user.current.account.username) } {
                // Decorative: the name and handle sit right beside it, so a
                // screen reader announcing the image too would only stutter.
                img.account__avatar src=(avatar_src(user)) alt=""
                    width="32" height="32" loading="lazy";
                span.account__name { (display_name(user)) }
                span.account__handle { "@" (user.current.account.username) }
            }
            a.account__switch href="/login?switch=1" title=(locale.text("nav-switch-account"))
                aria-label=(locale.text("nav-switch-account")) { (icon("switch")) }
            form.logout method="post" action="/logout" {
                input type="hidden" name="csrf" value=(user.csrf);
                button.logout__btn type="submit" title=(locale.text("nav-sign-out")) { (icon("logout")) }
            }
        }
    }
}

/// The desktop sidebar: brand, primary navigation, a compose button and the
/// account block.
fn sidebar(user: Option<&WebUser>, anon: AnonNav, locale: Locale) -> Markup {
    html! {
        aside.sidebar {
            a.brand href="/" {
                span.brand__mark { "P" }
                span.brand__name { "Plamenu" }
            }
            nav.nav aria-label=(locale.text("nav-primary")) {
                @if let Some(user) = user {
                    (primary_nav(user))
                } @else {
                    (anon_primary_nav(anon, locale))
                }
            }
            @if let Some(user) = user {
                a.compose-btn href="/compose" { (icon("compose")) span { (locale.text("nav-new-post")) } }
                (account_block(user))
            } @else {
                a.compose-btn href="/login" { (icon("profile")) span { (locale.text("nav-sign-in")) } }
            }
        }
    }
}

/// The mobile bottom tab bar. Signed in, it holds the five most-used
/// destinations — Home, Live feeds, New post, Notifications — plus a burger that
/// opens the full drawer (everything the desktop sidebar reaches). The bar was
/// previously overloaded with every link; the drawer keeps it to five. Signed
/// out it works the same way, just smaller: Home plus the burger, with every
/// other destination living in the drawer.
fn tabbar(user: Option<&WebUser>, anon: AnonNav, locale: Locale) -> Markup {
    html! {
        nav.tabbar aria-label=(locale.text("nav-primary")) {
            a.tabbar__link href="/" title=(locale.text("nav-home")) { (icon("home")) }
            @if let Some(user) = user {
                a.tabbar__link href="/public" title=(locale.text("nav-live-feeds")) { (icon("feeds")) }
                a.tabbar__link.tabbar__link--accent href="/compose" title=(locale.text("nav-new-post")) { (icon("compose")) }
                a.tabbar__link href="/notifications" data-notification-nav
                    title=(locale.text("nav-notifications")) {
                    (bell_icon(user.unread_notifications))
                }
            }
            (drawer(user, anon, locale))
        }
    }
}

/// The mobile "burger" drawer: a `<details>` disclosure whose summary is the
/// last tab-bar slot and whose panel is a scrollable side overlay carrying the
/// full sidebar — brand, every nav link, and (signed in) compose and the
/// account block, or (signed out) the sign-in button. The scrim behind the
/// panel closes it on an outside tap (see `app.js`); it also closes on
/// Escape. The panel scrolls on its own so a growing menu never overflows the
/// viewport.
fn drawer(user: Option<&WebUser>, anon: AnonNav, locale: Locale) -> Markup {
    // The burger carries the private-mentions dot: that nav entry lives
    // inside the drawer, so without it a phone user never learns a DM
    // arrived (notifications have their own tab slot).
    let unread = user.is_some_and(|user| user.unread_conversations);
    let menu_title = if unread {
        locale.text("nav-menu-new-private")
    } else {
        locale.text("nav-menu")
    };
    html! {
        details.drawer data-drawer {
            summary.tabbar__link.drawer__toggle title=(menu_title) aria-label=(menu_title) {
                (dotted_icon("menu", unread))
            }
            div.drawer__scrim data-drawer-scrim {
                aside.drawer__panel {
                    a.brand href="/" {
                        span.brand__mark { "P" }
                        span.brand__name { "Plamenu" }
                    }
                    nav.nav aria-label=(locale.text("nav-primary")) {
                        @if let Some(user) = user {
                            (primary_nav(user))
                        } @else {
                            (anon_primary_nav(anon, locale))
                        }
                    }
                    @if let Some(user) = user {
                        a.compose-btn href="/compose" { (icon("compose")) span { (locale.text("nav-new-post")) } }
                        (account_block(user))
                    } @else {
                        a.compose-btn href="/login" { (icon("profile")) span { (locale.text("nav-sign-in")) } }
                    }
                }
            }
        }
    }
}

/// The account's display name, falling back to the handle when unset.
fn display_name(user: &WebUser) -> String {
    let account = &user.current.account;
    if account.display_name.is_empty() {
        format!("@{}", account.username)
    } else {
        account.display_name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_shell_starts_with_a_localized_main_content_bypass() {
        let content = html! { h1 { "Example" } };
        let rendered = shell_visitor_localized(
            "Example",
            None,
            AnonNav::default(),
            &content,
            Locale::negotiate(Some("ru"), None),
        )
        .into_string();

        let skip = rendered
            .find(r##"<a class="skip-link" href="#main-content">Перейти к содержимому</a>"##)
            .expect("localized skip link");
        let navigation = rendered.find("<nav").expect("navigation");
        assert!(
            skip < navigation,
            "skip link must precede repeated navigation"
        );
        assert!(rendered.contains(r#"<main class="app-main" id="main-content" tabindex="-1">"#));
    }
}
