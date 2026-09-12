//! `GET /api/v2/search` and `GET /api/v1/accounts/search` — Mastodon's
//! search API, backed by Postgres FTS (`SearchService` and friends use the
//! same database queries when Elasticsearch is disabled).

use axum::Json;
use axum::extract::{Query, State};
use plamenu_ap::acct::Acct;
use plamenu_ap::actor::RemoteActor;
use plamenu_ap::urls;
use plamenu_db::account::{self, Account, AccountSearch};
use plamenu_db::status::{self, Status, StatusSearch};
use plamenu_db::{featured_tag, follow, remote_stream_source, tag};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::truthy;
use crate::auth::{CurrentUser, MaybeUser};
use crate::entities::{self, account_json, can_view, render_accounts, render_statuses};
use crate::error::ApiError;
use crate::ingest::is_ingestible_note;
use crate::remote::{host_of, refresh_remote_actor, resolve_remote_accounts};
use crate::state::AppState;

const RESULTS_LIMIT: i64 = 20;
const ACCOUNTS_LIMIT: i64 = 40;
/// Anonymous viewers need at least this many characters for non-exact
/// account matches (Mastodon's `MIN_QUERY_LENGTH`).
const MIN_QUERY_LENGTH: usize = 3;

/// Typographic quote characters Mastodon folds to `"` before searching.
const QUOTE_EQUIVALENTS: [char; 11] = [
    '\u{201C}', '\u{201D}', '\u{201E}', '\u{AB}', '\u{BB}', '\u{300C}', '\u{300D}', '\u{300E}',
    '\u{300F}', '\u{300A}', '\u{300B}',
];

/// Mastodon's `limit_param`: absent → the default, present → its absolute
/// value capped at twice the default (unparsable input counts as 0).
fn limit_param(raw: Option<&str>, default: i64) -> i64 {
    match raw {
        None => default,
        Some(s) => s.parse::<i64>().unwrap_or(0).abs().min(default * 2),
    }
}

fn id_param(raw: Option<&str>) -> Option<i64> {
    raw.and_then(|s| s.parse::<i64>().ok())
}

#[derive(Deserialize)]
pub struct SearchV2Query {
    q: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    resolve: Option<String>,
    following: Option<String>,
    account_id: Option<String>,
    max_id: Option<String>,
    min_id: Option<String>,
    limit: Option<String>,
    offset: Option<String>,
}

/// `GET /api/v2/search`. Anonymous use requires `public_search`; when allowed
/// it runs, like Mastodon's, without pagination or remote resolution, and
/// never over statuses.
pub async fn search_v2(
    State(state): State<AppState>,
    viewer: MaybeUser,
    Query(query): Query<SearchV2Query>,
) -> Result<Json<Value>, ApiError> {
    let viewer = viewer.require_preview(state.public_search().await)?;
    if let Some(user) = &viewer {
        // Mastodon accepts `read` or `read:search`.
        user.require_scope("read:search")?;
    }
    let raw = query.q.as_deref().ok_or_else(|| {
        // Rails' `params.require(:q)` wording.
        ApiError::BadRequest("param is missing or the value is empty or invalid: q".into())
    })?;
    if viewer.is_none() {
        if query.offset.is_some() {
            return Err(ApiError::Unauthorized(
                "Search queries pagination is not supported without authentication".into(),
            ));
        }
        if truthy(query.resolve.as_deref()) {
            return Err(ApiError::Unauthorized(
                "Search queries that resolve remote resources are not supported without authentication"
                    .into(),
            ));
        }
    }

    let q: String = raw
        .trim()
        .chars()
        .map(|c| {
            if QUOTE_EQUIVALENTS.contains(&c) {
                '"'
            } else {
                c
            }
        })
        .collect();
    let limit = limit_param(query.limit.as_deref(), RESULTS_LIMIT);
    let kind = query.kind.as_deref().filter(|k| !k.is_empty());
    // Offsets only apply to single-type searches, like Mastodon's.
    let offset = if kind.is_some() {
        id_param(query.offset.as_deref()).unwrap_or(0).max(0)
    } else {
        0
    };
    let resolve = truthy(query.resolve.as_deref());

    let mut results = json!({
        "accounts": [],
        "statuses": [],
        "hashtags": [],
        "collections": [],
    });
    if q.is_empty() || limit == 0 {
        return Ok(Json(results));
    }
    let viewer_id = viewer.as_ref().map(|u| u.account.id);

    // A URL with `resolve` dereferences that one resource instead of
    // searching.
    if resolve && (q.starts_with("https://") || q.starts_with("http://")) {
        if offset == 0 {
            Box::pin(url_search(&state, &q, kind, viewer_id, &mut results)).await?;
        }
        return Ok(Json(results));
    }

    if kind.is_none_or(|k| k == "accounts") {
        let opts = AccountSearchOpts {
            limit,
            offset,
            resolve,
            following: truthy(query.following.as_deref()),
        };
        let found = search_accounts(&state, viewer.as_ref().map(|u| &u.account), &q, opts).await?;
        let entities =
            render_accounts(&state.pool, &state.config.domain, &found, viewer_id).await?;
        results["accounts"] = Value::Array(entities);
    }

    // Status search is viewer-scoped; Mastodon skips it entirely when
    // anonymous.
    if kind.is_none_or(|k| k == "statuses")
        && let Some(user) = &viewer
    {
        results["statuses"] =
            Value::Array(search_statuses(&state, user, &q, &query, limit, offset).await?);
    }

    if kind.is_none_or(|k| k == "hashtags") {
        results["hashtags"] =
            Value::Array(search_hashtags(&state, viewer_id, &q, limit, offset).await?);
    }
    Ok(Json(results))
}

/// The `hashtags` slice of a search: prefix-matched tags rendered as `Tag`
/// entities, with `following`/`featuring` resolved for the whole page in one
/// query each (only for an authenticated viewer).
async fn search_hashtags(
    state: &AppState,
    viewer_id: Option<i64>,
    q: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<Value>, ApiError> {
    let term = q.strip_prefix('#').unwrap_or(q);
    let found = tag::search(&state.pool, term, limit, offset).await?;
    let (following, featuring) = match viewer_id {
        Some(id) => {
            let ids: Vec<i64> = found.iter().map(|t| t.id).collect();
            (
                tag::followed_ids(&state.pool, id, &ids).await?,
                featured_tag::featured_ids(&state.pool, id, &ids).await?,
            )
        }
        None => (
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        ),
    };
    entities::tag_json_page(
        &state.pool,
        &state.config.domain,
        &found,
        &following,
        &featuring,
        viewer_id.is_some(),
    )
    .await
}

#[derive(Deserialize)]
pub struct AccountSearchV1Query {
    q: Option<String>,
    resolve: Option<String>,
    following: Option<String>,
    limit: Option<String>,
}

/// `GET /api/v1/accounts/search` — the account-autocomplete endpoint.
/// Requires a user, like Mastodon's.
pub async fn search_accounts_v1(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<AccountSearchV1Query>,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `read` or `read:accounts`.
    current.require_scope("read:accounts")?;
    let q = query.q.unwrap_or_default();
    let opts = AccountSearchOpts {
        limit: limit_param(query.limit.as_deref(), ACCOUNTS_LIMIT),
        offset: 0,
        resolve: truthy(query.resolve.as_deref()),
        following: truthy(query.following.as_deref()),
    };
    let found = search_accounts(&state, Some(&current.account), &q, opts).await?;
    let entities = render_accounts(
        &state.pool,
        &state.config.domain,
        &found,
        Some(current.account.id),
    )
    .await?;
    Ok(Json(Value::Array(entities)))
}

/// Fills `results` with the single resource a URL query dereferences to,
/// when the `type` filter admits it.
async fn url_search(
    state: &AppState,
    q: &str,
    kind: Option<&str>,
    viewer_id: Option<i64>,
    results: &mut Value,
) -> Result<(), ApiError> {
    match Box::pin(resolve_url(state, q, viewer_id)).await? {
        Some(UrlResource::Account(found)) if kind.is_none_or(|k| k == "accounts") => {
            results["accounts"] =
                json!([account_json(&state.pool, &state.config.domain, &found, viewer_id).await?]);
        }
        Some(UrlResource::Status(found)) if kind.is_none_or(|k| k == "statuses") => {
            let rendered = render_statuses(
                &state.pool,
                &state.config.domain,
                std::slice::from_ref(&*found),
                viewer_id,
            )
            .await?;
            results["statuses"] = Value::Array(rendered);
        }
        _ => {}
    }
    Ok(())
}

/// The `statuses` part of a combined search, including the deprecated
/// `account_id`/`max_id`/`min_id` filter parameters.
async fn search_statuses(
    state: &AppState,
    user: &CurrentUser,
    q: &str,
    query: &SearchV2Query,
    limit: i64,
    offset: i64,
) -> Result<Vec<Value>, ApiError> {
    let account_id = match query.account_id.as_deref() {
        // An unknown `account_id` is a 404, like Mastodon's `find`.
        Some(raw_id) => {
            let target_id = raw_id.parse::<i64>().map_err(|_| ApiError::NotFound)?;
            account::find_by_id(&state.pool, target_id)
                .await?
                .ok_or(ApiError::NotFound)?;
            Some(target_id)
        }
        None => None,
    };
    let found = status::search(
        &state.pool,
        q,
        &StatusSearch {
            viewer: user.account.id,
            account_id,
            max_id: id_param(query.max_id.as_deref()),
            min_id: id_param(query.min_id.as_deref()),
            limit,
            offset,
        },
    )
    .await?;
    render_statuses(
        &state.pool,
        &state.config.domain,
        &found,
        Some(user.account.id),
    )
    .await
}

pub(crate) struct AccountSearchOpts {
    pub(crate) limit: i64,
    pub(crate) offset: i64,
    pub(crate) resolve: bool,
    pub(crate) following: bool,
}

/// How a `user@domain` search term's prefix narrows the actor kind.
#[derive(Clone, Copy)]
enum HandleMode {
    /// `!name@host` — communities/groups only.
    GroupOnly,
    /// `@name@host` — the person-like actor, falling back to a same-named group
    /// when no person exists (Mastodon-compatible, and how `@community` has
    /// always resolved).
    PersonPreferred,
    /// bare `name@host` — every actor answering the handle (person and group).
    Both,
}

/// The exact remote actors a `user@domain` term resolves to under `mode`,
/// either from cache (`resolve == false`) or via `WebFinger` discovery.
async fn exact_handle_candidates(
    state: &AppState,
    acct: &Acct,
    mode: HandleMode,
    resolve: bool,
) -> Result<Vec<Account>, ApiError> {
    use account::ActorClass;
    let (username, domain) = (acct.username(), acct.domain());
    Ok(match mode {
        HandleMode::GroupOnly if resolve => {
            resolve_remote_accounts(state, acct, ActorClass::Group).await?
        }
        HandleMode::GroupOnly => {
            account::find_remote_by_acct_class(&state.pool, username, domain, ActorClass::Group)
                .await?
        }
        HandleMode::Both if resolve => {
            resolve_remote_accounts(state, acct, ActorClass::Any).await?
        }
        HandleMode::Both => account::find_remote_by_acct_all(&state.pool, username, domain).await?,
        HandleMode::PersonPreferred if resolve => {
            let people = resolve_remote_accounts(state, acct, ActorClass::PersonLike).await?;
            if people.is_empty() {
                resolve_remote_accounts(state, acct, ActorClass::Group).await?
            } else {
                people
            }
        }
        HandleMode::PersonPreferred => {
            let people = account::find_remote_by_acct_class(
                &state.pool,
                username,
                domain,
                ActorClass::PersonLike,
            )
            .await?;
            if people.is_empty() {
                account::find_remote_by_acct_class(&state.pool, username, domain, ActorClass::Group)
                    .await?
            } else {
                people
            }
        }
    })
}

/// Mastodon's `AccountSearchService`: an exact `user@domain` match first
/// (webfinger-resolved with `resolve`), then ranked full-text matches.
/// Shared with the web composer's autocomplete endpoint.
pub(crate) async fn search_accounts(
    state: &AppState,
    viewer: Option<&Account>,
    raw_query: &str,
    opts: AccountSearchOpts,
) -> Result<Vec<Account>, ApiError> {
    let trimmed = raw_query.trim();
    // A leading `!` asks for a community only (Lemmy `!name@host`), `@` prefers
    // a person but falls back to a same-named group (so `@community@host` still
    // resolves the community, as it always has); a bare handle surfaces BOTH
    // when a host serves a Person and a Group under one name.
    let (mode, q) = if let Some(rest) = trimmed.strip_prefix('!') {
        (HandleMode::GroupOnly, rest)
    } else if let Some(rest) = trimmed.strip_prefix('@') {
        (HandleMode::PersonPreferred, rest)
    } else {
        (HandleMode::Both, trimmed)
    };
    if q.is_empty() || opts.limit < 1 {
        return Ok(Vec::new());
    }
    // `following` only means something for a signed-in viewer.
    let following = opts.following && viewer.is_some();

    // Exact `user@domain` matches — possibly two, when a handle names both a
    // Person and a Group and the query didn't disambiguate with `@`/`!`.
    let mut exact: Vec<Account> = Vec::new();
    if opts.offset == 0
        && let Some((username, domain)) = q.split_once('@')
        && let Ok(acct) = Acct::new(username, domain)
    {
        let candidates = if state.config.is_local_domain(acct.domain()) {
            // Local handles are unambiguous (one namespace); return the row
            // whatever the `!`/`@` prefix.
            account::find_public_local_account_by_username(&state.pool, acct.username())
                .await?
                .into_iter()
                .collect()
        } else {
            exact_handle_candidates(state, &acct, mode, opts.resolve).await?
        };
        for found in candidates {
            if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &found)
                .await?
            {
                continue;
            }
            // With `following`, an exact match must itself be followed (and
            // `following` is only set when there is a viewer).
            if let Some(viewer) = viewer.filter(|_| following) {
                let followed = follow::find(&state.pool, viewer.id, found.id)
                    .await?
                    .is_some_and(|edge| !edge.pending);
                if !followed {
                    continue;
                }
            }
            exact.push(found);
        }
    }

    let mut remaining = opts.limit - i64::try_from(exact.len()).unwrap_or(0);
    if viewer.is_none() && q.chars().count() < MIN_QUERY_LENGTH {
        remaining = 0;
    }
    // For `user@ourdomain` only the username part is a useful search term.
    let terms = match q.split_once('@') {
        Some((username, domain)) if state.config.is_local_domain(domain) => username,
        _ => q,
    };
    let mut found = if remaining > 0 {
        account::search(
            &state.pool,
            &AccountSearch {
                terms,
                viewer: viewer.map(|v| v.id),
                following,
                limit: remaining,
                offset: opts.offset,
            },
        )
        .await?
    } else {
        Vec::new()
    };
    let mut visible = Vec::with_capacity(found.len());
    for account in found {
        if crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account)
            .await?
        {
            visible.push(account);
        }
    }
    found = visible;
    if !exact.is_empty() {
        let exact_ids: std::collections::HashSet<i64> = exact.iter().map(|a| a.id).collect();
        found.retain(|a| !exact_ids.contains(&a.id));
        // Exact matches lead, in resolution order (person before group).
        for (index, account) in exact.into_iter().enumerate() {
            found.insert(index, account);
        }
    }
    Ok(found)
}

pub enum UrlResource {
    Account(Box<Account>),
    Status(Box<Status>),
}

/// Returns the status as a search result if the viewer may see it.
fn viewable_resource(viewable: bool, found: Status) -> Option<UrlResource> {
    viewable.then_some(UrlResource::Status(Box::new(found)))
}

/// What the database alone says about a URL — [`resolve_url`]'s first half.
pub enum KnownUrl {
    Found(UrlResource),
    /// Local-but-nothing, policy-refused, or not viewable: resolution stops
    /// here, a network fetch must not follow.
    Refused,
    /// An unknown remote object, eligible for a fetch.
    Unknown,
}

/// The storage-only half of [`resolve_url`]: our own URLs and already-known
/// remote URIs. This is all an anonymous `/web/go` click gets —
/// resolution fetches are reserved for signed-in viewers.
pub async fn resolve_url_known(
    state: &AppState,
    url: &str,
    viewer: Option<i64>,
) -> Result<KnownUrl, ApiError> {
    let domain = &state.config.domain;
    // Local URLs never go through the network.
    if crate::local_identity::has_local_actor_shape(domain, url) {
        return Ok(crate::local_identity::find_actor(&state.pool, domain, url)
            .await?
            .map_or(KnownUrl::Refused, |found| {
                KnownUrl::Found(UrlResource::Account(Box::new(found)))
            }));
    }
    if urls::parse_local_status_url(domain, url).is_some()
        || urls::parse_local_numeric_status_url(domain, url).is_some()
    {
        let Some(found) = crate::ingest::resolve_status_ref(state, url).await? else {
            return Ok(KnownUrl::Refused);
        };
        let viewable = can_view(&state.pool, &found, viewer).await?;
        return Ok(viewable_resource(viewable, found).map_or(KnownUrl::Refused, KnownUrl::Found));
    }
    if url.starts_with(&format!("https://{domain}/")) {
        return Ok(KnownUrl::Refused); // some other local route: nothing to resolve
    }
    if !crate::instance_policy::can_federate_url(&state.pool, domain, url).await? {
        return Ok(KnownUrl::Refused);
    }

    // Known remote objects.
    if let Some(found) = account::find_by_uri(&state.pool, url).await? {
        if account::is_internal(&state.pool, found.id).await?
            || !crate::instance_policy::account_visible(&state.pool, domain, &found).await?
        {
            return Ok(KnownUrl::Refused);
        }
        return Ok(KnownUrl::Found(UrlResource::Account(Box::new(found))));
    }
    if let Some(found) = status::find_by_uri(&state.pool, url).await? {
        let viewable = can_view(&state.pool, &found, viewer).await?;
        return Ok(viewable_resource(viewable, found).map_or(KnownUrl::Refused, KnownUrl::Found));
    }
    // ActivityPub actor/object ids often differ from their human-facing URLs.
    // Discourse uses random `/ap/...` ids while clients paste `/c/...` and
    // `/t/...` links, so check the stored permalink identity as well.
    if let Some(found) = account::find_by_url(&state.pool, url).await? {
        if account::is_internal(&state.pool, found.id).await?
            || !crate::instance_policy::account_visible(&state.pool, domain, &found).await?
        {
            return Ok(KnownUrl::Refused);
        }
        return Ok(KnownUrl::Found(UrlResource::Account(Box::new(found))));
    }
    if let Some(found) = status::find_by_url(&state.pool, url).await? {
        let viewable = can_view(&state.pool, &found, viewer).await?;
        return Ok(viewable_resource(viewable, found).map_or(KnownUrl::Refused, KnownUrl::Found));
    }
    // Owncast's homepage is the stream's canonical human URL but is not its
    // actor id or actor `url` on every released version. Once positively
    // discovered, remember that exact mapping like the ordinary `url` fields.
    if let Some(source) = remote_stream_source::find_by_homepage(
        &state.pool,
        &normalized_web_url(url).unwrap_or_else(|| url.to_owned()),
    )
    .await?
        && let Some(found) = account::find_by_id(&state.pool, source.account_id).await?
    {
        if account::is_internal(&state.pool, found.id).await?
            || !crate::instance_policy::account_visible(&state.pool, domain, &found).await?
        {
            return Ok(KnownUrl::Refused);
        }
        return Ok(KnownUrl::Found(UrlResource::Account(Box::new(found))));
    }
    Ok(KnownUrl::Unknown)
}

fn normalized_web_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    url.set_fragment(None);
    if url.path().len() > 1 {
        let trimmed = url.path().trim_end_matches('/').to_owned();
        url.set_path(&trimmed);
    }
    Some(url.to_string())
}

fn discourse_category_acct(raw: &str) -> Option<Acct> {
    let url = url::Url::parse(raw).ok()?;
    let host = url.host_str()?;
    let segments: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    if segments.first().copied() != Some("c") || segments.len() < 2 {
        return None;
    }
    // `/c/slug`, `/c/slug/id`, and nested `/c/parent/slug/id` all name the
    // last non-numeric path component before the optional numeric category id.
    let slug = segments[1..]
        .iter()
        .rev()
        .find(|part| !part.chars().all(|c| c.is_ascii_digit()))?;
    Acct::new(slug, host).ok()
}

/// Discourse category pages do not advertise an AP alternate. The plugin's
/// default actor handle is the category slug, so `WebFinger` that candidate and
/// accept it only when the returned Group declares the exact page as its URL.
async fn resolve_discourse_category_url(
    state: &AppState,
    raw: &str,
) -> Result<Option<Account>, ApiError> {
    let Some(acct) = discourse_category_acct(raw) else {
        return Ok(None);
    };
    let expected = normalized_web_url(raw);
    let candidates = resolve_remote_accounts(state, &acct, account::ActorClass::Group).await?;
    Ok(candidates.into_iter().find(|candidate| {
        candidate
            .url
            .as_deref()
            .and_then(normalized_web_url)
            .as_ref()
            == expected.as_ref()
    }))
}

fn discourse_topic_json_target(raw: &str) -> Option<(String, Option<i64>)> {
    let mut url = url::Url::parse(raw).ok()?;
    let segments: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    if segments.first().copied() != Some("t") || segments.len() < 2 {
        return None;
    }
    let topic_id_index = segments[1..]
        .iter()
        .position(|part| part.chars().all(|c| c.is_ascii_digit()))?
        + 1;
    let post_number = segments
        .get(topic_id_index + 1)
        .and_then(|part| part.parse::<i64>().ok());
    let path = format!("{}.json", url.path().trim_end_matches('/'));
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Some((url.to_string(), post_number))
}

/// The Discourse plugin exposes the canonical AP object id in public topic
/// JSON even though the HTML topic URL answers AP content negotiation with
/// 406. Read that one same-origin hint and let the ordinary AP fetch/ingest
/// path validate the resulting object and author.
async fn discover_discourse_topic_object(state: &AppState, raw: &str) -> Option<String> {
    let (json_url, post_number) = discourse_topic_json_target(raw)?;
    let page = state
        .federation
        .fetch_page(&json_url, "application/json")
        .await
        .ok()?;
    let document: Value = serde_json::from_str(&page.body).ok()?;
    let candidate = if let Some(post_number) = post_number {
        document
            .pointer("/post_stream/posts")?
            .as_array()?
            .iter()
            .find(|post| post.get("post_number").and_then(Value::as_i64) == Some(post_number))?
            .get("activity_pub_object_id")?
            .as_str()?
    } else {
        document
            .get("activity_pub_object_id")
            .and_then(Value::as_str)
            .or_else(|| {
                document
                    .pointer("/post_stream/posts/0/activity_pub_object_id")
                    .and_then(Value::as_str)
            })?
    };
    let source = url::Url::parse(raw).ok()?;
    let target = url::Url::parse(candidate).ok()?;
    (source.origin() == target.origin()).then(|| candidate.to_owned())
}

/// Mastodon's `ResolveURLService`: our own URLs answer from storage, known
/// remote URIs from the database, anything else by fetching the object
/// (and, for notes, its author). Failures are an empty result, not an
/// error.
pub async fn resolve_url(
    state: &AppState,
    url: &str,
    viewer: Option<i64>,
) -> Result<Option<UrlResource>, ApiError> {
    let domain = &state.config.domain;
    match resolve_url_known(state, url, viewer).await? {
        KnownUrl::Found(UrlResource::Status(found)) => {
            // Exact resolution is also the repair path for posts stored before
            // their linked page became crawlable (or before support for that
            // page's media metadata existed). The crawler is a no-op when the
            // status already has a card, attachment or quote, and performs no
            // network request when its body has no eligible link.
            crate::link_preview::crawl_status(state, found.id).await?;
            return Ok(Some(UrlResource::Status(found)));
        }
        KnownUrl::Found(found) => return Ok(Some(found)),
        KnownUrl::Refused => return Ok(None),
        KnownUrl::Unknown => {}
    }

    // Owncast root pages do not expose an AP alternate. A tightly-scoped
    // NodeInfo + public-config + WebFinger loopback resolves that exact URL to
    // its Service actor and retains the advertised HLS capability.
    if let Some(found) = crate::owncast::discover_homepage(state, url).await? {
        if crate::instance_policy::account_visible(&state.pool, domain, &found).await? {
            return Ok(Some(UrlResource::Account(Box::new(found))));
        }
        return Ok(None);
    }

    // Discourse's browser routes deliberately answer AP negotiation with 406.
    // Recognize their stable route shapes before a generic AP fetch so a
    // successful plugin-specific discovery does not poison the original HTML
    // URL's finite failure budget.
    if discourse_category_acct(url).is_some()
        && let Ok(Some(found)) = resolve_discourse_category_url(state, url).await
    {
        if crate::instance_policy::account_visible(&state.pool, domain, &found).await? {
            return Ok(Some(UrlResource::Account(Box::new(found))));
        }
        return Ok(None);
    }
    let discovered_topic = discover_discourse_topic_object(state, url).await;

    // Fresh fetch. The query may be a permalink (an object's `url`, like
    // Mastodon's `/@user/123`) rather than its canonical `id`, so follow to
    // and validate that id. A Discourse JSON object id is authoritative for
    // that same-origin page; all other URLs use ordinary AP discovery.
    let fetch_url = discovered_topic.as_deref().unwrap_or(url);
    let Ok(object) = state.federation.fetch_object_following(fetch_url).await else {
        return Ok(None);
    };
    // Storage keys on the canonical id, so a permalink fetch can land on an
    // object we already have — re-check by it before ingesting.
    let canonical = object.get("id").and_then(Value::as_str).unwrap_or(url);
    if canonical != url {
        if let Some(found) = account::find_by_uri(&state.pool, canonical).await? {
            if account::is_internal(&state.pool, found.id).await?
                || !crate::instance_policy::account_visible(&state.pool, domain, &found).await?
            {
                return Ok(None);
            }
            return Ok(Some(UrlResource::Account(Box::new(found))));
        }
        if let Some(found) = status::find_by_uri(&state.pool, canonical).await? {
            let viewable = can_view(&state.pool, &found, viewer).await?;
            return Ok(viewable_resource(viewable, found));
        }
    }
    match object.get("type").and_then(Value::as_str) {
        Some("Person" | "Service" | "Application" | "Group" | "Organization") => {
            let Ok(actor) = serde_json::from_value::<RemoteActor>(object.clone()) else {
                return Ok(None);
            };
            if !crate::instance_policy::can_federate_url(&state.pool, domain, &actor.id).await? {
                return Ok(None);
            }
            let stored = refresh_remote_actor(state, &actor).await?;
            Ok(Some(UrlResource::Account(Box::new(stored))))
        }
        _ if is_ingestible_note(&object) => {
            let Some(attributed_to) = plamenu_ap::activity::attributed_to_id(&object) else {
                return Ok(None);
            };
            // The claimed author must live on the note's own host.
            if host_of(attributed_to) != host_of(canonical) {
                return Ok(None);
            }
            let author = if let Some(known) =
                account::find_by_uri(&state.pool, attributed_to).await?
            {
                if !crate::instance_policy::account_visible(&state.pool, domain, &known).await? {
                    return Ok(None);
                }
                known
            } else {
                if !crate::instance_policy::can_federate_url(&state.pool, domain, attributed_to)
                    .await?
                {
                    return Ok(None);
                }
                let Ok(actor) = state.federation.fetch_actor(attributed_to).await else {
                    return Ok(None);
                };
                refresh_remote_actor(state, &actor).await?
            };
            let stored = crate::ingest::ingest_remote_note_in_context(
                state,
                &author,
                &object,
                crate::ingest::RemoteIngestContext::ExplicitResolution,
            )
            .await?;
            let viewable = can_view(&state.pool, &stored, viewer).await?;
            Ok(viewable_resource(viewable, stored))
        }
        _ => Ok(None),
    }
}
