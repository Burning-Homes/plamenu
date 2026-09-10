//! Instance settings for the admin dashboard — branding, contact and custom
//! CSS (Mastodon's `admin/settings` pages backed by `Form::AdminSettings`).
//! The values feed the instance API entities, `GET /api/v1/instance/
//! extended_description` and `/custom.css`. All pages require
//! `MANAGE_SETTINGS`.
//!
//! The page is ~90 knobs, so it is split into collapsed `<details>` groups,
//! each its own form with its own Save button ([`Section`]). A submit carries
//! only its group's fields and the handler overlays them on the stored row,
//! so one group can never reset another. With JS on, a field whose governing
//! option is not selected is hidden (`data-show-when`); with JS off every
//! field stays visible and every group still saves.

use axum::extract::{Form, Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::instance_settings::{
    DomainBlocksDisclosure, GroupCreationPolicy, InstanceSettings, RegistrationsMode,
    SettingsUpdate,
};
use plamenu_db::role::permission;
use plamenu_db::site_upload::{self, NewVariant, SiteUpload};
use plamenu_db::{account, custom_emoji, instance_settings};
use serde::Deserialize;

use super::super::clock::ViewerClock;
use super::super::view::icon;
use super::{WebAdmin, admin_shell};
use crate::AppState;
use crate::media_processing::process_site_upload_blocking;
use crate::web::session::csrf_rejection;

/// Mastodon's `Form::AdminSettings::DESCRIPTION_LIMIT`.
const DESCRIPTION_LIMIT: usize = 200;

/// One collapsible group of settings. Each is a separate form naming itself
/// in a hidden `section` field; [`save`] applies only that group's fields on
/// top of the stored values. A form that names no section (an older client,
/// or a test) submits the whole set at once — [`Section::All`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    All,
    Branding,
    Landing,
    Registrations,
    Groups,
    Posting,
    CustomEmoji,
    Webxdc,
    Timelines,
    Discovery,
    Anonymous,
    Federation,
    Images,
    Video,
    Retention,
    Translation,
    Limits,
    Appearance,
}

impl Section {
    /// The form value / DOM id suffix; `All` has none (it is never rendered).
    fn key(self) -> &'static str {
        match self {
            Self::All => "",
            Self::Branding => "branding",
            Self::Landing => "landing",
            Self::Registrations => "registrations",
            Self::Groups => "groups",
            Self::Posting => "posting",
            Self::CustomEmoji => "custom-emoji",
            Self::Webxdc => "webxdc",
            Self::Timelines => "timelines",
            Self::Discovery => "discovery",
            Self::Anonymous => "anonymous",
            Self::Federation => "federation",
            Self::Images => "images",
            Self::Video => "video",
            Self::Retention => "retention",
            Self::Translation => "translation",
            Self::Limits => "limits",
            Self::Appearance => "appearance",
        }
    }

    /// `None` for a value no section claims — a hand-made or corrupted
    /// submit, refused rather than silently applied as "everything".
    fn parse(value: &str) -> Option<Self> {
        let section = match value.trim() {
            "" => Self::All,
            "branding" => Self::Branding,
            "landing" => Self::Landing,
            "registrations" => Self::Registrations,
            "groups" => Self::Groups,
            "posting" => Self::Posting,
            "custom-emoji" => Self::CustomEmoji,
            "webxdc" => Self::Webxdc,
            "timelines" => Self::Timelines,
            "discovery" => Self::Discovery,
            "anonymous" => Self::Anonymous,
            "federation" => Self::Federation,
            "images" => Self::Images,
            "video" => Self::Video,
            "retention" => Self::Retention,
            "translation" => Self::Translation,
            "limits" => Self::Limits,
            "appearance" => Self::Appearance,
            _ => return None,
        };
        Some(section)
    }

    /// Whether a submit of `self` carries `other`'s fields.
    fn covers(self, other: Self) -> bool {
        self == Self::All || self == other
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
    /// Free text for flash codes that carry one (test-e-mail results).
    detail: Option<String>,
    /// Which section to render expanded — the one just saved, so the operator
    /// lands back where they were working.
    open: Option<String>,
}

/// `GET /admin/settings` — the instance settings page: one collapsed
/// disclosure per group.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;

    let settings = instance_settings::get(&state.pool).await.map_err(api_err)?;
    let emoji_settings = custom_emoji::settings(&state.pool).await.map_err(api_err)?;
    let webxdc_limits = plamenu_db::webxdc::limits(&state.pool)
        .await
        .map_err(api_err)?;
    let uploads = site_upload::all(&state.pool).await.map_err(api_err)?;
    let open = query.open.as_deref();
    let clock = &admin.user.clock;
    let csrf = admin.user.csrf.as_str();
    let ctx = SectionCtx { csrf, open };
    let body = html! {
        (flash_banner(query.flash.as_deref(), query.detail.as_deref()))
        p.settings-field__hint.admin-sections__intro {
            "Each group opens on its own and saves on its own — a Save button "
            "only writes the group it sits in."
        }
        div.admin-sections data-settings-deps {
            (ctx.form(Section::Branding, "Server identity",
                "Name, description and who to contact",
                "Save identity", &branding_fields(&settings)))
            (ctx.panel("site-images", "Site images",
                "Thumbnail, mascot, favicon and app icon",
                &site_images_fields(&state.config.domain, &uploads, csrf)))
            (ctx.form(Section::Landing, "Landing page",
                "What a signed-out visitor sees at the server root",
                "Save landing page", &landing_fields(&settings)))
            (ctx.form(Section::Registrations, "Registrations",
                "Who may sign up, and the age gate",
                "Save registrations", &registrations_fields(&settings, clock)))
            (ctx.form(Section::Groups, "Groups",
                "Who may create local communities",
                "Save groups", &groups_fields(&settings)))
            (ctx.form(Section::Posting, "Posting limits",
                "Character, attachment and poll caps",
                "Save posting limits", &posting_fields(&settings)))
            (ctx.form(Section::Webxdc, "Webxdc apps",
                "Package sizes and retained session storage",
                "Save Webxdc limits", &webxdc_fields(webxdc_limits)))
            (ctx.form(Section::CustomEmoji, "Custom emoji",
                "Personal collections and local emoji uploads",
                "Save custom emoji settings", &custom_emoji_fields(emoji_settings)))
            (ctx.form(Section::Timelines, "Timelines",
                "How the built-in web client composes feeds",
                "Save timelines", &timeline_fields(&settings)))
            (ctx.form(Section::Discovery, "Discovery",
                "Which public directories and APIs this server publishes",
                "Save discovery", &discovery_fields(&settings)))
            (ctx.form(Section::Anonymous, "Anonymous access",
                "What a signed-out visitor may browse",
                "Save anonymous access", &anonymous_access_fields(&settings)))
            (ctx.form(Section::Federation, "Federation",
                "Signature posture toward other servers",
                "Save federation", &federation_fields(&settings, &state.config)))
            (ctx.form(Section::Images, "Image processing",
                "How uploaded and cached images are stored",
                "Save image processing", &image_fields(&settings)))
            (ctx.form(Section::Video, "Video & audio",
                "Transcoding parameters and upload caps",
                "Save video & audio", &video_fields(&settings)))
            (ctx.form(Section::Retention, "Content retention",
                "How long cached remote content and sign-in logs are kept",
                "Save retention", &retention_fields(&settings)))
            (ctx.form(Section::Translation, "Translation",
                "Translation cache and backend throttles",
                "Save translation",
                &translation_fields(&settings, state.config.translation.is_some())))
            (ctx.form(Section::Limits, "Rate limits",
                "Request throttles for the API and the web client",
                "Save rate limits", &rate_limit_fields(&settings)))
            (ctx.form(Section::Appearance, "Appearance",
                "Custom CSS served to every web page",
                "Save appearance", &appearance_fields(&settings)))
            (ctx.panel("email", "E-mail",
                "Check the configured SMTP relay",
                &email_fields(state.config.smtp.is_some(), csrf)))
            (ctx.panel("maintenance", "Maintenance",
                "One-shot housekeeping jobs",
                &maintenance_fields(csrf)))
        }
    };
    Ok(admin_shell(&admin, "/admin/settings", "Settings", &body).into_response())
}

/// The per-request bits every section needs: the CSRF token and which section
/// the redirect asked to be expanded.
struct SectionCtx<'a> {
    csrf: &'a str,
    open: Option<&'a str>,
}

impl SectionCtx<'_> {
    /// A collapsible group that is not a settings form (site images, the
    /// e-mail probe, maintenance jobs — each carries its own small forms).
    fn panel(&self, key: &str, title: &str, blurb: &str, body: &Markup) -> Markup {
        html! {
            details.admin-section id=(format!("sec-{key}")) open[self.open == Some(key)] {
                summary.admin-section__summary {
                    span.admin-section__heading {
                        span.admin-section__title { (title) }
                        span.admin-section__blurb { (blurb) }
                    }
                    span.admin-section__caret { (icon("chevron")) }
                }
                div.admin-section__body { (body) }
            }
        }
    }

    /// A collapsible group whose body is one settings form, saving only its
    /// own fields.
    fn form(
        &self,
        section: Section,
        title: &str,
        blurb: &str,
        save_label: &str,
        fields: &Markup,
    ) -> Markup {
        let body = html! {
            form.admin-form method="post" action="/web/admin/settings" {
                input type="hidden" name="csrf" value=(self.csrf);
                input type="hidden" name="section" value=(section.key());
                (fields)
                div.admin-actions { button type="submit" { (save_label) } }
            }
        };
        self.panel(section.key(), title, blurb, &body)
    }
}

// ---- Section bodies ------------------------------------------------------

fn branding_fields(settings: &InstanceSettings) -> Markup {
    html! {
        label {
            "Server name"
            input type="text" name="site_title" value=(settings.site_title) required;
        }
        label {
            "Short description"
            textarea name="site_short_description" rows="2"
                maxlength=(DESCRIPTION_LIMIT) { (settings.site_short_description) }
            span.settings-field__hint {
                "One sentence, shown in link previews and the instance API. "
                "Up to " (DESCRIPTION_LIMIT) " characters."
            }
        }
        label {
            "Extended description"
            textarea name="site_extended_description" rows="8" {
                (settings.site_extended_description)
            }
            span.settings-field__hint { "Markdown. The server's public about page." }
        }
        label {
            "Contact username"
            input type="text" name="site_contact_username"
                value=(settings.site_contact_username);
            span.settings-field__hint { "A local account, published as the server contact." }
        }
        label {
            "Contact e-mail"
            input type="email" name="site_contact_email"
                value=(settings.site_contact_email);
            span.settings-field__hint { "Published as the server contact; not used to send mail." }
        }
    }
}

fn landing_fields(settings: &InstanceSettings) -> Markup {
    html! {
        label {
            span.admin-check {
                input type="checkbox" name="landing_page" value="1"
                    checked[settings.landing_page];
                " Public welcome page at the server root"
            }
            span.settings-field__hint {
                "Off, signed-out visitors go straight to the sign-in page. "
                "Rules and staff keep their own public pages either way."
            }
        }
        label.admin-form__nested data-show-when="landing_page" {
            span.admin-check {
                input type="checkbox" name="landing_show_stats" value="1"
                    checked[settings.landing_show_stats];
                " Show server statistics on it"
            }
            span.settings-field__hint {
                "Local and known-network user/post counts plus connected "
                "relays, cached a few minutes."
            }
        }
    }
}

/// Who may sign up, plus the age gate — with the auto-close notice (O3) when
/// the maintenance worker reverted unattended open registration.
fn registrations_fields(settings: &InstanceSettings, clock: &ViewerClock) -> Markup {
    let mode = settings.registrations_mode();
    html! {
        label {
            "Who may sign up"
            select name="registrations_mode" {
                option value="none" selected[mode == RegistrationsMode::None] {
                    "Nobody (the operator creates accounts)"
                }
                option value="approved" selected[mode == RegistrationsMode::Approved] {
                    "Anyone, with admin approval"
                }
                option value="open" selected[mode == RegistrationsMode::Open] {
                    "Anyone"
                }
            }
            span.settings-field__hint {
                "Self-service sign-up also needs an SMTP relay ([smtp] in "
                "plamenu.toml) for the confirmation e-mails."
            }
            @if let Some(closed_at) = settings.registrations_auto_closed_at {
                span.settings-field__hint {
                    "Switched from open to approval mode on "
                    (clock.element(closed_at))
                    " because no moderator had been active for a week. Saving "
                    "any group here dismisses this notice."
                }
            }
        }
        label data-show-when="registrations_mode!=none" {
            "Minimum sign-up age"
            input type="number" name="min_age" min="0" max="120" step="1"
                value=(settings.min_age);
            span.settings-field__hint {
                "0 turns the age gate off; a positive value asks for a date of "
                "birth at least that old."
            }
        }
    }
}

/// The group creation policy select.
fn groups_fields(settings: &InstanceSettings) -> Markup {
    let policy = settings.group_creation_policy();
    html! {
        label {
            "Who may create groups"
            select name="group_creation_policy" {
                option value="everyone" selected[policy == GroupCreationPolicy::Everyone] {
                    "Anyone with an account"
                }
                option value="approved" selected[policy == GroupCreationPolicy::Approved] {
                    "Staff, plus approved accounts"
                }
                option value="admins" selected[policy == GroupCreationPolicy::Admins] {
                    "Staff only"
                }
            }
            span.settings-field__hint {
                "Local communities other servers can follow. Until the "
                "per-account approval queue ships, \u{201c}approved "
                "accounts\u{201d} behaves like staff-only."
            }
        }
    }
}

fn posting_fields(settings: &InstanceSettings) -> Markup {
    html! {
        (limit_field("max_characters", "Characters per post", settings.max_characters,
            "Counted like Mastodon: a URL weighs 23, a mention's domain is free."))
        (limit_field("max_characters_long_form", "Characters per long-form post",
            settings.max_characters_long_form,
            "Only for posts published as articles, which federate their full body."))
        div.admin-form__grid {
            (limit_field("max_media_attachments", "Attachments per post",
                settings.max_media_attachments, ""))
            (limit_field("poll_max_options", "Poll choices", settings.poll_max_options, ""))
        }
    }
}

fn webxdc_fields(limits: plamenu_db::webxdc::Limits) -> Markup {
    html! {
        p.settings-field__hint { "Sizes are in MiB. Package limits apply to uploads and apps fetched from other servers. Changes apply to new uploads, fetches and updates; existing apps remain available." }
        p { a href="/admin/webxdc" { "Manage sessions and storage →" } }
        div.admin-form__grid {
            @for (name, label, value, max) in [
                ("webxdc_bundle_mb", "Package upload", limits.bundle_mb, 512),
                ("webxdc_expanded_mb", "Expanded package", limits.expanded_mb, 1024),
                ("webxdc_file_mb", "Individual file inside a package", limits.file_mb, 512),
                ("webxdc_session_mb", "Storage per session", limits.session_mb, 65_536),
                ("webxdc_account_mb", "Storage per local account", limits.account_mb, 1_048_576),
                ("webxdc_total_mb", "Total server storage", limits.total_mb, 1_048_576),
            ] {
                label { (label) input type="number" name=(name) min="1" max=(max) step="1" value=(value) required; }
            }
        }
        p.settings-field__hint { "Session storage counts the original package, expanded files and durable updates. Allow at least the package and expanded limits combined. Account storage must cover at least one session. Identical packages count once per account and once across the server; durable updates count separately per session. Deleting the last session using a package releases its bytes." }
    }
}

fn custom_emoji_fields(settings: custom_emoji::CustomEmojiSettings) -> Markup {
    html! {
        div.admin-form__grid {
            label {
                "Personal emoji per member"
                input type="number" name="personal_emoji_limit" min="0" step="1"
                    value=(settings.personal_limit) required;
                span.settings-field__hint {
                    "How many personal custom emoji one member may keep. 0 allows unlimited emoji. Promoted instance-wide emoji no longer count."
                }
            }
            label {
                "Largest custom emoji file (KiB)"
                input type="number" name="custom_emoji_max_file_size_kb" min="1" max="16384"
                    step="1" value=(settings.max_file_size_kb) required;
                span.settings-field__hint {
                    "Applies to PNG, GIF and WebP files added by upload, borrow, the CLI, or a compatible API. Default: 512 KiB; maximum: 16384 KiB."
                }
            }
        }
        p.settings-field__hint {
            a href="/admin/custom-emojis" { "Manage custom emoji and review trends" }
        }
    }
}

/// Feed presentation in the built-in web client. Nothing here reaches
/// `/api/v1/timelines/*` — a third-party client keeps every boost row and does
/// its own thing with them.
fn timeline_fields(settings: &InstanceSettings) -> Markup {
    html! {
        label {
            span.admin-check {
                input type="checkbox" name="boost_collapse" value="1"
                    checked[settings.boost_collapse];
                " Merge repeated boosts into one post"
            }
            span.settings-field__hint {
                "Home and list timelines in this server's own web client show "
                "\"Alice, Bob and 3 others boosted\" on a single card instead "
                "of one card per boost. A community's posts are never merged, "
                "and each reader can turn this off for themselves."
            }
        }
        label.admin-form__nested data-show-when="boost_collapse" {
            "Look back this many posts"
            input type="number" name="boost_collapse_lookback"
                value=(settings.boost_collapse_lookback) min="0" max="500" step="1" required;
            span.settings-field__hint {
                "0 (the default) merges only within the page being read, and "
                "costs nothing. A higher number also hides a boost of a post "
                "already shown further up the feed — at the price of a second "
                "timeline query on every page after the first, which is the "
                "most expensive query this server runs."
            }
        }
    }
}

fn discovery_fields(settings: &InstanceSettings) -> Markup {
    html! {
        label {
            span.admin-check {
                input type="checkbox" name="peers_api_enabled" value="1"
                    checked[settings.peers_api_enabled];
                " Publish the list of discovered servers"
            }
            span.settings-field__hint { "Serves /api/v1/instance/peers and /api/v1/peers/search." }
        }
        label {
            span.admin-check {
                input type="checkbox" name="activity_api_enabled" value="1"
                    checked[settings.activity_api_enabled];
                " Publish aggregate weekly activity"
            }
            span.settings-field__hint {
                "Serves /api/v1/instance/activity: posts, sign-ins and sign-ups per week."
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="profile_directory" value="1"
                    checked[settings.profile_directory];
                " Profile directory"
            }
            span.settings-field__hint {
                "Serves /api/v1/directory: profiles that opted in to discovery."
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="trends_enabled" value="1"
                    checked[settings.trends_enabled];
                " Trends"
            }
            span.settings-field__hint {
                "Serves /api/v1/trends/tags: hashtags gaining traction."
            }
        }
        label.admin-form__nested data-show-when="trends_enabled" {
            span.admin-check {
                input type="checkbox" name="trendable_by_default" value="1"
                    checked[settings.trendable_by_default];
                " Let tags trend without review"
            }
            span.settings-field__hint {
                "Otherwise a moderator approves each hashtag before it may trend."
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="public_timeline_replies" value="1"
                    checked[settings.public_timeline_replies];
                " Show replies on the public timelines"
            }
            span.settings-field__hint {
                "Off (like Mastodon), the federated and local timelines carry "
                "posts and their authors' own threads, not replies to other "
                "people. Hashtag timelines always carry replies, and the home "
                "and list timelines are unaffected."
            }
        }
        (disclosure_field("show_domain_blocks", "Show the moderated-server list",
            settings.show_domain_blocks(),
            "Who may read the limited and suspended domains on \
             /api/v1/instance/domain_blocks.", ""))
        (disclosure_field("show_domain_blocks_rationale", "Show why they were moderated",
            settings.show_domain_blocks_rationale(),
            "Who may read each entry's public comment.",
            "show_domain_blocks!=disabled"))
    }
}

/// What a signed-out visitor may browse. Profiles and individual public posts
/// stay visible regardless of these.
fn anonymous_access_fields(settings: &InstanceSettings) -> Markup {
    html! {
        p.settings-field__hint {
            "Profiles and individual public posts are always visible to "
            "signed-out visitors; these switches cover the browsing surfaces."
        }
        label {
            span.admin-check {
                input type="checkbox" name="timeline_preview_federated" value="1"
                    checked[settings.timeline_preview_federated];
                " Federated timeline"
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="timeline_preview_local" value="1"
                    checked[settings.timeline_preview_local];
                " Local timeline"
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="timeline_preview_tag" value="1"
                    checked[settings.timeline_preview_tag];
                " Hashtag timelines"
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="public_search" value="1"
                    checked[settings.public_search];
                " Search"
            }
        }
        label data-show-when="trends_enabled" {
            span.admin-check {
                input type="checkbox" name="anon_trends" value="1"
                    checked[settings.anon_trends];
                " Trending pages"
            }
            span.settings-field__hint {
                "Web only — the trends API is public whenever trends are enabled."
            }
        }
        label data-show-when="profile_directory" {
            span.admin-check {
                input type="checkbox" name="anon_directory" value="1"
                    checked[settings.anon_directory];
                " People directory"
            }
            span.settings-field__hint {
                "Web only — the directory API is public whenever the profile "
                "directory is enabled."
            }
        }
        label.admin-form__nested data-show-when="anon_directory" {
            span.admin-check {
                input type="checkbox" name="anon_directory_federated" value="1"
                    checked[settings.anon_directory_federated];
                " …including remote profiles"
            }
            span.settings-field__hint {
                "Off, signed-out visitors see only this server's accounts there."
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="anon_groups" value="1"
                    checked[settings.anon_groups];
                " Groups directory"
            }
        }
    }
}

/// Federation posture overrides (O4). Every knob here also exists in
/// plamenu.toml as the bootstrap default; a value chosen here wins and takes
/// effect within the settings-cache TTL.
fn federation_fields(settings: &InstanceSettings, config: &crate::config::Config) -> Markup {
    // The carve-out only bites under secure mode, which may be on either
    // because it was forced here or because the config file defaults it on.
    let when_secure = if config.authorized_fetch {
        "authorized_fetch=on|authorized_fetch="
    } else {
        "authorized_fetch=on"
    };
    html! {
        (override_toggle("authorized_fetch", "Authorized fetch (secure mode)",
            settings.authorized_fetch, config.authorized_fetch,
            "Require an HTTP signature on ActivityPub fetches of actors, posts \
             and collections, so blocked servers cannot read this one.", ""))
        (override_toggle("authorized_fetch_unsigned_profile",
            "Serve profiles to unsigned fetchers",
            settings.authorized_fetch_unsigned_profile,
            config.authorized_fetch_unsigned_profile,
            "Answer an unsigned actor fetch with the full profile instead of a \
             key-only one. Keep this on: default-config Lemmy dereferences \
             unsigned and otherwise drops our deliveries.", when_secure))
        label {
            span.admin-check {
                input type="checkbox" name="emit_integrity_proofs" value="1"
                    checked[settings.emit_integrity_proofs];
                " Sign outgoing activities with integrity proofs (FEP-8b32)"
            }
            span.settings-field__hint {
                "An Ed25519 proof travels with the activity, so receivers — and "
                "anyone they forward it to — can authenticate it without "
                "refetching the origin."
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="emit_rfc9421" value="1"
                    checked[settings.emit_rfc9421];
                " Offer RFC 9421 HTTP signatures (double-knock)"
            }
            span.settings-field__hint {
                "Try the modern dialect on deliveries first, remembering per host "
                "which peers reject it and falling back to draft-cavage."
            }
        }
        (override_toggle("conversation_containers",
            "Own conversation containers (FEP-171b)",
            settings.conversation_containers, config.conversation_containers,
            "Publish a container collection for private conversations rooted \
             here, letting participants' servers backfill the thread. Consuming \
             other servers' containers is always on.", ""))
    }
}

/// `data-show-when` expressions for the image-quality knobs: each encoder's
/// parameters only appear while some rendition actually uses that encoder.
const WHEN_AVIF_FULL: &str = "media_full_processing=avif|media_remote_full_processing=avif";
const WHEN_AVIF_PREVIEW: &str = "media_preview_processing=avif|media_cached_image_processing=avif";
const WHEN_AVIF: &str = "media_full_processing=avif|media_remote_full_processing=avif\
                         |media_preview_processing=avif|media_cached_image_processing=avif";
const WHEN_JPEG: &str = "media_full_processing=jpeg|media_remote_full_processing=jpeg\
                         |media_preview_processing=jpeg";
const WHEN_JXL: &str = "media_full_processing=jxl|media_remote_full_processing=jxl";
const WHEN_REENCODE: &str =
    "media_full_processing!=passthrough|media_remote_full_processing!=passthrough";

/// How image uploads and incoming image caches are processed. Local
/// avatars/headers are always a Mastodon-compatible photo (never AVIF) and
/// are not configurable here.
#[allow(clippy::too_many_lines, reason = "one flat form section")]
fn image_fields(settings: &InstanceSettings) -> Markup {
    let full = settings.media_full_processing.as_str();
    let remote_full = settings.media_remote_full_processing.as_str();
    let preview = settings.media_preview_processing.as_str();
    let cached = settings.media_cached_image_processing.as_str();
    html! {
        h4.admin-section__label { "Renditions" }
        label {
            "Uploaded image originals"
            (full_processing_select("media_full_processing", full, "Keep as uploaded"))
            span.settings-field__hint {
                "\u{201c}Keep as uploaded\u{201d} preserves format and quality, "
                "stripping only EXIF/GPS. Re-encoding also downscales oversized "
                "images. Members' avatars and headers stay JPEG/PNG regardless — "
                "other servers fetch those and not all accept AVIF."
            }
        }
        label {
            "Incoming image originals"
            (full_processing_select("media_remote_full_processing", remote_full,
                "Keep as they arrived"))
            span.settings-field__hint {
                "The same choice for full-size images arriving in other servers' posts."
            }
        }
        label {
            "Preview thumbnails"
            select name="media_preview_processing" {
                option value="avif" selected[preview == "avif"] { "AVIF (smallest)" }
                option value="jpeg" selected[preview == "jpeg"] { "JPEG" }
            }
            span.settings-field__hint {
                "The downscaled preview shown in timelines, local and remote alike."
            }
        }
        label {
            "Incoming emoji, avatar & link-card images"
            select name="media_cached_image_processing" {
                option value="avif" selected[cached == "avif"] { "Re-encode to AVIF (smallest)" }
                option value="passthrough" selected[cached == "passthrough"] {
                    "Keep as they arrived"
                }
            }
            span.settings-field__hint {
                "Served only to this server's own users, so AVIF is safe here. "
                "Animated images are always kept as they arrived."
            }
        }

        h4.admin-section__label { "Encoder settings" }
        div.admin-form__grid {
            label data-show-when=(WHEN_AVIF) {
                "AVIF quality (1-100)"
                input type="number" name="media_avif_quality" min="1" max="100" step="1"
                    value=(settings.media_avif_quality);
            }
            label data-show-when=(WHEN_AVIF_FULL) {
                "AVIF speed, originals (1-10)"
                input type="number" name="media_avif_speed_full" min="1" max="10" step="1"
                    value=(settings.media_avif_speed_full);
                span.settings-field__hint {
                    "Higher is faster but larger. Originals may encode while a "
                    "viewer waits — keep them fast."
                }
            }
            label data-show-when=(WHEN_AVIF_PREVIEW) {
                "AVIF speed, previews (1-10)"
                input type="number" name="media_avif_speed_preview" min="1" max="10" step="1"
                    value=(settings.media_avif_speed_preview);
                span.settings-field__hint {
                    "Off the hot path; can afford a denser (slower) setting."
                }
            }
            label data-show-when=(WHEN_JPEG) {
                "JPEG quality (1-100)"
                input type="number" name="media_jpeg_quality" min="1" max="100" step="1"
                    value=(settings.media_jpeg_quality);
            }
            label data-show-when=(WHEN_JXL) {
                "JPEG XL distance (0-15)"
                input type="number" name="media_jxl_distance" min="0" max="15" step="0.1"
                    value=(settings.media_jxl_distance);
                span.settings-field__hint {
                    "1.0 is visually lossless, 0 mathematically lossless. Needs an "
                    "ffmpeg built with libjxl (the official image has it); only "
                    "Safari displays JXL out of the box today."
                }
            }
            label data-show-when=(WHEN_JXL) {
                "JPEG XL effort (1-9)"
                input type="number" name="media_jxl_effort" min="1" max="9" step="1"
                    value=(settings.media_jxl_effort);
                span.settings-field__hint { "Higher is denser but slower." }
            }
            label data-show-when=(WHEN_REENCODE) {
                "Re-encoded size cap (px, longest edge)"
                input type="number" name="media_max_edge" min="320" max="8192" step="1"
                    value=(settings.media_max_edge);
                span.settings-field__hint {
                    "\u{201c}Keep as uploaded\u{201d} images are never resized."
                }
            }
            label {
                "Largest image upload (MiB)"
                input type="number" name="media_max_image_mb" min="1" max="16" step="1"
                    value=(settings.media_max_image_mb);
            }
        }

        h4.admin-section__label { "Animated GIFs" }
        label {
            "Uploaded GIFs"
            select name="media_local_gif_handling" {
                option value="gifv" selected[settings.media_local_gif_handling == "gifv"] {
                    "Convert to looping video (Mastodon-style gifv)"
                }
                option value="keep" selected[settings.media_local_gif_handling == "keep"] {
                    "Keep as GIF"
                }
            }
            span.settings-field__hint {
                "The looping video is much smaller, but H.264 cannot carry "
                "transparency — GIFs with transparent regions stay GIFs either way."
            }
        }
        label {
            "Incoming GIFs"
            select name="media_remote_gif_handling" {
                option value="keep" selected[settings.media_remote_gif_handling == "keep"] {
                    "Keep as they arrived"
                }
                option value="gifv" selected[settings.media_remote_gif_handling == "gifv"] {
                    "Convert opaque ones to looping video"
                }
                option value="webp" selected[settings.media_remote_gif_handling == "webp"] {
                    "Re-encode to animated WebP (alpha-safe)"
                }
            }
            span.settings-field__hint {
                "Keeping the origin's bytes preserves pixel art and transparency "
                "exactly; the conversions save disk and bandwidth. Animated WebP "
                "needs an ffmpeg built with libwebp_anim (the official image has it)."
            }
        }
    }
}

/// The four-way original-rendition selector, shared by the local-upload and
/// incoming-attachment settings.
fn full_processing_select(name: &str, value: &str, keep_label: &str) -> Markup {
    html! {
        select name=(name) {
            option value="passthrough" selected[value == "passthrough"] {
                (keep_label) " (strip metadata only)"
            }
            option value="avif" selected[value == "avif"] { "Re-encode to AVIF (smallest)" }
            option value="jpeg" selected[value == "jpeg"] { "Re-encode to JPEG" }
            option value="jxl" selected[value == "jxl"] { "Re-encode to JPEG XL (experimental)" }
        }
    }
}

/// Video/audio transcode parameters, upload caps and the heavy-work
/// concurrency knob. Remote video is remux-only by design and unaffected by
/// the encoder settings here.
fn video_fields(settings: &InstanceSettings) -> Markup {
    const PRESETS: &[&str] = &[
        "ultrafast",
        "superfast",
        "veryfast",
        "faster",
        "fast",
        "medium",
        "slow",
    ];
    html! {
        p.settings-field__hint {
            "Applies to uploads that need re-encoding; already-compatible files "
            "are stream-copied untouched, and remote video is only remuxed."
        }
        label {
            "x264 preset"
            select name="media_video_preset" {
                @for preset in PRESETS {
                    option value=(preset) selected[settings.media_video_preset == *preset] {
                        (preset)
                    }
                }
            }
            span.settings-field__hint {
                "Slower presets fit more quality into the same bitrate, at CPU cost."
            }
        }
        label {
            "Rate control"
            select name="media_video_rate_mode" {
                option value="abr" selected[settings.media_video_rate_mode == "abr"] {
                    "Bitrate budget (Mastodon-compatible sizes)"
                }
                option value="crf" selected[settings.media_video_rate_mode == "crf"] {
                    "Constant quality (CRF)"
                }
            }
            span.settings-field__hint {
                "The budget caps file size predictably; constant quality spends "
                "bytes where the picture needs them and usually looks better when "
                "disk is not scarce."
            }
        }
        div.admin-form__grid {
            label data-show-when="media_video_rate_mode=crf" {
                "CRF (0-51, lower is better)"
                input type="number" name="media_video_crf" min="0" max="51" step="1"
                    value=(settings.media_video_crf);
            }
            label {
                "Audio bitrate (kbps)"
                input type="number" name="media_audio_bitrate_kbps" min="32" max="320" step="1"
                    value=(settings.media_audio_bitrate_kbps);
                span.settings-field__hint { "AAC, for transcoded sound tracks." }
            }
            label {
                "Largest video/audio upload (MiB)"
                input type="number" name="media_max_av_mb" min="1" max="99" step="1"
                    value=(settings.media_max_av_mb);
                span.settings-field__hint {
                    "Also the size the bitrate budget squeezes long videos into."
                }
            }
            label {
                "Soundless video treated as a GIF, up to (s)"
                input type="number" name="media_gifv_max_seconds" min="1" max="3600" step="1"
                    value=(settings.media_gifv_max_seconds);
                span.settings-field__hint {
                    "Shorter soundless videos autoplay and loop (Mastodon's gifv); "
                    "longer ones present as normal videos."
                }
            }
            label {
                "Concurrent heavy media jobs"
                input type="number" name="media_processing_concurrency" min="0" max="64" step="1"
                    value=(settings.media_processing_concurrency);
                span.settings-field__hint {
                    "How many re-encodes (ffmpeg or image) run at once; further "
                    "jobs queue. 0 = half the CPU cores. Applied at the next restart."
                }
            }
        }
    }
}

fn retention_fields(settings: &InstanceSettings) -> Markup {
    html! {
        p.settings-field__hint {
            "Only cached copies of other servers' content are evicted — posts "
            "and your own members' uploads are never touched. Evicted media is "
            "refetched on demand."
        }
        h4.admin-section__label { "Remote video" }
        label {
            "Cache budget per video (MiB)"
            input type="number" name="remote_video_max_mb" min="0" step="1"
                value=(settings.remote_video_max_mb);
            span.settings-field__hint {
                "Long-form remote video (PeerTube) is downloaded on its first "
                "play — never just from a timeline — remuxed and served from "
                "here. Videos over this budget stay uncached, and play only for "
                "viewers who opted in to direct remote fetches. 0 turns remote "
                "video caching off."
            }
        }
        div.admin-form__grid data-show-when="remote_video_max_mb!=0" {
            label {
                "Preferred resolution cap (px tall)"
                input type="number" name="remote_video_max_height" min="0" step="1"
                    value=(settings.remote_video_max_height);
                span.settings-field__hint {
                    "The tallest rendition at or under this height is cached. "
                    "0 = no cap."
                }
            }
            label {
                "Retention (days)"
                input type="number" name="media_video_retention_days" min="0" step="1"
                    value=[settings.media_video_retention_days]
                    placeholder="same as images";
                span.settings-field__hint {
                    "Videos are big and cheap to refetch, so they can go on a "
                    "shorter clock than images. Blank follows the image period; "
                    "0 keeps them forever."
                }
            }
            label {
                "Cache size cap (GiB)"
                input type="number" name="media_video_cache_max_gb" min="0" step="1"
                    value=(settings.media_video_cache_max_gb);
                span.settings-field__hint {
                    "Least recently watched evicted first. 0 = no cap."
                }
            }
        }
        h4.admin-section__label { "Remote images" }
        div.admin-form__grid {
            label {
                "Attachment retention (days)"
                input type="number" name="media_cache_retention_days" min="0" step="1"
                    value=(settings.media_cache_retention_days);
                span.settings-field__hint { "0 disables eviction." }
            }
            label {
                "Avatar & header retention (days)"
                input type="number" name="media_profile_retention_days" min="0" step="1"
                    value=(settings.media_profile_retention_days);
                span.settings-field__hint { "0 keeps them forever." }
            }
            label {
                "Link-card image retention (days)"
                input type="number" name="media_card_retention_days" min="0" step="1"
                    value=(settings.media_card_retention_days);
                span.settings-field__hint { "0 keeps them forever." }
            }
            label {
                "Custom emoji retention (days)"
                input type="number" name="media_emoji_retention_days" min="0" step="1"
                    value=(settings.media_emoji_retention_days);
                span.settings-field__hint { "0 keeps them forever." }
            }
            label {
                "Image cache size cap (GiB)"
                input type="number" name="media_cache_max_gb" min="0" step="1"
                    value=(settings.media_cache_max_gb);
                span.settings-field__hint { "Oldest evicted first. 0 = no cap." }
            }
        }
        h4.admin-section__label { "Sign-in log" }
        label {
            "IP & sign-in log retention (days)"
            input type="number" name="ip_retention_days" min="1" step="1"
                value=(settings.ip_retention_days);
            span.settings-field__hint {
                "Stored IP addresses and sign-in log entries older than this are "
                "scrubbed daily. Must be at least 1."
            }
        }
    }
}

/// The status-translation cache and backend-protection knobs. Rendered
/// whether or not a backend is configured — the backend itself lives in the
/// config file.
fn translation_fields(settings: &InstanceSettings, configured: bool) -> Markup {
    html! {
        @if !configured {
            p.settings-field__hint {
                "No [translation] section in plamenu.toml, so translation is off "
                "and these values are inert until a backend is configured there."
            }
        }
        div.admin-form__grid {
            label {
                "Cached translation retention (days)"
                input type="number" name="translation_cache_retention_days" min="0" step="1"
                    value=(settings.translation_cache_retention_days);
                span.settings-field__hint {
                    "Unused translations are pruned after this. Editing or "
                    "deleting a post always invalidates its translations. "
                    "0 keeps them until then."
                }
            }
            label {
                "Cached translation row cap"
                input type="number" name="translation_cache_max_rows" min="0" step="1"
                    value=(settings.translation_cache_max_rows);
                span.settings-field__hint {
                    "Least recently used evicted beyond it. 0 = no cap."
                }
            }
            label {
                "Backend concurrency"
                input type="number" name="translation_backend_concurrency" min="1" step="1"
                    value=(settings.translation_backend_concurrency);
                span.settings-field__hint {
                    "Match a self-hosted backend's parallel slots (2 for the "
                    "default llama.cpp setup); commercial APIs tolerate more."
                }
            }
            label {
                "Per-user translations per hour"
                input type="number" name="translation_user_rate_limit_per_hour" min="0" step="1"
                    value=(settings.translation_user_rate_limit_per_hour);
                span.settings-field__hint {
                    "Only translations that reach the backend count — cache hits "
                    "are free. 0 disables the limit."
                }
            }
        }
        label {
            span.admin-check {
                input type="checkbox" name="translation_refresh_on_provider_change" value="1"
                    checked[settings.translation_refresh_on_provider_change];
                " Re-translate cached posts when the backend changes"
            }
            span.settings-field__hint {
                "Off, translations cached under a previous backend are served with "
                "their original attribution until they expire."
            }
        }
    }
}

fn rate_limit_fields(settings: &InstanceSettings) -> Markup {
    html! {
        label {
            span.admin-check {
                input type="checkbox" name="rate_limiting_enabled" value="1"
                    checked[settings.rate_limiting_enabled];
                " Enable rate limiting"
            }
            span.settings-field__hint {
                "The counts are editable; the windows are fixed, matching Mastodon's."
            }
        }
        div.admin-form__grid data-show-when="rate_limiting_enabled" {
            (limit_field("rate_limit_authenticated_api", "API requests per user",
                settings.rate_limit_authenticated_api, "Across /api, per 5 min."))
            (limit_field("rate_limit_per_token_api", "API requests per token",
                settings.rate_limit_per_token_api, "Across /api, per 5 min."))
            (limit_field("rate_limit_unauthenticated_api", "API requests per IP, signed out",
                settings.rate_limit_unauthenticated_api, "Across /api, per 5 min."))
            (limit_field("rate_limit_paging", "Paged requests",
                settings.rate_limit_paging, "Per user or IP, per 15 min."))
            (limit_field("rate_limit_api_media", "Media uploads per user",
                settings.rate_limit_api_media, "Per 30 min."))
            (limit_field("rate_limit_api_delete", "Post deletions per user",
                settings.rate_limit_api_delete, "Deletes and un-boosts, per 30 min."))
            (limit_field("rate_limit_api_sign_up", "API sign-ups per IP",
                settings.rate_limit_api_sign_up, "Per 30 min."))
            (limit_field("rate_limit_sign_up_web", "Web sign-up attempts per IP",
                settings.rate_limit_sign_up_web, "Per 5 min."))
            (limit_field("rate_limit_app_registrations", "App registrations per IP",
                settings.rate_limit_app_registrations, "Per 10 min."))
            (limit_field("rate_limit_login_attempts", "Login attempts",
                settings.rate_limit_login_attempts,
                "Per IP per 5 min, and per e-mail address per hour."))
            (limit_field("rate_limit_password_resets", "Password resets per e-mail",
                settings.rate_limit_password_resets, "Per 30 min."))
        }
    }
}

fn appearance_fields(settings: &InstanceSettings) -> Markup {
    html! {
        label {
            "Custom CSS"
            textarea name="custom_css" rows="10" spellcheck="false" {
                (settings.custom_css)
            }
            span.settings-field__hint {
                "Served at /custom.css after the built-in styles, so any selector "
                "here wins. Overriding the :root design tokens re-themes the whole "
                "client."
            }
        }
    }
}

fn email_fields(smtp_configured: bool, csrf: &str) -> Markup {
    html! {
        @if smtp_configured {
            form.admin-form method="post" action="/web/admin/settings/test-email" {
                input type="hidden" name="csrf" value=(csrf);
                p.settings-field__hint {
                    "An SMTP relay is configured ([smtp] in plamenu.toml). Send "
                    "yourself a test message to check it end to end — the relay's "
                    "verdict is reported here."
                }
                div.admin-actions { button type="submit" { "Send a test e-mail" } }
            }
        } @else {
            p.settings-field__hint {
                "No [smtp] section in plamenu.toml. Confirmation and password-reset "
                "e-mails are unavailable until a relay is configured there "
                "(file-only; needs a restart)."
            }
        }
    }
}

fn maintenance_fields(csrf: &str) -> Markup {
    html! {
        form.admin-form method="post" action="/web/admin/settings/backfill-sizes" {
            input type="hidden" name="csrf" value=(csrf);
            p.settings-field__hint {
                "One-shot migration aid: stat every stored file whose byte size is "
                "still unrecorded, so the storage metrics account for media from "
                "before sizes were tracked. Runs in the background; the result "
                "lands in the server log."
            }
            div.admin-actions { button type="submit" { "Backfill stored file sizes" } }
        }
    }
}

fn site_images_fields(domain: &str, uploads: &[SiteUpload], csrf: &str) -> Markup {
    html! {
        div.admin-list {
            @for (var, label, hint) in UPLOAD_SLOTS {
                (upload_slot(domain, var, label, hint,
                    uploads.iter().find(|u| u.var == *var), csrf))
            }
        }
    }
}

// ---- Shared field renderers ---------------------------------------------

/// A three-way override select for a config-bridged boolean (O4): keep the
/// plamenu.toml bootstrap default (stored as NULL) or force the flag on/off,
/// applying live without a restart.
fn override_toggle(
    name: &str,
    label: &str,
    value: Option<bool>,
    config_default: bool,
    hint: &str,
    when: &str,
) -> Markup {
    let default_word = if config_default { "on" } else { "off" };
    // A toggle that only bites under another one is also indented under it.
    let nested = !when.is_empty();
    html! {
        label.admin-form__nested[nested] data-show-when=[nested.then_some(when)] {
            (label)
            select name=(name) {
                option value="" selected[value.is_none()] {
                    "Server default (currently " (default_word) ")"
                }
                option value="on" selected[value == Some(true)] { "Enabled" }
                option value="off" selected[value == Some(false)] { "Disabled" }
            }
            span.settings-field__hint { (hint) }
        }
    }
}

fn disclosure_field(
    name: &str,
    label: &str,
    value: DomainBlocksDisclosure,
    hint: &str,
    when: &str,
) -> Markup {
    html! {
        label data-show-when=[(!when.is_empty()).then_some(when)] {
            (label)
            select name=(name) {
                option value="disabled" selected[value == DomainBlocksDisclosure::Disabled] {
                    "Nobody"
                }
                option value="users" selected[value == DomainBlocksDisclosure::Users] {
                    "Signed-in users"
                }
                option value="all" selected[value == DomainBlocksDisclosure::All] { "Everyone" }
            }
            span.settings-field__hint { (hint) }
        }
    }
}

fn limit_field(name: &str, label: &str, value: i32, hint: &str) -> Markup {
    html! {
        label {
            (label)
            input type="number" name=(name) value=(value) min="1" step="1" required;
            @if !hint.is_empty() { span.settings-field__hint { (hint) } }
        }
    }
}

/// The four Mastodon `SiteUpload` slots, with the admin-facing wording.
const UPLOAD_SLOTS: &[(&str, &str, &str)] = &[
    (
        "thumbnail",
        "Server thumbnail",
        "Shown in link previews and the instance API (cropped to 1200×630).",
    ),
    ("mascot", "Mascot", "Shown on the sign-in page."),
    (
        "favicon",
        "Favicon",
        "Served at /favicon.ico (resized to 16/32/48).",
    ),
    (
        "app_icon",
        "App icon",
        "Served through the instance API for client home screens.",
    ),
];

fn upload_slot(
    domain: &str,
    var: &str,
    label: &str,
    hint: &str,
    upload: Option<&SiteUpload>,
    csrf: &str,
) -> Markup {
    html! {
        article.admin-record {
            div.admin-record__head {
                strong { (label) }
                span.admin-table__sub { (hint) }
            }
            @if let Some(upload) = upload {
                div.admin-upload__current {
                    img.admin-upload__preview
                        src=(format!("https://{domain}/media/{}", upload.file_name))
                        alt=(upload.description);
                    span.admin-table__sub {
                        (upload.width) "×" (upload.height) ", " (upload.content_type)
                    }
                }
                @if var == "thumbnail" {
                    form.admin-form method="post"
                        action="/web/admin/site-uploads/thumbnail/description" {
                        input type="hidden" name="csrf" value=(csrf);
                        label {
                            "Description"
                            input type="text" name="description" value=(upload.description)
                                maxlength=(DESCRIPTION_LIMIT);
                        }
                        button type="submit" { "Save description" }
                    }
                }
                form method="post" action=(format!("/web/admin/site-uploads/{var}/delete")) {
                    input type="hidden" name="csrf" value=(csrf);
                    button.admin-danger type="submit" { "Remove" }
                }
            }
            form.admin-form method="post" enctype="multipart/form-data"
                action=(format!("/web/admin/site-uploads/{var}")) {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    @if upload.is_some() { "Replace image" } @else { "Upload image" }
                    input type="file" name="image"
                        accept="image/png,image/jpeg,image/gif,image/webp" required;
                }
                button type="submit" { "Upload" }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    csrf: String,
    /// Which group this submit carries; absent = the whole set.
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    site_title: String,
    #[serde(default)]
    site_short_description: String,
    #[serde(default)]
    site_extended_description: String,
    #[serde(default)]
    site_contact_username: String,
    #[serde(default)]
    site_contact_email: String,
    #[serde(default)]
    custom_css: String,
    #[serde(default)]
    registrations_mode: String,
    #[serde(default)]
    group_creation_policy: String,
    #[serde(default)]
    min_age: String,
    /// Checkbox: present when checked, absent when not.
    #[serde(default)]
    rate_limiting_enabled: Option<String>,
    #[serde(default)]
    rate_limit_authenticated_api: String,
    #[serde(default)]
    rate_limit_per_token_api: String,
    #[serde(default)]
    rate_limit_unauthenticated_api: String,
    #[serde(default)]
    rate_limit_api_media: String,
    #[serde(default)]
    rate_limit_api_delete: String,
    #[serde(default)]
    rate_limit_api_sign_up: String,
    #[serde(default)]
    rate_limit_app_registrations: String,
    #[serde(default)]
    rate_limit_paging: String,
    #[serde(default)]
    rate_limit_login_attempts: String,
    #[serde(default)]
    rate_limit_password_resets: String,
    #[serde(default)]
    rate_limit_sign_up_web: String,
    #[serde(default)]
    max_characters: String,
    #[serde(default)]
    max_characters_long_form: String,
    #[serde(default)]
    max_media_attachments: String,
    #[serde(default)]
    poll_max_options: String,
    #[serde(default)]
    personal_emoji_limit: String,
    #[serde(default)]
    webxdc_bundle_mb: String,
    #[serde(default)]
    webxdc_expanded_mb: String,
    #[serde(default)]
    webxdc_file_mb: String,
    #[serde(default)]
    webxdc_session_mb: String,
    #[serde(default)]
    webxdc_account_mb: String,
    #[serde(default)]
    webxdc_total_mb: String,

    #[serde(default)]
    custom_emoji_max_file_size_kb: String,
    /// Checkboxes: present when checked, absent when not.
    #[serde(default)]
    activity_api_enabled: Option<String>,
    #[serde(default)]
    trends_enabled: Option<String>,
    #[serde(default)]
    trendable_by_default: Option<String>,
    #[serde(default)]
    peers_api_enabled: Option<String>,
    #[serde(default)]
    profile_directory: Option<String>,
    #[serde(default)]
    landing_page: Option<String>,
    #[serde(default)]
    landing_show_stats: Option<String>,
    #[serde(default)]
    show_domain_blocks: String,
    #[serde(default)]
    show_domain_blocks_rationale: String,
    #[serde(default)]
    media_cache_retention_days: String,
    #[serde(default)]
    media_full_processing: String,
    #[serde(default)]
    media_preview_processing: String,
    #[serde(default)]
    media_remote_full_processing: String,
    #[serde(default)]
    media_cached_image_processing: String,
    #[serde(default)]
    remote_video_max_mb: String,
    #[serde(default)]
    remote_video_max_height: String,
    #[serde(default)]
    media_local_gif_handling: String,
    #[serde(default)]
    media_remote_gif_handling: String,
    #[serde(default)]
    media_gifv_max_seconds: String,
    #[serde(default)]
    media_avif_quality: String,
    #[serde(default)]
    media_avif_speed_full: String,
    #[serde(default)]
    media_avif_speed_preview: String,
    #[serde(default)]
    media_jpeg_quality: String,
    #[serde(default)]
    media_max_edge: String,
    #[serde(default)]
    media_jxl_distance: String,
    #[serde(default)]
    media_jxl_effort: String,
    #[serde(default)]
    media_video_preset: String,
    #[serde(default)]
    media_video_rate_mode: String,
    #[serde(default)]
    media_video_crf: String,
    #[serde(default)]
    media_audio_bitrate_kbps: String,
    #[serde(default)]
    media_max_image_mb: String,
    #[serde(default)]
    media_max_av_mb: String,
    #[serde(default)]
    media_processing_concurrency: String,
    /// Empty = follow `media_cache_retention_days` (stored as NULL).
    #[serde(default)]
    media_video_retention_days: String,
    #[serde(default)]
    media_profile_retention_days: String,
    #[serde(default)]
    media_card_retention_days: String,
    #[serde(default)]
    media_emoji_retention_days: String,
    #[serde(default)]
    media_cache_max_gb: String,
    #[serde(default)]
    media_video_cache_max_gb: String,
    /// O4 overrides: empty = follow the config file (stored as NULL).
    #[serde(default)]
    authorized_fetch: String,
    #[serde(default)]
    authorized_fetch_unsigned_profile: String,
    /// Checkboxes: present when checked, absent when not.
    #[serde(default)]
    emit_integrity_proofs: Option<String>,
    #[serde(default)]
    emit_rfc9421: Option<String>,
    #[serde(default)]
    conversation_containers: String,
    #[serde(default)]
    ip_retention_days: String,
    #[serde(default)]
    timeline_preview_federated: Option<String>,
    #[serde(default)]
    timeline_preview_local: Option<String>,
    #[serde(default)]
    timeline_preview_tag: Option<String>,
    #[serde(default)]
    public_search: Option<String>,
    #[serde(default)]
    anon_trends: Option<String>,
    #[serde(default)]
    anon_directory: Option<String>,
    #[serde(default)]
    anon_directory_federated: Option<String>,
    #[serde(default)]
    anon_groups: Option<String>,
    #[serde(default)]
    public_timeline_replies: Option<String>,
    #[serde(default)]
    boost_collapse: Option<String>,
    #[serde(default)]
    boost_collapse_lookback: String,
    #[serde(default)]
    translation_cache_retention_days: String,
    #[serde(default)]
    translation_cache_max_rows: String,
    #[serde(default)]
    translation_backend_concurrency: String,
    #[serde(default)]
    translation_user_rate_limit_per_hour: String,
    #[serde(default)]
    translation_refresh_on_provider_change: Option<String>,
}

/// Canonicalises a full-rendition processing value (`media_full_processing` /
/// `media_remote_full_processing`), ignoring anything not in the allowed set
/// (defaults to `passthrough`).
fn canonical_full_processing(value: &str) -> &'static str {
    match value.trim() {
        "avif" => "avif",
        "jpeg" => "jpeg",
        "jxl" => "jxl",
        _ => "passthrough",
    }
}

/// Canonicalises the `media_cached_image_processing` value (defaults to
/// `avif`).
fn canonical_cached_image_processing(value: &str) -> &'static str {
    match value.trim() {
        "passthrough" => "passthrough",
        _ => "avif",
    }
}

/// Canonicalises the `media_preview_processing` value (defaults to `avif`).
fn canonical_preview_processing(value: &str) -> &'static str {
    match value.trim() {
        "jpeg" => "jpeg",
        _ => "avif",
    }
}

/// Age gate: a non-negative integer; 0 (or blank) leaves it off.
fn parse_min_age(value: &str) -> i32 {
    value.trim().parse::<i32>().unwrap_or(0).clamp(0, 120)
}

/// A bounded numeric knob: unparseable input falls back to `default` (the
/// stored value, so a field a submit did not carry keeps what it had),
/// out-of-range input is clamped.
fn parse_clamped(value: &str, min: i32, max: i32, default: i32) -> i32 {
    value
        .trim()
        .parse::<i32>()
        .unwrap_or(default)
        .clamp(min, max)
}

/// [`parse_clamped`] for the one fractional knob (JPEG XL distance).
fn parse_clamped_f32(value: &str, min: f32, max: f32, default: f32) -> f32 {
    value
        .trim()
        .parse::<f32>()
        .unwrap_or(default)
        .clamp(min, max)
}

/// A blank-able day count: blank → `None` (inherit), otherwise a
/// non-negative day count (invalid input also inherits).
fn parse_optional_days(value: &str) -> Option<i32> {
    value.trim().parse::<i32>().ok().filter(|days| *days >= 0)
}

fn canonical_local_gif(value: &str) -> &'static str {
    match value.trim() {
        "keep" => "keep",
        _ => "gifv",
    }
}

fn canonical_remote_gif(value: &str) -> &'static str {
    match value.trim() {
        "gifv" => "gifv",
        "webp" => "webp",
        _ => "keep",
    }
}

fn canonical_rate_mode(value: &str) -> &'static str {
    match value.trim() {
        "crf" => "crf",
        _ => "abr",
    }
}

fn canonical_video_preset(value: &str) -> &'static str {
    match value.trim() {
        "ultrafast" => "ultrafast",
        "superfast" => "superfast",
        "faster" => "faster",
        "fast" => "fast",
        "medium" => "medium",
        "slow" => "slow",
        _ => "veryfast",
    }
}

/// A throttle count from the form: a positive integer (upper-bounded only to
/// keep the value inside `i32`).
fn parse_limit(value: &str) -> Option<i32> {
    value.trim().parse().ok().filter(|count| *count >= 1)
}

/// The rate-limit counts, in form order; `None` when any fails to parse.
fn parse_rate_limits(form: &SettingsForm) -> Option<[i32; 11]> {
    parse_all(&[
        &form.rate_limit_authenticated_api,
        &form.rate_limit_per_token_api,
        &form.rate_limit_unauthenticated_api,
        &form.rate_limit_api_media,
        &form.rate_limit_api_delete,
        &form.rate_limit_api_sign_up,
        &form.rate_limit_app_registrations,
        &form.rate_limit_paging,
        &form.rate_limit_login_attempts,
        &form.rate_limit_password_resets,
        &form.rate_limit_sign_up_web,
    ])
}

/// The posting caps, in form order; `None` when any fails to parse.
fn parse_posting_limits(form: &SettingsForm) -> Option<[i32; 4]> {
    parse_all(&[
        &form.max_characters,
        &form.max_characters_long_form,
        &form.max_media_attachments,
        &form.poll_max_options,
    ])
}

/// [`parse_limit`] across a fixed-size slice of fields, all-or-nothing.
fn parse_all<const N: usize>(values: &[&String; N]) -> Option<[i32; N]> {
    let mut parsed = [0_i32; N];
    for (slot, value) in parsed.iter_mut().zip(values) {
        *slot = parse_limit(value)?;
    }
    Some(parsed)
}

/// A non-negative integer field where 0 is meaningful (the remote-video
/// budget/height knobs: 0 = off / no cap).
fn parse_non_negative(value: &str) -> Option<i32> {
    value.trim().parse::<i32>().ok().filter(|n| *n >= 0)
}

/// Parses a three-way override select (O4): empty = follow the config file
/// (stored as NULL), `on`/`off` = forced. Anything else counts as empty.
fn parse_override(value: &str) -> Option<bool> {
    match value.trim() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// A [`SettingsUpdate`] that re-saves the stored values verbatim — the base
/// every section overlay is applied to, so a section's form only has to carry
/// its own fields.
#[allow(clippy::too_many_lines, reason = "one flat field-for-field copy")]
fn base_update(s: &InstanceSettings) -> SettingsUpdate<'_> {
    SettingsUpdate {
        site_title: s.site_title.as_str(),
        site_short_description: s.site_short_description.as_str(),
        site_extended_description: s.site_extended_description.as_str(),
        site_contact_username: s.site_contact_username.as_str(),
        site_contact_email: s.site_contact_email.as_str(),
        custom_css: s.custom_css.as_str(),
        registrations_mode: s.registrations_mode(),
        rate_limiting_enabled: s.rate_limiting_enabled,
        rate_limit_authenticated_api: s.rate_limit_authenticated_api,
        rate_limit_per_token_api: s.rate_limit_per_token_api,
        rate_limit_unauthenticated_api: s.rate_limit_unauthenticated_api,
        rate_limit_api_media: s.rate_limit_api_media,
        rate_limit_api_delete: s.rate_limit_api_delete,
        rate_limit_api_sign_up: s.rate_limit_api_sign_up,
        rate_limit_app_registrations: s.rate_limit_app_registrations,
        rate_limit_paging: s.rate_limit_paging,
        rate_limit_login_attempts: s.rate_limit_login_attempts,
        rate_limit_password_resets: s.rate_limit_password_resets,
        rate_limit_sign_up_web: s.rate_limit_sign_up_web,
        max_characters: s.max_characters,
        max_characters_long_form: s.max_characters_long_form,
        max_media_attachments: s.max_media_attachments,
        poll_max_options: s.poll_max_options,
        activity_api_enabled: s.activity_api_enabled,
        peers_api_enabled: s.peers_api_enabled,
        profile_directory: s.profile_directory,
        trends_enabled: s.trends_enabled,
        trendable_by_default: s.trendable_by_default,
        landing_page: s.landing_page,
        landing_show_stats: s.landing_show_stats,
        show_domain_blocks: s.show_domain_blocks(),
        show_domain_blocks_rationale: s.show_domain_blocks_rationale(),
        min_age: s.min_age,
        media_cache_retention_days: s.media_cache_retention_days,
        media_full_processing: s.media_full_processing.as_str(),
        media_preview_processing: s.media_preview_processing.as_str(),
        media_remote_full_processing: s.media_remote_full_processing.as_str(),
        media_cached_image_processing: s.media_cached_image_processing.as_str(),
        remote_video_max_mb: s.remote_video_max_mb,
        remote_video_max_height: s.remote_video_max_height,
        media_local_gif_handling: s.media_local_gif_handling.as_str(),
        media_remote_gif_handling: s.media_remote_gif_handling.as_str(),
        media_gifv_max_seconds: s.media_gifv_max_seconds,
        media_avif_quality: s.media_avif_quality,
        media_avif_speed_full: s.media_avif_speed_full,
        media_avif_speed_preview: s.media_avif_speed_preview,
        media_jpeg_quality: s.media_jpeg_quality,
        media_max_edge: s.media_max_edge,
        media_jxl_distance: s.media_jxl_distance,
        media_jxl_effort: s.media_jxl_effort,
        media_video_preset: s.media_video_preset.as_str(),
        media_video_rate_mode: s.media_video_rate_mode.as_str(),
        media_video_crf: s.media_video_crf,
        media_audio_bitrate_kbps: s.media_audio_bitrate_kbps,
        media_max_image_mb: s.media_max_image_mb,
        media_max_av_mb: s.media_max_av_mb,
        media_processing_concurrency: s.media_processing_concurrency,
        media_video_retention_days: s.media_video_retention_days,
        media_profile_retention_days: s.media_profile_retention_days,
        media_card_retention_days: s.media_card_retention_days,
        media_emoji_retention_days: s.media_emoji_retention_days,
        media_cache_max_gb: s.media_cache_max_gb,
        media_video_cache_max_gb: s.media_video_cache_max_gb,
        group_creation_policy: s.group_creation_policy(),
        authorized_fetch: s.authorized_fetch,
        authorized_fetch_unsigned_profile: s.authorized_fetch_unsigned_profile,
        emit_integrity_proofs: s.emit_integrity_proofs,
        emit_rfc9421: s.emit_rfc9421,
        conversation_containers: s.conversation_containers,
        ip_retention_days: s.ip_retention_days,
        timeline_preview_federated: s.timeline_preview_federated,
        timeline_preview_local: s.timeline_preview_local,
        timeline_preview_tag: s.timeline_preview_tag,
        public_search: s.public_search,
        anon_trends: s.anon_trends,
        anon_directory: s.anon_directory,
        anon_directory_federated: s.anon_directory_federated,
        anon_groups: s.anon_groups,
        public_timeline_replies: s.public_timeline_replies,
        boost_collapse: s.boost_collapse,
        boost_collapse_lookback: s.boost_collapse_lookback,
        translation_cache_retention_days: s.translation_cache_retention_days,
        translation_cache_max_rows: s.translation_cache_max_rows,
        translation_backend_concurrency: s.translation_backend_concurrency,
        translation_user_rate_limit_per_hour: s.translation_user_rate_limit_per_hour,
        translation_refresh_on_provider_change: s.translation_refresh_on_provider_change,
    }
}

/// `POST /web/admin/settings` — save one section (or, for a submit that names
/// none, the full set as before). Fields outside the submitted section keep
/// their stored values, so a stale or partial page can never reset them.
#[allow(clippy::too_many_lines, reason = "one linear form-to-update mapping")]
pub async fn save(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<SettingsForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(section) = Section::parse(form.section.as_deref().unwrap_or_default()) else {
        return Ok(redirect_settings("error"));
    };

    if section == Section::Webxdc {
        let limits = plamenu_db::webxdc::Limits {
            bundle_mb: form.webxdc_bundle_mb.parse().unwrap_or(0),
            expanded_mb: form.webxdc_expanded_mb.parse().unwrap_or(0),
            file_mb: form.webxdc_file_mb.parse().unwrap_or(0),
            session_mb: form.webxdc_session_mb.parse().unwrap_or(0),
            account_mb: form.webxdc_account_mb.parse().unwrap_or(0),
            total_mb: form.webxdc_total_mb.parse().unwrap_or(0),
        };
        if !limits.valid() {
            return Ok(redirect_section("error", section));
        }
        plamenu_db::webxdc::set_limits(&state.pool, limits)
            .await
            .map_err(api_err)?;
        return Ok(redirect_section("applied", section));
    }

    if section == Section::CustomEmoji {
        let (Some(personal_limit), Some(max_file_size_kb)) = (
            parse_non_negative(&form.personal_emoji_limit),
            parse_non_negative(&form.custom_emoji_max_file_size_kb)
                .filter(|size| (1..=16_384).contains(size)),
        ) else {
            return Ok(redirect_section("error", section));
        };
        custom_emoji::set_settings(&state.pool, personal_limit, max_file_size_kb)
            .await
            .map_err(api_err)?;
        return Ok(redirect_section("applied", section));
    }

    let current = instance_settings::get(&state.pool).await.map_err(api_err)?;
    let mut update = base_update(&current);
    // Owned values the update borrows; declared here so they outlive it.
    let contact_username;

    if section.covers(Section::Branding) {
        let site_title = form.site_title.trim();
        let short_description = form.site_short_description.trim();
        contact_username = form
            .site_contact_username
            .trim()
            .trim_start_matches('@')
            .to_owned();
        if site_title.is_empty() || short_description.chars().count() > DESCRIPTION_LIMIT {
            return Ok(redirect_section("error", section));
        }
        // Mirror Mastodon's `existing_username` validation: a non-empty
        // contact must name a real local account.
        if !contact_username.is_empty()
            && account::find_local_by_username(&state.pool, &contact_username)
                .await
                .map_err(api_err)?
                .is_none()
        {
            return Ok(redirect_section("no_such_account", section));
        }
        update.site_title = site_title;
        update.site_short_description = short_description;
        update.site_extended_description = form.site_extended_description.trim();
        update.site_contact_username = &contact_username;
        update.site_contact_email = form.site_contact_email.trim();
    }

    if section.covers(Section::Landing) {
        update.landing_page = form.landing_page.is_some();
        update.landing_show_stats = form.landing_show_stats.is_some();
    }

    if section.covers(Section::Registrations) {
        update.registrations_mode = RegistrationsMode::parse(&form.registrations_mode);
        update.min_age = parse_min_age(&form.min_age);
    }

    if section.covers(Section::Groups) {
        update.group_creation_policy = GroupCreationPolicy::parse(&form.group_creation_policy);
    }

    if section.covers(Section::Posting) {
        let Some(
            [
                max_characters,
                max_characters_long_form,
                max_media_attachments,
                poll_max_options,
            ],
        ) = parse_posting_limits(&form)
        else {
            return Ok(redirect_section("error", section));
        };
        update.max_characters = max_characters;
        update.max_characters_long_form = max_characters_long_form;
        update.max_media_attachments = max_media_attachments;
        update.poll_max_options = poll_max_options;
    }

    if section.covers(Section::Timelines) {
        update.boost_collapse = form.boost_collapse.is_some();
        update.boost_collapse_lookback = parse_clamped(
            &form.boost_collapse_lookback,
            0,
            500,
            current.boost_collapse_lookback,
        );
    }

    if section.covers(Section::Discovery) {
        update.peers_api_enabled = form.peers_api_enabled.is_some();
        update.activity_api_enabled = form.activity_api_enabled.is_some();
        update.profile_directory = form.profile_directory.is_some();
        update.trends_enabled = form.trends_enabled.is_some();
        update.trendable_by_default = form.trendable_by_default.is_some();
        update.public_timeline_replies = form.public_timeline_replies.is_some();
        update.show_domain_blocks = DomainBlocksDisclosure::parse(&form.show_domain_blocks);
        update.show_domain_blocks_rationale =
            DomainBlocksDisclosure::parse(&form.show_domain_blocks_rationale);
    }

    if section.covers(Section::Anonymous) {
        update.timeline_preview_federated = form.timeline_preview_federated.is_some();
        update.timeline_preview_local = form.timeline_preview_local.is_some();
        update.timeline_preview_tag = form.timeline_preview_tag.is_some();
        update.public_search = form.public_search.is_some();
        update.anon_trends = form.anon_trends.is_some();
        update.anon_directory = form.anon_directory.is_some();
        update.anon_directory_federated = form.anon_directory_federated.is_some();
        update.anon_groups = form.anon_groups.is_some();
    }

    if section.covers(Section::Federation) {
        update.authorized_fetch = parse_override(&form.authorized_fetch);
        update.authorized_fetch_unsigned_profile =
            parse_override(&form.authorized_fetch_unsigned_profile);
        update.emit_integrity_proofs = form.emit_integrity_proofs.is_some();
        update.emit_rfc9421 = form.emit_rfc9421.is_some();
        update.conversation_containers = parse_override(&form.conversation_containers);
    }

    if section.covers(Section::Images) {
        update.media_full_processing = canonical_full_processing(&form.media_full_processing);
        update.media_remote_full_processing =
            canonical_full_processing(&form.media_remote_full_processing);
        update.media_preview_processing =
            canonical_preview_processing(&form.media_preview_processing);
        update.media_cached_image_processing =
            canonical_cached_image_processing(&form.media_cached_image_processing);
        update.media_local_gif_handling = canonical_local_gif(&form.media_local_gif_handling);
        update.media_remote_gif_handling = canonical_remote_gif(&form.media_remote_gif_handling);
        update.media_avif_quality =
            parse_clamped(&form.media_avif_quality, 1, 100, current.media_avif_quality);
        update.media_avif_speed_full = parse_clamped(
            &form.media_avif_speed_full,
            1,
            10,
            current.media_avif_speed_full,
        );
        update.media_avif_speed_preview = parse_clamped(
            &form.media_avif_speed_preview,
            1,
            10,
            current.media_avif_speed_preview,
        );
        update.media_jpeg_quality =
            parse_clamped(&form.media_jpeg_quality, 1, 100, current.media_jpeg_quality);
        update.media_max_edge =
            parse_clamped(&form.media_max_edge, 320, 8192, current.media_max_edge);
        update.media_jxl_distance = parse_clamped_f32(
            &form.media_jxl_distance,
            0.0,
            15.0,
            current.media_jxl_distance,
        );
        update.media_jxl_effort =
            parse_clamped(&form.media_jxl_effort, 1, 9, current.media_jxl_effort);
        update.media_max_image_mb =
            parse_clamped(&form.media_max_image_mb, 1, 16, current.media_max_image_mb);
    }

    if section.covers(Section::Video) {
        update.media_video_preset = canonical_video_preset(&form.media_video_preset);
        update.media_video_rate_mode = canonical_rate_mode(&form.media_video_rate_mode);
        update.media_video_crf =
            parse_clamped(&form.media_video_crf, 0, 51, current.media_video_crf);
        update.media_audio_bitrate_kbps = parse_clamped(
            &form.media_audio_bitrate_kbps,
            32,
            320,
            current.media_audio_bitrate_kbps,
        );
        update.media_gifv_max_seconds = parse_clamped(
            &form.media_gifv_max_seconds,
            1,
            3600,
            current.media_gifv_max_seconds,
        );
        update.media_max_av_mb =
            parse_clamped(&form.media_max_av_mb, 1, 99, current.media_max_av_mb);
        update.media_processing_concurrency = parse_clamped(
            &form.media_processing_concurrency,
            0,
            64,
            current.media_processing_concurrency,
        );
    }

    if section.covers(Section::Retention) {
        let (
            Some(media_cache_retention_days),
            Some(remote_video_max_mb),
            Some(remote_video_max_height),
        ) = (
            parse_non_negative(&form.media_cache_retention_days),
            parse_non_negative(&form.remote_video_max_mb),
            parse_non_negative(&form.remote_video_max_height),
        )
        else {
            return Ok(redirect_section("error", section));
        };
        // IP/sign-in-log retention must stay bounded: 0 would mean scrubbing
        // everything (not keeping forever), so a positive day count is
        // required.
        let Some(ip_retention_days) =
            parse_non_negative(&form.ip_retention_days).filter(|d| *d > 0)
        else {
            return Ok(redirect_section("error", section));
        };
        update.media_cache_retention_days = media_cache_retention_days;
        update.remote_video_max_mb = remote_video_max_mb;
        update.remote_video_max_height = remote_video_max_height;
        update.ip_retention_days = ip_retention_days;
        update.media_video_retention_days = parse_optional_days(&form.media_video_retention_days);
        update.media_profile_retention_days = parse_clamped(
            &form.media_profile_retention_days,
            0,
            i32::MAX,
            current.media_profile_retention_days,
        );
        update.media_card_retention_days = parse_clamped(
            &form.media_card_retention_days,
            0,
            i32::MAX,
            current.media_card_retention_days,
        );
        update.media_emoji_retention_days = parse_clamped(
            &form.media_emoji_retention_days,
            0,
            i32::MAX,
            current.media_emoji_retention_days,
        );
        update.media_cache_max_gb = parse_clamped(
            &form.media_cache_max_gb,
            0,
            i32::MAX,
            current.media_cache_max_gb,
        );
        update.media_video_cache_max_gb = parse_clamped(
            &form.media_video_cache_max_gb,
            0,
            i32::MAX,
            current.media_video_cache_max_gb,
        );
    }

    if section.covers(Section::Translation) {
        let (
            Some(translation_cache_retention_days),
            Some(translation_cache_max_rows),
            Some(translation_user_rate_limit_per_hour),
        ) = (
            parse_non_negative(&form.translation_cache_retention_days),
            parse_non_negative(&form.translation_cache_max_rows),
            parse_non_negative(&form.translation_user_rate_limit_per_hour),
        )
        else {
            return Ok(redirect_section("error", section));
        };
        // Zero concurrency would deadlock every translate request; require ≥ 1.
        let Some(translation_backend_concurrency) =
            parse_non_negative(&form.translation_backend_concurrency).filter(|c| *c > 0)
        else {
            return Ok(redirect_section("error", section));
        };
        update.translation_cache_retention_days = translation_cache_retention_days;
        update.translation_cache_max_rows = translation_cache_max_rows;
        update.translation_backend_concurrency = translation_backend_concurrency;
        update.translation_user_rate_limit_per_hour = translation_user_rate_limit_per_hour;
        update.translation_refresh_on_provider_change =
            form.translation_refresh_on_provider_change.is_some();
    }

    if section.covers(Section::Limits) {
        let Some(counts) = parse_rate_limits(&form) else {
            return Ok(redirect_section("error", section));
        };
        let [
            rate_limit_authenticated_api,
            rate_limit_per_token_api,
            rate_limit_unauthenticated_api,
            rate_limit_api_media,
            rate_limit_api_delete,
            rate_limit_api_sign_up,
            rate_limit_app_registrations,
            rate_limit_paging,
            rate_limit_login_attempts,
            rate_limit_password_resets,
            rate_limit_sign_up_web,
        ] = counts;
        update.rate_limiting_enabled = form.rate_limiting_enabled.is_some();
        update.rate_limit_authenticated_api = rate_limit_authenticated_api;
        update.rate_limit_per_token_api = rate_limit_per_token_api;
        update.rate_limit_unauthenticated_api = rate_limit_unauthenticated_api;
        update.rate_limit_api_media = rate_limit_api_media;
        update.rate_limit_api_delete = rate_limit_api_delete;
        update.rate_limit_api_sign_up = rate_limit_api_sign_up;
        update.rate_limit_app_registrations = rate_limit_app_registrations;
        update.rate_limit_paging = rate_limit_paging;
        update.rate_limit_login_attempts = rate_limit_login_attempts;
        update.rate_limit_password_resets = rate_limit_password_resets;
        update.rate_limit_sign_up_web = rate_limit_sign_up_web;
    }

    if section.covers(Section::Appearance) {
        update.custom_css = &form.custom_css;
    }

    instance_settings::save(&state.pool, update)
        .await
        .map_err(api_err)?;
    // The rate limiter reads settings through this cache; apply immediately.
    state.settings_cache.invalidate();
    Ok(redirect_section("applied", section))
}

/// `POST /web/admin/site-uploads/{var}` — upload/replace a site image slot.
/// Processing mirrors Mastodon's `SiteUpload` styles: the original is stored
/// metadata-stripped alongside the slot's fill-cropped PNG renditions.
pub async fn upload_site_image(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(var): Path<String>,
    mut multipart: Multipart,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !site_upload::VARS.contains(&var.as_str()) {
        return Err(crate::error::ApiError::NotFound.into_response());
    }

    let mut csrf = String::new();
    let mut image = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| bad_request(format!("invalid multipart body: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "csrf" => csrf = field.text().await.unwrap_or_default(),
            "image" => {
                image = field
                    .bytes()
                    .await
                    .map_err(|e| bad_request(format!("upload failed: {e}")))?
                    .to_vec();
            }
            _ => {}
        }
    }
    if !admin.user.csrf_ok(&csrf) {
        return Err(csrf_rejection());
    }
    let Ok(processed) = process_site_upload_blocking(var.clone(), image).await else {
        return Ok(redirect_uploads("upload_error"));
    };

    let file_name = format!(
        "{}.{}",
        plamenu_db::id::next(),
        processed.original.extension
    );
    state
        .media
        .put(&file_name, processed.original.bytes.clone())
        .await
        .map_err(|e| bad_request(format!("upload failed: {e}")))?;
    let mut variants = Vec::with_capacity(processed.styles.len());
    for style in &processed.styles {
        let variant_name = format!("{}.png", plamenu_db::id::next());
        state
            .media
            .put(&variant_name, style.bytes.clone())
            .await
            .map_err(|e| bad_request(format!("upload failed: {e}")))?;
        variants.push(NewVariant {
            style: style.style.clone(),
            file_name: variant_name,
            width: style.width.cast_signed(),
            height: style.height.cast_signed(),
        });
    }

    let replaced = site_upload::upsert(
        &state.pool,
        &var,
        &file_name,
        processed.original.content_type,
        i64::try_from(processed.original.bytes.len()).unwrap_or(i64::MAX),
        processed.original.width.cast_signed(),
        processed.original.height.cast_signed(),
        processed.blurhash.as_deref(),
        &variants,
    )
    .await
    .map_err(api_err)?;
    for old in replaced {
        let _ = state.media.delete(&old).await;
    }
    Ok(redirect_uploads("applied"))
}

#[derive(Debug, Deserialize)]
pub struct DescriptionForm {
    csrf: String,
    #[serde(default)]
    description: String,
}

/// `POST /web/admin/site-uploads/{var}/description` — the slot's alt text
/// (Mastodon's `thumbnail_description`).
pub async fn describe_site_image(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(var): Path<String>,
    Form(form): Form<DescriptionForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let description = form.description.trim();
    if description.chars().count() > DESCRIPTION_LIMIT {
        return Ok(redirect_uploads("error"));
    }
    let updated = site_upload::set_description(&state.pool, &var, description)
        .await
        .map_err(api_err)?;
    Ok(redirect_uploads(if updated { "applied" } else { "error" }))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/settings/test-email` — one synchronous SMTP round trip to
/// the acting admin's own address (O4): the relay's verdict lands in the
/// flash banner instead of a background retry loop, so a misconfigured
/// `[smtp]` section is caught before a user's confirmation mail silently
/// stalls.
pub async fn send_test_email(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(smtp) = state.config.smtp.as_ref() else {
        return Ok(redirect_open("no_smtp", "email"));
    };
    let Some(recipient) = admin.user.current.user.email.clone() else {
        return Ok(redirect_open("no_email", "email"));
    };
    match crate::mailer::send_test(smtp, &recipient).await {
        Ok(()) => Ok(redirect_settings_detail("test_email_sent", &recipient)),
        Err(error) => {
            tracing::warn!(%error, "admin test e-mail failed");
            Ok(redirect_settings_detail("test_email_failed", &error))
        }
    }
}

/// `POST /web/admin/settings/backfill-sizes` — the web face of `plamenu media
/// backfill-sizes` (O5). The sweep stats every stored file, so it runs in the
/// background; re-running is harmless (already-filled rows are skipped). A
/// process single-flight guard (finding #54) means repeated clicks can't launch
/// concurrent full-store scans — the second submit reports "already running".
pub async fn backfill_sizes(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(guard) = crate::maintenance::try_begin_backfill() else {
        return Ok(redirect_open("backfill_running", "maintenance"));
    };
    tokio::spawn(async move {
        // The guard frees the single-flight slot when this task ends (finish,
        // error, or panic).
        let _guard = guard;
        match crate::maintenance::backfill_stored_sizes(&state).await {
            Ok((filled, skipped)) => {
                tracing::info!(filled, skipped, "stored-size backfill finished");
            }
            Err(error) => tracing::warn!(%error, "stored-size backfill failed"),
        }
    });
    Ok(redirect_open("backfill_started", "maintenance"))
}

/// `POST /web/admin/site-uploads/{var}/delete` — empty the slot and remove
/// the stored files.
pub async fn delete_site_image(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(var): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let replaced = site_upload::delete(&state.pool, &var)
        .await
        .map_err(api_err)?;
    let removed = !replaced.is_empty();
    for old in replaced {
        let _ = state.media.delete(&old).await;
    }
    Ok(redirect_uploads(if removed { "applied" } else { "error" }))
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

/// Settings-specific flash codes, falling through to the shared banner.
/// `detail` carries the test-e-mail recipient or relay error.
fn flash_banner(flash: Option<&str>, detail: Option<&str>) -> Markup {
    let detail = detail.unwrap_or_default();
    match flash {
        Some("applied") => html! { p.admin-flash role="status" { "Settings saved." } },
        Some("no_such_account") => html! {
            p.admin-flash.is-error role="alert" {
                "The contact username does not match any local account."
            }
        },
        Some("upload_error") => html! {
            p.admin-flash.is-error role="alert" { "That image could not be processed." }
        },
        Some("test_email_sent") => html! {
            p.admin-flash role="status" {
                "Test e-mail accepted by the relay — check the inbox of " (detail) "."
            }
        },
        Some("test_email_failed") => html! {
            p.admin-flash.is-error role="alert" {
                "The test e-mail could not be sent: " (detail)
            }
        },
        Some("no_email") => html! {
            p.admin-flash.is-error role="alert" {
                "Your account has no e-mail address to send the test message to."
            }
        },
        Some("no_smtp") => html! {
            p.admin-flash.is-error role="alert" { "No SMTP relay is configured." }
        },
        Some("backfill_started") => html! {
            p.admin-flash role="status" {
                "Stored-size backfill started — the result lands in the server log."
            }
        },
        Some("backfill_running") => html! {
            p.admin-flash role="status" {
                "A stored-size backfill is already running — its result lands in the server log."
            }
        },
        _ => super::flash_banner(flash, "The settings could not be saved."),
    }
}

fn redirect_settings(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/settings?flash={flash}"))],
    )
        .into_response()
}

/// [`redirect_settings`] that also re-expands a named group, so the operator
/// lands back on the section they submitted.
fn redirect_open(flash: &str, key: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/settings?flash={flash}&open={key}"),
        )],
    )
        .into_response()
}

/// [`redirect_open`] for a settings section; [`Section::All`] (a submit that
/// named none) has no group to re-expand.
fn redirect_section(flash: &str, section: Section) -> Response {
    if section == Section::All {
        redirect_settings(flash)
    } else {
        redirect_open(flash, section.key())
    }
}

/// The site-image slots, which live in their own group.
fn redirect_uploads(flash: &str) -> Response {
    redirect_open(flash, "site-images")
}

/// Like [`redirect_settings`] with a free-text `detail` query value (the
/// test-e-mail recipient or relay error), URL-encoded and length-capped.
fn redirect_settings_detail(flash: &str, detail: &str) -> Response {
    let capped: String = detail.chars().take(300).collect();
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("flash", flash)
        .append_pair("detail", &capped)
        .append_pair("open", "email")
        .finish();
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/settings?{query}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
