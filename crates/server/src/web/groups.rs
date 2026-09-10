//! Local groups in the first-party web UI: the `/groups` section —
//! the groups you've joined, the local directory and the creation form. A
//! group's own page is its profile (`/@name`), which already wears the Group
//! badge; the forum-flavored body (titled posts, votes, sorts) arrives with
//! the later group slices.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::account::{self, Account};
use plamenu_db::group::{self, Affiliation, Group, MembershipPolicy, PostingPolicy};
use plamenu_db::{follow, report, status};
use serde::Deserialize;
use serde_json::Value;

use super::i18n::Locale;
use super::pages::resolve_handle;
use super::session::{MaybeWebUser, WebUser, csrf_rejection, preview_redirect};
use super::settings::{
    bad_form, checked, error_flash, field, form_pairs, redirect_to, saved_flash,
};
use super::{layout, view};
use crate::entities::render_accounts;
use crate::error::ApiError;
use crate::groups::{
    CreateFailure, CreateGroupParams, CreateInvalid, GroupProfileEdit, create_group, may_create,
};
use crate::media_processing::MAX_UPLOAD_BYTES;
use crate::state::AppState;

/// Directory page size, offset-paged via the shared "Show more" link.
const DIRECTORY_LIMIT: i64 = 100;

/// A group's display name is an account display name, so it takes the same
/// limit; the description is not a bio but a community sidebar — where the
/// rules go — so it gets a far longer one. Both are well under the 20 KB
/// Mastodon truncates an inbound `summary` at, so what a moderator writes is
/// what peers show.
const MAX_GROUP_NAME_CHARS: usize = 30;
const MAX_GROUP_NOTE_CHARS: usize = 2000;
/// Metadata fields, matching the profile editor's limits (Mastodon's).
const MAX_GROUP_FIELDS: usize = 4;
const MAX_GROUP_FIELD_CHARS: usize = 255;

#[derive(Deserialize)]
pub struct GroupsQuery {
    saved: Option<String>,
    error: Option<String>,
    offset: Option<i64>,
}

/// Why a group write was refused. The pages these writes redirect to are not
/// the ones that produced the refusal, so it travels as a stable `?error=`
/// code and is re-stated from the catalog where it lands — never as an
/// English sentence in the query string (`web::lists::Refusal` is the model).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    CreationLimited,
    NameBlank,
    NameTooLong,
    NameInvalid,
    NameReserved,
    NameTaken,
    DisplayNameTooLong,
    GroupQuota,
    NoteTooLong,
    TooManyFields,
    FieldsTooLong,
    BanSelf,
    BanOwner,
    RequestStale,
    NoSuchAccount,
    GroupModerator,
    PostNotInGroup,
}

impl Refusal {
    fn code(self) -> &'static str {
        match self {
            Refusal::CreationLimited => "creation_limited",
            Refusal::NameBlank => "name_blank",
            Refusal::NameTooLong => "name_too_long",
            Refusal::NameInvalid => "name_invalid",
            Refusal::NameReserved => "name_reserved",
            Refusal::NameTaken => "name_taken",
            Refusal::DisplayNameTooLong => "display_too_long",
            Refusal::GroupQuota => "group_quota",
            Refusal::NoteTooLong => "note_too_long",
            Refusal::TooManyFields => "too_many_fields",
            Refusal::FieldsTooLong => "fields_too_long",
            Refusal::BanSelf => "ban_self",
            Refusal::BanOwner => "ban_owner",
            Refusal::RequestStale => "request_stale",
            Refusal::NoSuchAccount => "no_account",
            Refusal::GroupModerator => "group_moderator",
            Refusal::PostNotInGroup => "post_not_in_group",
        }
    }

    /// An unrecognized code renders nothing, so no text a caller puts in the
    /// query string reaches the page.
    fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "creation_limited" => Refusal::CreationLimited,
            "name_blank" => Refusal::NameBlank,
            "name_too_long" => Refusal::NameTooLong,
            "name_invalid" => Refusal::NameInvalid,
            "name_reserved" => Refusal::NameReserved,
            "name_taken" => Refusal::NameTaken,
            "display_too_long" => Refusal::DisplayNameTooLong,
            "group_quota" => Refusal::GroupQuota,
            "note_too_long" => Refusal::NoteTooLong,
            "too_many_fields" => Refusal::TooManyFields,
            "fields_too_long" => Refusal::FieldsTooLong,
            "ban_self" => Refusal::BanSelf,
            "ban_owner" => Refusal::BanOwner,
            "request_stale" => Refusal::RequestStale,
            "no_account" => Refusal::NoSuchAccount,
            "group_moderator" => Refusal::GroupModerator,
            "post_not_in_group" => Refusal::PostNotInGroup,
            _ => return None,
        })
    }

    fn message(self, locale: Locale) -> String {
        fn with_limit<V>(locale: Locale, id: &str, limit: V) -> String
        where
            V: Into<fluent_bundle::FluentValue<'static>>,
        {
            let mut args = FluentArgs::new();
            args.set("limit", limit);
            locale.text_with(id, &args)
        }
        match self {
            Refusal::CreationLimited => locale.text("groups-error-limited"),
            Refusal::NameBlank => locale.text("groups-error-name-blank"),
            Refusal::NameTooLong => {
                with_limit(locale, "groups-error-name-too-long", MAX_GROUP_NAME_CHARS)
            }
            Refusal::NameInvalid => locale.text("groups-error-name-invalid"),
            Refusal::NameReserved => locale.text("groups-error-name-reserved"),
            Refusal::NameTaken => locale.text("groups-error-name-taken"),
            Refusal::DisplayNameTooLong => with_limit(
                locale,
                "groups-error-display-too-long",
                MAX_GROUP_NAME_CHARS,
            ),
            Refusal::GroupQuota => with_limit(
                locale,
                "groups-error-quota",
                crate::groups::MAX_GROUPS_PER_ACCOUNT,
            ),
            Refusal::NoteTooLong => {
                with_limit(locale, "groups-error-note-too-long", MAX_GROUP_NOTE_CHARS)
            }
            Refusal::TooManyFields => locale.text("groups-error-too-many-fields"),
            Refusal::FieldsTooLong => with_limit(
                locale,
                "groups-error-fields-too-long",
                MAX_GROUP_FIELD_CHARS,
            ),
            Refusal::BanSelf => locale.text("groups-error-ban-self"),
            Refusal::BanOwner => locale.text("groups-error-ban-owner"),
            Refusal::RequestStale => locale.text("groups-error-request-stale"),
            Refusal::NoSuchAccount => locale.text("groups-error-no-account"),
            Refusal::GroupModerator => locale.text("groups-error-group-moderator"),
            Refusal::PostNotInGroup => locale.text("groups-error-post-not-in-group"),
        }
    }
}

impl From<CreateInvalid> for Refusal {
    fn from(invalid: CreateInvalid) -> Self {
        match invalid {
            CreateInvalid::NameBlank => Self::NameBlank,
            CreateInvalid::NameTooLong => Self::NameTooLong,
            CreateInvalid::NameInvalid => Self::NameInvalid,
            CreateInvalid::NameReserved => Self::NameReserved,
            CreateInvalid::NameTaken => Self::NameTaken,
            CreateInvalid::DisplayNameTooLong => Self::DisplayNameTooLong,
            CreateInvalid::Quota => Self::GroupQuota,
        }
    }
}

/// The flash for a `?error=` code, or nothing when it is not one of ours.
fn error_message(code: Option<&str>, locale: Locale) -> Option<String> {
    code.and_then(Refusal::from_code)
        .map(|refusal| refusal.message(locale))
}

/// Redirects back to `base` with the refusal as a `?error=` code.
fn redirect_refused(base: &str, refusal: Refusal) -> Response {
    redirect_to(&format!("{base}?error={}", refusal.code()))
}

/// `GET /groups` — joined groups and the local directory, with a link to the
/// dedicated creation page. Anonymous visitors get the directory read-only
/// under the operator's `anon_groups` switch.
pub async fn index(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<GroupsQuery>,
) -> Response {
    if let Some(redirect) = preview_redirect(&session, state.anon_groups().await) {
        return redirect;
    }
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let offset = query.offset.unwrap_or(0).max(0);
    let joined = match viewer_id {
        // Later directory pages list on; the joined block belongs to the first.
        Some(viewer_id) if offset == 0 => {
            match group::joined_group_ids(&state.pool, viewer_id).await {
                Ok(ids) => ids,
                Err(err) => return ApiError::from(err).into_response(),
            }
        }
        _ => Vec::new(),
    };
    let local = match group::local_group_ids(&state.pool, false, DIRECTORY_LIMIT, offset).await {
        Ok(ids) => ids,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let fetched = local.len();
    let joined_cards = match group_cards(&state, &joined, viewer_id).await {
        Ok(cards) => cards,
        Err(response) => return response,
    };
    // The directory skips groups already shown under "Your groups".
    let discover: Vec<i64> = local
        .iter()
        .filter(|id| !joined.contains(id))
        .copied()
        .collect();
    let discover_cards = match group_cards(&state, &discover, viewer_id).await {
        Ok(cards) => cards,
        Err(response) => return response,
    };
    let settings = match state.settings_cache.get(&state.pool).await {
        Ok(settings) => settings,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let can_create = session
        .as_ref()
        .is_some_and(|user| may_create(settings.group_creation_policy(), user.is_staff));
    let body = html! {
        section.column {
            h1 { (view::icon("group")) " " (locale.text("nav-groups")) }
            (saved_flash(query.saved.is_some(), &locale.text("groups-created-flash")))
            (error_flash(error_message(query.error.as_deref(), locale).as_deref()))
            p.settings-field__hint {
                (locale.markup("groups-intro", &[
                    ("search", html! {
                        a href="/search" { (locale.text("groups-intro-search")) }
                    }),
                    ("example", html! { code { "!hiking@lemmy.example" } }),
                ]))
            }

            @if can_create {
                p {
                    a.pill-button href="/groups/new" {
                        (view::icon("group")) " " (locale.text("groups-create-button"))
                    }
                }
            }

            @if session.is_some() && offset == 0 {
                h2 { (locale.text("groups-yours-heading")) }
                @if joined_cards.is_empty() {
                    p.empty { (locale.text("groups-yours-empty")) }
                } @else {
                    ul.relationships-list {
                        @for value in &joined_cards {
                            (group_row(value))
                        }
                    }
                }
            }

            h2 { (locale.text("groups-local-heading")) }
            @if discover_cards.is_empty() {
                @if offset == 0 {
                    p.empty { (locale.text("groups-local-empty")) }
                } @else {
                    p.empty { (locale.text("groups-local-no-more")) }
                }
            } @else {
                ul.relationships-list {
                    @for value in &discover_cards {
                        (group_row(value))
                    }
                }
            }
            (super::explore::more_link("/groups", fetched, DIRECTORY_LIMIT, offset, locale))
        }
    };
    layout::shell_visitor_localized(
        &locale.text("nav-groups"),
        session.as_ref(),
        super::pages::anon_nav(&state).await,
        &body,
        locale,
    )
    .into_response()
}

/// `GET /groups/new` — the dedicated group-creation page.
pub async fn new_page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<GroupsQuery>,
) -> Response {
    let settings = match state.settings_cache.get(&state.pool).await {
        Ok(settings) => settings,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let can_create = may_create(settings.group_creation_policy(), user.is_staff);
    let locale = user.locale;
    let body = html! {
        section.column {
            h1 { (view::icon("group")) " " (locale.text("groups-new-title")) }
            (error_flash(error_message(query.error.as_deref(), locale).as_deref()))
            @if can_create {
                (create_form(&user))
            } @else {
                p.settings__error role="alert" { (locale.text("groups-error-limited")) }
                p { a href="/groups" { (locale.text("groups-back")) } }
            }
        }
    };
    layout::shell(&locale.text("groups-new-title"), Some(&user), &body).into_response()
}

/// The new-group form: name, display name, membership policy.
fn create_form(user: &WebUser) -> Markup {
    let locale = user.locale;
    html! {
        form.settings-form method="post" action="/web/groups" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("groups-new-title")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-name-label")) }
                    input type="text" name="name"
                        placeholder=(locale.text("groups-name-placeholder"))
                        maxlength="30" autocomplete="off" required;
                    span.settings-field__hint { (locale.text("groups-name-hint")) }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-display-name-label")) }
                    input type="text" name="display_name"
                        placeholder=(locale.text("groups-display-name-placeholder"))
                        maxlength="30" autocomplete="off";
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-membership-label")) }
                    select name="membership_policy" {
                        option value="open" selected { (locale.text("groups-membership-open")) }
                        option value="approval" { (locale.text("groups-membership-approval")) }
                    }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-posting-label")) }
                    select name="posting_policy" {
                        option value="anyone" { (locale.text("groups-posting-anyone")) }
                        option value="members" selected { (locale.text("groups-posting-members")) }
                        option value="mods" { (locale.text("groups-posting-mods")) }
                    }
                    span.settings-field__hint { (locale.text("groups-posting-hint")) }
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("groups-create-button")) }
                a.settings-button--plain href="/groups" { (locale.text("filters-cancel")) }
            }
        }
    }
}

/// The group page's "New post" entry point: a button to the shared
/// full composer, scoped to this group (`/compose?group={id}`), where the
/// forum-style Title and Link fields join the regular composer (media, content
/// warning, poll, language, preview). Rendered only for members the posting
/// policy allows.
pub fn post_form(group_id: i64, locale: Locale) -> Markup {
    html! {
        a.button.group-post-link href=(format!("/compose?group={group_id}")) {
            (view::icon("compose")) " " (locale.text("groups-new-post"))
        }
    }
}

/// One group row: the account card linking to the group's page.
fn group_row(value: &serde_json::Value) -> Markup {
    let account = view::Account(value);
    html! {
        li.relationships-list__item {
            (view::account_card(&account))
        }
    }
}

/// `POST /web/groups` — create a group and land on its page.
pub async fn create_action(
    State(state): State<AppState>,
    user: WebUser,
    crate::instance_policy::RemoteIp(ip): crate::instance_policy::RemoteIp,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let settings = match state.settings_cache.get(&state.pool).await {
        Ok(settings) => settings,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if !may_create(settings.group_creation_policy(), user.is_staff) {
        return redirect_refused("/groups", Refusal::CreationLimited);
    }
    // Per-account/IP admission before the RSA key is generated.
    if let Err(error) =
        crate::rate_limit::check_group_create(&state, user.current.account.id, ip).await
    {
        return error.into_response();
    }
    let name = field(&pairs, "name").unwrap_or_default().trim().to_owned();
    let display_name = field(&pairs, "display_name")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let membership_policy =
        MembershipPolicy::parse(field(&pairs, "membership_policy").unwrap_or("open"));
    let posting_policy = PostingPolicy::parse(field(&pairs, "posting_policy").unwrap_or("members"));
    match create_group(
        &state,
        CreateGroupParams {
            name: &name,
            display_name: &display_name,
            membership_policy,
            posting_policy,
            created_by: user.current.account.id,
            enforce_username_blocklist: true,
            enforce_account_quota: true,
        },
    )
    .await
    {
        Ok((created, _)) => redirect_to(&format!("/@{}", created.username)),
        // Validation problems come back as a typed refusal restated from the
        // catalog on the form; anything else is a real error page.
        Err(CreateFailure::Invalid(invalid)) => redirect_refused("/groups/new", invalid.into()),
        Err(CreateFailure::Api(err)) => err.into_response(),
    }
}

/// Fetches group accounts as rendered entities, preserving `ids` order.
async fn group_cards(
    state: &AppState,
    ids: &[i64],
    viewer_id: Option<i64>,
) -> Result<Vec<serde_json::Value>, Response> {
    let mut accounts = match account::find_by_ids(&state.pool, ids).await {
        Ok(accounts) => accounts,
        Err(err) => return Err(ApiError::from(err).into_response()),
    };
    accounts.sort_by_key(|a| ids.iter().position(|id| *id == a.id).unwrap_or(usize::MAX));
    render_accounts(&state.pool, &state.config.domain, &accounts, viewer_id)
        .await
        .map_err(IntoResponse::into_response)
}

// ===========================================================================
// Group settings & moderation pages (owner/moderator gated)
// ===========================================================================

const MEMBERS_LIMIT: i64 = 40;

#[derive(Deserialize)]
pub struct ManageQuery {
    saved: Option<String>,
    error: Option<String>,
    max_id: Option<i64>,
}

/// An account's handle for display: `name` locally, `name@domain` for remotes.
fn handle_of(domain: &str, account: &Account) -> String {
    crate::entities::account_acct(domain, account)
}

/// Loads a group and asserts the signed-in user moderates it, returning the
/// group account, sidecar row and the user's rank. A non-group, missing group
/// or non-moderator all render the styled 404 (management is invisible to
/// outsiders).
async fn moderated_group(
    state: &AppState,
    user: &WebUser,
    group_id: i64,
) -> Result<(Account, Group, Affiliation), Response> {
    let not_found = || super::pages::not_found(state, Some(user), user.locale);
    let group_account = match account::find_by_id(&state.pool, group_id).await {
        Ok(Some(account)) if account.is_group() => account,
        Ok(_) => return Err(not_found().await),
        Err(err) => return Err(ApiError::from(err).into_response()),
    };
    let group = match group::find(&state.pool, group_id).await {
        Ok(Some(group)) => group,
        Ok(None) => return Err(not_found().await),
        Err(err) => return Err(ApiError::from(err).into_response()),
    };
    match group::affiliation_of(&state.pool, group_id, user.current.account.id).await {
        Ok(Some(rank @ (Affiliation::Owner | Affiliation::Moderator))) => {
            Ok((group_account, group, rank))
        }
        Ok(_) => Err(not_found().await),
        Err(err) => Err(ApiError::from(err).into_response()),
    }
}

/// The management section selector — the same `tab_strip` disclosure the admin
/// console, settings and profile use, so group moderation reads as part of the
/// client rather than a bolt-on. The Moderators section is owner-only; Join
/// requests is hidden for open groups; Reports folds its open-count into the
/// label (the selector has no badge slot).
fn manage_tabs(
    group: &Account,
    active: &str,
    is_owner: bool,
    approval: bool,
    reports_open: u64,
    locale: Locale,
) -> Markup {
    let base = format!("/groups/{}", group.id);
    // The open-count folds into the label through an explicit `[0]` variant,
    // like the relationships tabs' pending badges.
    let mut args = FluentArgs::new();
    args.set("count", reports_open);
    let reports_label = locale.text_with("groups-tab-reports", &args);
    // Owned (slug, href, label) so the borrowed `Tab` slice outlives the call;
    // `settings` lives at `/manage`, every other section at `/{slug}`.
    let mut entries: Vec<(&str, String, String)> = vec![
        (
            "settings",
            format!("{base}/manage"),
            locale.text("groups-tab-settings"),
        ),
        (
            "members",
            format!("{base}/members"),
            locale.text("groups-tab-members"),
        ),
    ];
    if approval {
        entries.push((
            "requests",
            format!("{base}/requests"),
            locale.text("groups-tab-requests"),
        ));
    }
    if is_owner {
        entries.push((
            "moderators",
            format!("{base}/moderators"),
            locale.text("groups-tab-moderators"),
        ));
    }
    entries.push(("reports", format!("{base}/reports"), reports_label));

    let tabs: Vec<view::Tab> = entries
        .iter()
        .map(|(slug, href, label)| view::Tab::new(href, label, *slug == active))
        .collect();
    view::tab_strip(&locale.text("groups-manage-aria"), &tabs)
}

/// The management page shell: title, tabs, flashes, then `body`.
#[allow(clippy::too_many_arguments)] // the shared shell threads every page's slots
async fn manage_shell(
    state: &AppState,
    user: &WebUser,
    group_account: &Account,
    group: &Group,
    active: &str,
    rank: Affiliation,
    query: &ManageQuery,
    saved_id: &str,
    body: Markup,
) -> Response {
    let locale = user.locale;
    let reports_open = report::count_unresolved_for_group(&state.pool, group_account.id)
        .await
        .unwrap_or(0);
    let is_owner = rank == Affiliation::Owner;
    let approval = group.membership_policy() == MembershipPolicy::Approval;
    let mut title_args = FluentArgs::new();
    title_args.set("name", group_account.username.as_str());
    let title = locale.text_with("groups-manage-title", &title_args);
    let content = html! {
        section.column {
            h1 { (view::icon("group")) " " (group_account.username) }
            (manage_tabs(group_account, active, is_owner, approval, reports_open, locale))
            (saved_flash(query.saved.is_some(), &locale.text(saved_id)))
            (error_flash(error_message(query.error.as_deref(), locale).as_deref()))
            (body)
        }
    };
    layout::shell(&title, Some(user), &content).into_response()
}

// ---- Settings -------------------------------------------------------------

/// `GET /groups/{id}/manage` — the group's settings.
pub async fn manage(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    Query(query): Query<ManageQuery>,
) -> Response {
    let (group_account, group, rank) = match moderated_group(&state, &user, group_id).await {
        Ok(triple) => triple,
        Err(response) => return response,
    };
    let approval = group.membership_policy() == MembershipPolicy::Approval;
    let fields = group_field_rows(&group_account);
    let locale = user.locale;
    let mut fields_hint_args = FluentArgs::new();
    fields_hint_args.set("max", MAX_GROUP_FIELDS);
    let membership_labels = [
        locale.text("groups-membership-open"),
        locale.text("groups-membership-approval"),
    ];
    let posting_labels = [
        locale.text("groups-posting-anyone"),
        locale.text("groups-posting-members"),
        locale.text("groups-posting-mods"),
    ];
    let body = html! {
        form.settings-form method="post" enctype="multipart/form-data"
            action=(format!("/web/groups/{group_id}/settings")) {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("groups-profile-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-display-name-label")) }
                    input type="text" name="display_name" maxlength=(MAX_GROUP_NAME_CHARS)
                        value=(group_account.display_name) autocomplete="off";
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-note-label")) }
                    textarea name="note" rows="8" maxlength=(MAX_GROUP_NOTE_CHARS) {
                        (group_account.note_source)
                    }
                    span.settings-field__hint { (locale.text("groups-note-hint")) }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-avatar-label")) }
                    input type="file" name="avatar" accept="image/*";
                    span.settings-field__hint { (locale.text("groups-avatar-hint")) }
                }
                label.settings-field {
                    span.settings-field__label {
                        (locale.text("groups-avatar-description-label"))
                    }
                    input type="text" name="avatar_description" maxlength="1500"
                        value=(group_account.avatar_description);
                    span.settings-field__hint {
                        (locale.text("settings-profile-image-description-hint"))
                    }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-banner-label")) }
                    input type="file" name="header" accept="image/*";
                }
                label.settings-field {
                    span.settings-field__label {
                        (locale.text("groups-banner-description-label"))
                    }
                    input type="text" name="header_description" maxlength="1500"
                        value=(group_account.header_description);
                    span.settings-field__hint {
                        (locale.text("settings-profile-image-description-hint"))
                    }
                }
            }
            fieldset.settings-form__group {
                legend { (locale.text("groups-fields-legend")) }
                p.settings-field__hint {
                    (locale.text_with("groups-fields-hint", &fields_hint_args))
                }
                @for (index, (name, value, verified)) in fields.iter().enumerate() {
                    div.settings-field__pair {
                        label.settings-field {
                            span.settings-field__label { (locale.text("groups-field-label")) }
                            input type="text" name=(format!("fields_attributes[{index}][name]"))
                                maxlength=(MAX_GROUP_FIELD_CHARS) value=(name);
                        }
                        label.settings-field {
                            span.settings-field__label {
                                (locale.text("groups-field-content"))
                                @if *verified {
                                    " " span.settings-field__verified {
                                        (view::icon("check")) " "
                                        (locale.text("groups-field-verified"))
                                    }
                                }
                            }
                            input type="text" name=(format!("fields_attributes[{index}][value]"))
                                maxlength=(MAX_GROUP_FIELD_CHARS) value=(value);
                        }
                    }
                }
            }
            fieldset.settings-form__group {
                legend { (locale.text("groups-policies-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-membership-label")) }
                    (super::settings::select(
                        "membership_policy",
                        &[
                            ("open", membership_labels[0].as_str()),
                            ("approval", membership_labels[1].as_str()),
                        ],
                        if approval { "approval" } else { "open" },
                    ))
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-posting-label")) }
                    (super::settings::select(
                        "posting_policy",
                        &[
                            ("anyone", posting_labels[0].as_str()),
                            ("members", posting_labels[1].as_str()),
                            ("mods", posting_labels[2].as_str()),
                        ],
                        group.posting_policy().as_str(),
                    ))
                    span.settings-field__hint { (locale.text("groups-posting-hint")) }
                }
                (super::settings::checkbox(
                    "sensitive",
                    &locale.text("groups-sensitive-label"),
                    &locale.text("groups-sensitive-hint"),
                    group.sensitive,
                ))
                (super::settings::checkbox(
                    "discoverable",
                    &locale.text("groups-discoverable-label"),
                    &locale.text("groups-discoverable-hint"),
                    group_account.discoverable.unwrap_or(true),
                ))
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("groups-save-settings")) }
            }
        }
    };
    manage_shell(
        &state,
        &user,
        &group_account,
        &group,
        "settings",
        rank,
        &query,
        "groups-saved-settings",
        body,
    )
    .await
}

/// The group console's settings form, parsed out of its multipart body: the
/// urlencoded pairs the settings helpers already read, plus the uploads.
#[derive(Default)]
struct SettingsForm {
    pairs: Vec<(String, String)>,
    avatar: Option<Vec<u8>>,
    header: Option<Vec<u8>>,
    /// Field rows keyed by their form index, so they keep the editor's order.
    fields: BTreeMap<String, (String, String)>,
}

/// Reads the multipart settings form. Uploads are held in memory like the
/// profile editor's, under the same per-file cap.
async fn read_settings_form(multipart: &mut Multipart) -> Result<SettingsForm, Response> {
    let mut form = SettingsForm::default();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                return Err((StatusCode::BAD_REQUEST, format!("invalid form: {e}")).into_response());
            }
        };
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "avatar" | "header" => {
                let bytes = match field.bytes().await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        return Err((StatusCode::BAD_REQUEST, format!("upload failed: {e}"))
                            .into_response());
                    }
                };
                if bytes.len() > MAX_UPLOAD_BYTES {
                    return Err((StatusCode::PAYLOAD_TOO_LARGE, "file too large").into_response());
                }
                // An empty file part is "no new image", not "clear it".
                if !bytes.is_empty() {
                    if name == "avatar" {
                        form.avatar = Some(bytes.to_vec());
                    } else {
                        form.header = Some(bytes.to_vec());
                    }
                }
            }
            _ => {
                let text = field.text().await.unwrap_or_default();
                if let Some((index, part)) = parse_field_key(&name) {
                    let entry = form.fields.entry(index).or_default();
                    if part == "name" {
                        entry.0 = text;
                    } else {
                        entry.1 = text;
                    }
                } else {
                    form.pairs.push((name, text));
                }
            }
        }
    }
    Ok(form)
}

/// Splits `fields_attributes[0][name]` into its index and part.
fn parse_field_key(name: &str) -> Option<(String, &'static str)> {
    let rest = name.strip_prefix("fields_attributes[")?;
    let (index, rest) = rest.split_once(']')?;
    let part = match rest {
        "[name]" => "name",
        "[value]" => "value",
        _ => return None,
    };
    Some((index.to_owned(), part))
}

/// The stored metadata fields as editor rows, padded to the full count so the
/// form always offers an empty slot — the profile editor's `field_rows`, for a
/// group's `account_fields`.
fn group_field_rows(account: &Account) -> Vec<(String, String, bool)> {
    let mut rows: Vec<(String, String, bool)> = account
        .fields
        .as_array()
        .map(|stored| {
            stored
                .iter()
                .map(|field| {
                    let get = |key: &str| {
                        field
                            .get(key)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    };
                    let verified = field.get("verified_at").is_some_and(|v| !v.is_null());
                    (get("name"), get("value"), verified)
                })
                .collect()
        })
        .unwrap_or_default();
    rows.truncate(MAX_GROUP_FIELDS);
    rows.resize(MAX_GROUP_FIELDS, (String::new(), String::new(), false));
    rows
}

/// `POST /web/groups/{id}/settings`.
pub async fn settings_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    mut multipart: Multipart,
) -> Response {
    let form = match read_settings_form(&mut multipart).await {
        Ok(form) => form,
        Err(response) => return response,
    };
    let pairs = form.pairs;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let (group_account, _, _) = match moderated_group(&state, &user, group_id).await {
        Ok(triple) => triple,
        Err(response) => return response,
    };
    let back = format!("/groups/{group_id}/manage");
    let display_name = field(&pairs, "display_name")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let note = field(&pairs, "note").unwrap_or_default().trim().to_owned();
    if display_name.chars().count() > MAX_GROUP_NAME_CHARS {
        return redirect_refused(&back, Refusal::DisplayNameTooLong);
    }
    if note.chars().count() > MAX_GROUP_NOTE_CHARS {
        return redirect_refused(&back, Refusal::NoteTooLong);
    }
    let fields: Vec<(String, String)> = form.fields.into_values().collect();
    if fields.len() > MAX_GROUP_FIELDS {
        return redirect_refused(&back, Refusal::TooManyFields);
    }
    if fields.iter().any(|(name, value)| {
        name.chars().count() > MAX_GROUP_FIELD_CHARS
            || value.chars().count() > MAX_GROUP_FIELD_CHARS
    }) {
        return redirect_refused(&back, Refusal::FieldsTooLong);
    }
    let policy = MembershipPolicy::parse(field(&pairs, "membership_policy").unwrap_or("open"));
    let posting_policy = PostingPolicy::parse(field(&pairs, "posting_policy").unwrap_or("members"));
    let rendered =
        match crate::compose::compose(&state, &note, crate::compose::PostFormat::Plain).await {
            Ok(composed) => composed.html,
            Err(err) => return err.into_response(),
        };
    // The shared write + double-send fan-out path (see admin console).
    let settings = crate::groups::GroupSettings {
        display_name: &display_name,
        note_html: &rendered,
        note_source: &note,
        policy,
        sensitive: checked(&pairs, "sensitive"),
        posting_policy,
        discoverable: checked(&pairs, "discoverable"),
        profile: GroupProfileEdit {
            avatar: form.avatar,
            header: form.header,
            avatar_description: field(&pairs, "avatar_description").map(str::to_owned),
            header_description: field(&pairs, "header_description").map(str::to_owned),
            fields: Some(fields),
        },
    };
    if let Err(err) = crate::groups::update_settings(&state, &group_account, settings).await {
        return err.into_response();
    }
    redirect_to(&format!("{back}?saved=1"))
}

// ---- Members & bans -------------------------------------------------------

/// `GET /groups/{id}/members` — the member roster with per-member ban controls,
/// plus the current bans with unban.
pub async fn members_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    Query(query): Query<ManageQuery>,
) -> Response {
    let (group_account, group, rank) = match moderated_group(&state, &user, group_id).await {
        Ok(triple) => triple,
        Err(response) => return response,
    };
    let viewer = user.current.account.id;
    let member_ids = match group::members(&state.pool, group_id, query.max_id, MEMBERS_LIMIT).await
    {
        Ok(ids) => ids,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let ban_rows = match group::outcasts(&state.pool, group_id).await {
        Ok(rows) => rows,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let members = match group_cards(&state, &member_ids, Some(viewer)).await {
        Ok(cards) => cards,
        Err(response) => return response,
    };
    let banned_ids: Vec<i64> = ban_rows.iter().map(|b| b.account_id).collect();
    let banned = match group_cards(&state, &banned_ids, Some(viewer)).await {
        Ok(cards) => cards,
        Err(response) => return response,
    };
    let locale = user.locale;
    let body = html! {
        h2 { (locale.text("groups-members-heading")) }
        @if members.is_empty() {
            p.empty { (locale.text("groups-members-empty")) }
        } @else {
            ul.relationships-list {
                @for value in &members {
                    (member_manage_row(group_id, value, &user.csrf, locale))
                }
            }
        }
        h2 { (locale.text("groups-banned-heading")) }
        @if banned.is_empty() {
            p.empty { (locale.text("groups-banned-empty")) }
        } @else {
            ul.relationships-list {
                @for value in &banned {
                    (banned_manage_row(group_id, value, &user.csrf, locale))
                }
            }
        }
    };
    manage_shell(
        &state,
        &user,
        &group_account,
        &group,
        "members",
        rank,
        &query,
        "groups-saved-members",
        body,
    )
    .await
}

/// One member row with a ban form.
fn member_manage_row(
    group_id: i64,
    value: &serde_json::Value,
    csrf: &str,
    locale: Locale,
) -> Markup {
    let account = view::Account(value);
    let account_id = value.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    html! {
        li.relationships-list__item {
            (view::account_card(&account))
            form.inline-form method="post" action=(format!("/web/groups/{group_id}/ban")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="account_id" value=(account_id);
                button.settings-button--danger type="submit" { (locale.text("groups-ban")) }
            }
        }
    }
}

/// One banned-account row with an unban form.
fn banned_manage_row(
    group_id: i64,
    value: &serde_json::Value,
    csrf: &str,
    locale: Locale,
) -> Markup {
    let account = view::Account(value);
    let account_id = value.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    html! {
        li.relationships-list__item {
            (view::account_card(&account))
            form.inline-form method="post" action=(format!("/web/groups/{group_id}/unban")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="account_id" value=(account_id);
                button type="submit" { (locale.text("groups-unban")) }
            }
        }
    }
}

/// `POST /web/groups/{id}/ban`.
pub async fn ban_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    body: Bytes,
) -> Response {
    let (group_account, target, back) =
        match target_ctx(&state, &user, group_id, &body, "members").await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    if target.id == user.current.account.id {
        return redirect_refused(&back, Refusal::BanSelf);
    }
    if matches!(
        group::affiliation_of(&state.pool, group_account.id, target.id).await,
        Ok(Some(Affiliation::Owner))
    ) {
        return redirect_refused(&back, Refusal::BanOwner);
    }
    match crate::groups::ban_member(
        &state,
        &group_account,
        &user.current.account,
        &target,
        None,
        None,
    )
    .await
    {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/groups/{id}/unban`.
pub async fn unban_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    body: Bytes,
) -> Response {
    let (group_account, target, back) =
        match target_ctx(&state, &user, group_id, &body, "members").await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    match crate::groups::unban_member(&state, &group_account, &user.current.account, &target).await
    {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

// ---- Join requests --------------------------------------------------------

/// `GET /groups/{id}/requests` — pending join requests (approval groups).
pub async fn requests_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    Query(query): Query<ManageQuery>,
) -> Response {
    let (group_account, group, rank) = match moderated_group(&state, &user, group_id).await {
        Ok(triple) => triple,
        Err(response) => return response,
    };
    let viewer = user.current.account.id;
    let requests = match follow::requests_of(&state.pool, group_id, None, None, MEMBERS_LIMIT).await
    {
        Ok(entries) => entries,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let ids: Vec<i64> = requests.iter().map(|r| r.account_id).collect();
    let cards = match group_cards(&state, &ids, Some(viewer)).await {
        Ok(cards) => cards,
        Err(response) => return response,
    };
    let locale = user.locale;
    let body = html! {
        @if cards.is_empty() {
            p.empty { (locale.text("groups-requests-empty")) }
        } @else {
            ul.relationships-list {
                @for value in &cards {
                    (request_row(group_id, value, &user.csrf, locale))
                }
            }
        }
    };
    manage_shell(
        &state,
        &user,
        &group_account,
        &group,
        "requests",
        rank,
        &query,
        "groups-saved-request",
        body,
    )
    .await
}

/// One join-request row with approve and reject forms.
fn request_row(group_id: i64, value: &serde_json::Value, csrf: &str, locale: Locale) -> Markup {
    let account = view::Account(value);
    let account_id = value.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    html! {
        li.relationships-list__item {
            (view::account_card(&account))
            form.inline-form method="post"
                action=(format!("/web/groups/{group_id}/requests/approve")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="account_id" value=(account_id);
                button type="submit" { (locale.text("groups-approve")) }
            }
            form.inline-form method="post"
                action=(format!("/web/groups/{group_id}/requests/reject")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="account_id" value=(account_id);
                button.settings-button--danger type="submit" { (locale.text("groups-reject")) }
            }
        }
    }
}

/// `POST /web/groups/{id}/requests/approve`.
pub async fn request_approve_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    body: Bytes,
) -> Response {
    let (group_account, target, back) =
        match target_ctx(&state, &user, group_id, &body, "requests").await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    match crate::actions::authorize_follow_request(&state, &group_account, &target).await {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        // A stale request (already handled) redirects back cleanly.
        Err(ApiError::NotFound) => redirect_refused(&back, Refusal::RequestStale),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/groups/{id}/requests/reject`.
pub async fn request_reject_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    body: Bytes,
) -> Response {
    let (group_account, target, back) =
        match target_ctx(&state, &user, group_id, &body, "requests").await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    match crate::actions::reject_follow_request(&state, &group_account, &target).await {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(ApiError::NotFound) => redirect_refused(&back, Refusal::RequestStale),
        Err(err) => err.into_response(),
    }
}

// ---- Moderators (owner only) ----------------------------------------------

/// `GET /groups/{id}/moderators` — the mod roster with add/remove (owner only).
pub async fn moderators_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    Query(query): Query<ManageQuery>,
) -> Response {
    let (group_account, group, rank) = match moderated_group(&state, &user, group_id).await {
        Ok(triple) => triple,
        Err(response) => return response,
    };
    if rank != Affiliation::Owner {
        return super::pages::not_found(&state, Some(&user), user.locale).await;
    }
    let viewer = user.current.account.id;
    let elevated = match group::elevated(&state.pool, group_id).await {
        Ok(entries) => entries,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let ids: Vec<i64> = elevated.iter().map(|e| e.account_id).collect();
    let cards = match group_cards(&state, &ids, Some(viewer)).await {
        Ok(cards) => cards,
        Err(response) => return response,
    };
    let is_moderator: std::collections::HashMap<i64, bool> = elevated
        .iter()
        .map(|e| (e.account_id, e.affiliation == "moderator"))
        .collect();
    let locale = user.locale;
    let body = html! {
        form.settings-form method="post" action=(format!("/web/groups/{group_id}/moderators/add")) {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("groups-add-moderator-legend")) }
                p.settings-field__hint {
                    (locale.markup("groups-add-moderator-hint", &[
                        ("local", html! { code { "@alice" } }),
                        ("remote", html! { code { "bob@example.social" } }),
                    ]))
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("groups-handle-label")) }
                    input type="text" name="handle" placeholder="@user@domain"
                        autocomplete="off" required;
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("groups-add-moderator-submit")) }
            }
        }
        ul.relationships-list {
            @for value in &cards {
                @let id = value.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                @let numeric = id.parse::<i64>().unwrap_or_default();
                (moderator_row(
                    group_id,
                    value,
                    *is_moderator.get(&numeric).unwrap_or(&false),
                    &user.csrf,
                    locale,
                ))
            }
        }
        form.settings-form method="post"
            action=(format!("/web/groups/{group_id}/moderators/transfer")) {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("groups-transfer-legend")) }
                p.settings-field__hint { (locale.text("groups-transfer-hint")) }
                label.settings-field {
                    span.settings-field__label {
                        (locale.text("groups-transfer-handle-label"))
                    }
                    input type="text" name="handle" placeholder="@user"
                        autocomplete="off" required;
                }
            }
            div.settings-form__actions {
                button.settings-button--danger type="submit" {
                    (locale.text("groups-transfer-legend"))
                }
            }
        }
    };
    manage_shell(
        &state,
        &user,
        &group_account,
        &group,
        "moderators",
        rank,
        &query,
        "groups-saved-moderators",
        body,
    )
    .await
}

/// One elevated-role row; only moderators get a remove control (never the owner).
fn moderator_row(
    group_id: i64,
    value: &serde_json::Value,
    is_moderator: bool,
    csrf: &str,
    locale: Locale,
) -> Markup {
    let account = view::Account(value);
    let account_id = value.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    html! {
        li.relationships-list__item {
            (view::account_card(&account))
            @if is_moderator {
                form.inline-form method="post"
                    action=(format!("/web/groups/{group_id}/moderators/remove")) {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="account_id" value=(account_id);
                    button.settings-button--danger type="submit" {
                        (locale.text("groups-remove-moderator"))
                    }
                }
            } @else {
                span.pill { (locale.text("groups-owner-badge")) }
            }
        }
    }
}

/// `POST /web/groups/{id}/moderators/add` (owner only).
pub async fn moderator_add_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let (group_account, _, rank) = match moderated_group(&state, &user, group_id).await {
        Ok(triple) => triple,
        Err(response) => return response,
    };
    let back = format!("/groups/{group_id}/moderators");
    if rank != Affiliation::Owner {
        return super::pages::not_found(&state, Some(&user), user.locale).await;
    }
    let handle = field(&pairs, "handle")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let target = match resolve_handle(&state, &handle).await {
        Ok(Some(account)) => account,
        Ok(None) => return redirect_refused(&back, Refusal::NoSuchAccount),
        Err(err) => return err.into_response(),
    };
    if target.is_group() {
        return redirect_refused(&back, Refusal::GroupModerator);
    }
    match crate::groups::set_moderator(&state, &group_account, &user.current.account, &target, true)
        .await
    {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/groups/{id}/moderators/remove` (owner only).
pub async fn moderator_remove_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    body: Bytes,
) -> Response {
    let (group_account, target, back) =
        match target_ctx(&state, &user, group_id, &body, "moderators").await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    // Only the owner may change moderators.
    if !matches!(
        group::affiliation_of(&state.pool, group_account.id, user.current.account.id).await,
        Ok(Some(Affiliation::Owner))
    ) {
        return super::pages::not_found(&state, Some(&user), user.locale).await;
    }
    match crate::groups::set_moderator(
        &state,
        &group_account,
        &user.current.account,
        &target,
        false,
    )
    .await
    {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/groups/{id}/moderators/transfer` (owner only) — hand ownership
/// to another local member; the current owner is demoted to moderator.
pub async fn moderator_transfer_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let (group_account, _, rank) = match moderated_group(&state, &user, group_id).await {
        Ok(triple) => triple,
        Err(response) => return response,
    };
    let back = format!("/groups/{group_id}/moderators");
    if rank != Affiliation::Owner {
        return super::pages::not_found(&state, Some(&user), user.locale).await;
    }
    let handle = field(&pairs, "handle")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let target = match resolve_handle(&state, &handle).await {
        Ok(Some(account)) => account,
        Ok(None) => return redirect_refused(&back, Refusal::NoSuchAccount),
        Err(err) => return err.into_response(),
    };
    match crate::groups::transfer_owner(&state, &group_account, &target).await {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

// ---- Reports --------------------------------------------------------------

/// `GET /groups/{id}/reports` — the community report queue.
pub async fn reports_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(group_id): Path<i64>,
    Query(query): Query<ManageQuery>,
) -> Response {
    let (group_account, group, rank) = match moderated_group(&state, &user, group_id).await {
        Ok(triple) => triple,
        Err(response) => return response,
    };
    let filter = report::AdminReportFilter {
        unresolved: true,
        group_account_id: Some(group_id),
        limit: MEMBERS_LIMIT,
        ..report::AdminReportFilter::default()
    };
    let reports = match report::list_for_admin(&state.pool, &filter).await {
        Ok(reports) => reports,
        Err(err) => return ApiError::from(err).into_response(),
    };
    // Every reported account in one query — the same target across several
    // reports used to be fetched once per report.
    let mut target_ids: Vec<i64> = reports.iter().map(|r| r.target_account_id).collect();
    target_ids.sort_unstable();
    target_ids.dedup();
    let targets: std::collections::HashMap<i64, Account> =
        match account::find_by_ids(&state.pool, &target_ids).await {
            Ok(rows) => rows.into_iter().map(|a| (a.id, a)).collect(),
            Err(err) => return ApiError::from(err).into_response(),
        };
    let locale = user.locale;
    let rows: Vec<Markup> = reports
        .iter()
        .map(|report| {
            report_row(
                group_id,
                report,
                targets.get(&report.target_account_id),
                &user.csrf,
                locale,
                &state.config.domain,
            )
        })
        .collect();
    let body = html! {
        @if rows.is_empty() {
            p.empty { (locale.text("groups-reports-empty")) }
        } @else {
            ul.report-list {
                @for row in &rows { (row) }
            }
        }
    };
    manage_shell(
        &state,
        &user,
        &group_account,
        &group,
        "reports",
        rank,
        &query,
        "groups-saved-report",
        body,
    )
    .await
}

/// One report card: target, comment, and resolve control (the target row
/// comes prefetched from the page's batched lookup).
fn report_row(
    group_id: i64,
    report: &report::Report,
    target: Option<&Account>,
    csrf: &str,
    locale: Locale,
    domain: &str,
) -> Markup {
    let post_count = report.status_ids.len();
    let mut count_args = FluentArgs::new();
    count_args.set("count", post_count);
    html! {
        li.report-list__item {
            div {
                @match target {
                    Some(account) => strong { "@" (handle_of(domain, account)) }
                    None => strong { (locale.text("groups-report-unknown")) }
                }
                @if post_count > 0 {
                    " · " (locale.text_with("groups-report-posts", &count_args))
                }
            }
            @if !report.comment.is_empty() { p { (report.comment) } }
            form.inline-form method="post"
                action=(format!("/web/groups/{group_id}/reports/{}/resolve", report.id)) {
                input type="hidden" name="csrf" value=(csrf);
                button type="submit" { (locale.text("groups-resolve")) }
            }
        }
    }
}

/// `POST /web/groups/{id}/reports/{report_id}/resolve`.
pub async fn report_resolve_action(
    State(state): State<AppState>,
    user: WebUser,
    Path((group_id, report_id)): Path<(i64, i64)>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if moderated_group(&state, &user, group_id).await.is_err() {
        return super::pages::not_found(&state, Some(&user), user.locale).await;
    }
    // Only resolve a report that actually belongs to this group.
    match report::find_by_id(&state.pool, report_id).await {
        Ok(Some(report)) if report.group_account_id == Some(group_id) => {
            if let Err(err) = report::resolve(&state.pool, report_id, user.current.account.id).await
            {
                return ApiError::from(err).into_response();
            }
            redirect_to(&format!("/groups/{group_id}/reports?saved=1"))
        }
        Ok(_) => super::pages::not_found(&state, Some(&user), user.locale).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Post-level moderation (remove / lock / pin) --------------------------

/// `POST /web/groups/{id}/posts/{status}/remove`.
pub async fn post_remove_action(
    State(state): State<AppState>,
    user: WebUser,
    Path((group_id, status_id)): Path<(i64, i64)>,
    body: Bytes,
) -> Response {
    let (group_account, status, back) =
        match status_ctx(&state, &user, group_id, status_id, &body).await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    match crate::groups::remove_from_group(
        &state,
        &group_account,
        &user.current.account,
        &status,
        "Removed by moderator",
    )
    .await
    {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

#[derive(Deserialize)]
pub struct LockQuery {
    unlock: Option<String>,
}

/// `POST /web/groups/{id}/posts/{status}/lock` — `?unlock=1` reopens.
pub async fn post_lock_action(
    State(state): State<AppState>,
    user: WebUser,
    Path((group_id, status_id)): Path<(i64, i64)>,
    Query(q): Query<LockQuery>,
    body: Bytes,
) -> Response {
    let (group_account, status, back) =
        match status_ctx(&state, &user, group_id, status_id, &body).await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    let lock = q.unlock.is_none();
    let root_id = match status::thread_root(&state.pool, status.id).await {
        Ok(id) => id,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let root = match status::find_by_id(&state.pool, root_id).await {
        Ok(Some(root)) => root,
        Ok(None) => return super::pages::not_found(&state, Some(&user), user.locale).await,
        Err(err) => return ApiError::from(err).into_response(),
    };
    match crate::groups::set_thread_lock(&state, &group_account, &user.current.account, &root, lock)
        .await
    {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

#[derive(Deserialize)]
pub struct PinQuery {
    unpin: Option<String>,
}

/// `POST /web/groups/{id}/posts/{status}/pin` — `?unpin=1` removes the pin.
pub async fn post_pin_action(
    State(state): State<AppState>,
    user: WebUser,
    Path((group_id, status_id)): Path<(i64, i64)>,
    Query(q): Query<PinQuery>,
    body: Bytes,
) -> Response {
    let (group_account, status, back) =
        match status_ctx(&state, &user, group_id, status_id, &body).await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    let pin_it = q.unpin.is_none();
    match crate::groups::set_group_pin(
        &state,
        &group_account,
        &user.current.account,
        &status,
        pin_it,
    )
    .await
    {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

// ---- Shared action plumbing ----------------------------------------------

/// Parse + CSRF + moderator gate + resolve the `account_id` field. Returns the
/// group account, the target account and the tab's `back` url, or an early
/// `Response` (bad form / CSRF / 404 / missing account).
async fn target_ctx(
    state: &AppState,
    user: &WebUser,
    group_id: i64,
    body: &Bytes,
    tab: &str,
) -> Result<(Account, Account, String), Response> {
    let pairs = form_pairs(body).map_err(bad_form)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Err(csrf_rejection());
    }
    let (group_account, _, _) = moderated_group(state, user, group_id).await?;
    let slug = if tab == "settings" { "manage" } else { tab };
    let back = format!("/groups/{group_id}/{slug}");
    let Some(target_id) = field(&pairs, "account_id").and_then(|s| s.parse::<i64>().ok()) else {
        return Err(redirect_refused(&back, Refusal::NoSuchAccount));
    };
    let target = match account::find_by_id(&state.pool, target_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return Err(redirect_refused(&back, Refusal::NoSuchAccount)),
        Err(err) => return Err(ApiError::from(err).into_response()),
    };
    Ok((group_account, target, back))
}

/// Parse + CSRF + moderator gate + fetch the status, which must belong to the
/// group. Returns the group account, the status and the group page `back` url.
async fn status_ctx(
    state: &AppState,
    user: &WebUser,
    group_id: i64,
    status_id: i64,
    body: &Bytes,
) -> Result<(Account, status::Status, String), Response> {
    let pairs = form_pairs(body).map_err(bad_form)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Err(csrf_rejection());
    }
    let (group_account, _, _) = moderated_group(state, user, group_id).await?;
    let status = match status::find_by_id(&state.pool, status_id).await {
        Ok(Some(status)) => status,
        Ok(None) => return Err(super::pages::not_found(state, Some(user), user.locale).await),
        Err(err) => return Err(ApiError::from(err).into_response()),
    };
    let belongs = group::groups_of_status(&state.pool, status_id)
        .await
        .is_ok_and(|gs| gs.iter().any(|g| g.account_id == group_id));
    let back = format!("/@{}", group_account.username);
    if !belongs {
        return Err(redirect_refused(&back, Refusal::PostNotInGroup));
    }
    Ok((group_account, status, back))
}
