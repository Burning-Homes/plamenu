//! Mastodon API entity serialization. Shapes (and string-typed ids) follow
//! the documented Mastodon entities so existing clients parse them.

use std::collections::{HashMap, HashSet};

use plamenu_ap::urls::{LocalStatusUrls, LocalUserUrls};
use plamenu_db::account::Account;
use plamenu_db::admin_account::{self, AdminAccountView};
use plamenu_db::announcement::{self, ReactionGroup};
use plamenu_db::custom_emoji::{self, CustomEmoji};
use plamenu_db::instance_policy::{
    CanonicalEmailBlock, DomainAllow, DomainBlock, EmailDomainBlock, IpBlock,
};
use plamenu_db::media::{Media, MediaRendition};
use plamenu_db::notification::Notification;
use plamenu_db::oauth::App;
use plamenu_db::poll::Poll;
use plamenu_db::preview_card::PreviewCard;
use plamenu_db::report::Report;
use plamenu_db::rule::{Rule, RuleTranslation};
use plamenu_db::scheduled_status::ScheduledStatus;
use plamenu_db::status::{Engagement, Status};
use plamenu_db::user::UserSettings;
use plamenu_db::{
    PgPool, account, account_note, block, bookmark, collection, conversation, endorsement,
    favourite, featured_tag, follow, media, mention, mute, oauth, pin, poll, preview_card,
    preview_card_trend, quote, reaction, rule, status, tag, tagged_object, user,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::compose::{Composed, compose_local};
use crate::error::ApiError;
use crate::instance_policy::sha256_hex;
use crate::state::AppState;

pub fn rfc3339(t: time::OffsetDateTime) -> Result<String, ApiError> {
    t.format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))
}

/// The REST `Application` entity (Mastodon's `ApplicationSerializer`).
#[must_use]
pub fn application_json(app: &App) -> Value {
    json!({
        "id": app.id.to_string(),
        "name": app.name,
        "website": app.website,
        "scopes": app.scopes.split_whitespace().collect::<Vec<_>>(),
        "redirect_uri": app.redirect_uris.join("\n"),
        "redirect_uris": app.redirect_uris,
        "vapid_key": "",
    })
}

/// The nested `application` object on a Mastodon `Status` entity is smaller
/// than the app-registration entity: only client name and website.
#[must_use]
fn status_application_json(app: &App) -> Value {
    json!({
        "name": app.name,
        "website": app.website.as_deref().filter(|value| !value.is_empty()),
    })
}

/// The `ActivityPub` id of a status (stored for remote, derived for local).
#[must_use]
pub fn status_uri(domain: &str, item: &Status, author_username: &str) -> String {
    item.uri.clone().unwrap_or_else(|| {
        format!(
            "{}/statuses/{}",
            LocalUserUrls::new(domain, author_username).id,
            item.id
        )
    })
}

/// The `ActivityPub` id of a status using its author's persisted canonical actor
/// URI. The legacy helper above remains for import/export fixtures that only
/// carry a username; live federation paths must use this form.
#[must_use]
pub fn status_uri_for_account(domain: &str, item: &Status, author: &Account) -> String {
    item.uri.clone().unwrap_or_else(|| {
        let actor = account_uri(domain, author);
        LocalStatusUrls::from_actor_id(domain, &author.username, &actor, item.id).id
    })
}

/// The `ActivityPub` actor id of an account (stored for remote, derived for
/// local) — the URI form used when referring to the account in activities.
#[must_use]
pub fn account_uri(domain: &str, account: &Account) -> String {
    account
        .uri
        .clone()
        .unwrap_or_else(|| LocalUserUrls::new(domain, &account.username).id)
}

/// The human web URL of an account (`url`), distinct from its AP id (`uri`):
/// `/@username` for local accounts, the origin's published `url` for remote
/// ones (falling back to the AP id when it was never captured), like
/// Mastodon's `TagManager#url_for`.
#[must_use]
pub fn account_web_url(domain: &str, account: &Account) -> String {
    if account.has_local_account_on(domain) {
        LocalUserUrls::new(domain, &account.username).web_url
    } else {
        account
            .url
            .clone()
            .unwrap_or_else(|| account_uri(domain, account))
    }
}

/// Mastodon `acct` form for an account. Portable gateway accounts occupy the
/// local handle namespace even though their actor identity remains remote.
#[must_use]
pub fn account_acct(domain: &str, account: &Account) -> String {
    if account.has_local_account_on(domain) {
        account.username.clone()
    } else if let Some(remote) = account.domain.as_deref() {
        format!("{}@{remote}", account.username)
    } else {
        account.username.clone()
    }
}

/// The account-level `canFeature` bitmap Mastodon exposes as
/// `feature_approval`: local accounts derive it from discoverability and lock
/// state, while remote accounts use the parsed actor policy.
#[must_use]
pub fn feature_policy_bitmap(account: &Account) -> i32 {
    if !account.is_local() {
        return account.feature_approval_policy;
    }
    if !account.discoverable.unwrap_or(false) {
        0
    } else if account.locked {
        plamenu_ap::quote_policy::AUTOMATIC_FOLLOWERS
    } else {
        plamenu_ap::quote_policy::AUTOMATIC_PUBLIC
    }
}

async fn accepted_follow(
    pool: &PgPool,
    follower_id: i64,
    target_id: i64,
) -> Result<bool, ApiError> {
    Ok(follow::find(pool, follower_id, target_id)
        .await?
        .is_some_and(|edge| !edge.pending))
}

/// Where the per-account facts a render needs (accepted-follow edges for
/// `feature_approval`, and the local `users` overlay) come from. `Live`
/// resolves each on demand (lazily, for a single account); `Batched` answers
/// from data already loaded in bulk for a whole page (see [`FeatureRelations`]
/// and [`user::account_entity_overlay_batch`]) so a list render costs a fixed
/// number of queries instead of a handful per row. Both feed the *same*
/// decision code, so the batched and single paths cannot drift.
enum AccountRenderCtx<'a> {
    Live(&'a PgPool),
    /// The render of a migration target itself: facts resolve live, but the
    /// entity never carries its own `moved` (Mastodon's
    /// `moved_and_not_nested?`), so a chained or cyclic `movedTo` cannot
    /// recurse.
    Nested(&'a PgPool),
    Batched {
        relations: &'a FeatureRelations,
        overlays: &'a HashMap<i64, user::AccountEntityOverlay>,
        silenced_domains: &'a HashSet<String>,
        /// Migration targets pre-rendered once for the page (see
        /// [`moved_targets_map`]), keyed by the moving account's id.
        moved: &'a HashMap<i64, Value>,
        /// Profile custom emoji pre-rendered once for the page (see
        /// [`profile_emoji_map`]), keyed by account id — an account absent
        /// here has none.
        emojis: &'a HashMap<i64, Vec<Value>>,
    },
}

impl AccountRenderCtx<'_> {
    /// Whether `viewer_id` has an accepted follow of `target_id`.
    async fn viewer_follows(&self, viewer_id: i64, target_id: i64) -> Result<bool, ApiError> {
        match self {
            AccountRenderCtx::Live(pool) | AccountRenderCtx::Nested(pool) => {
                accepted_follow(pool, viewer_id, target_id).await
            }
            AccountRenderCtx::Batched { relations, .. } => {
                Ok(relations.follows_out.contains(&target_id))
            }
        }
    }

    /// Whether `target_id` has an accepted follow of `viewer_id`.
    async fn target_follows(&self, target_id: i64, viewer_id: i64) -> Result<bool, ApiError> {
        match self {
            AccountRenderCtx::Live(pool) | AccountRenderCtx::Nested(pool) => {
                accepted_follow(pool, target_id, viewer_id).await
            }
            AccountRenderCtx::Batched { relations, .. } => {
                Ok(relations.followed_by.contains(&target_id))
            }
        }
    }

    /// The local-account overlay (noindex + highlighted role) for `account_id`,
    /// or `None` for an account with no `users` row.
    async fn overlay(
        &self,
        pool: &PgPool,
        account_id: i64,
    ) -> Result<Option<user::AccountEntityOverlay>, ApiError> {
        match self {
            AccountRenderCtx::Live(_) | AccountRenderCtx::Nested(_) => {
                Ok(user::account_entity_overlay(pool, account_id).await?)
            }
            AccountRenderCtx::Batched { overlays, .. } => Ok(overlays.get(&account_id).cloned()),
        }
    }

    /// Whether `domain` (a remote account's home server) carries a `silence`
    /// domain block, making its accounts `limited`. `None` (a local account)
    /// is never domain-silenced.
    async fn domain_silenced(&self, domain: Option<&str>) -> Result<bool, ApiError> {
        let Some(domain) = domain else {
            return Ok(false);
        };
        match self {
            AccountRenderCtx::Live(pool) | AccountRenderCtx::Nested(pool) => {
                Ok(plamenu_db::instance_policy::is_domain_silenced(pool, domain).await?)
            }
            AccountRenderCtx::Batched {
                silenced_domains, ..
            } => Ok(silenced_domains.contains(domain)),
        }
    }

    /// The account's profile custom emoji (bio, display name, fields —
    /// [`account_emojifiable_text`]). `Live`/`Nested` scan and look them up
    /// on demand; `Batched` answers from the page's pre-rendered map, so the
    /// batched renderers really do cost a fixed number of queries even when
    /// profiles carry `:shortcode:` references.
    async fn profile_emojis(
        &self,
        pool: &PgPool,
        domain: &str,
        account: &Account,
        allow_direct: bool,
    ) -> Result<Vec<Value>, ApiError> {
        match self {
            AccountRenderCtx::Live(_) | AccountRenderCtx::Nested(_) => {
                if account.domain.is_none() {
                    crate::emoji::emojis_json_for_account(
                        pool,
                        domain,
                        account.id,
                        &[&account_emojifiable_text(account)],
                        allow_direct,
                    )
                    .await
                } else {
                    crate::emoji::emojis_json(
                        pool,
                        domain,
                        account.domain.as_deref(),
                        &[&account_emojifiable_text(account)],
                        allow_direct,
                    )
                    .await
                }
            }
            AccountRenderCtx::Batched { emojis, .. } => {
                Ok(emojis.get(&account.id).cloned().unwrap_or_default())
            }
        }
    }

    /// The rendered `moved` attachment for `account`, or `None` when it has
    /// not migrated (the caller omits the key). `Live` resolves the target on
    /// demand; `Batched` answers from the page's pre-rendered map; `Nested`
    /// never attaches one, so a chained or cyclic `movedTo` cannot recurse.
    async fn moved(
        &self,
        pool: &PgPool,
        domain: &str,
        account: &Account,
        viewer: Option<i64>,
    ) -> Result<Option<Value>, ApiError> {
        match self {
            AccountRenderCtx::Live(_) => moved_account_json(pool, domain, account, viewer).await,
            AccountRenderCtx::Nested(_) => Ok(None),
            AccountRenderCtx::Batched { moved, .. } => Ok(moved.get(&account.id).cloned()),
        }
    }
}

/// The viewer's accepted follow edges to and from a batch of accounts,
/// resolved in two queries so [`render_accounts`] and the status renderer can
/// answer every per-account feature-policy / quote-policy question without a
/// round trip per row.
#[derive(Default)]
pub struct FeatureRelations {
    /// Of the batch, the accounts the viewer accepted-follows (viewer → row).
    follows_out: HashSet<i64>,
    /// Of the batch, the accounts that accepted-follow the viewer (row → viewer).
    followed_by: HashSet<i64>,
}

impl FeatureRelations {
    /// Loads the viewer's in/out accepted edges against `account_ids`. An
    /// anonymous viewer follows and is followed by nobody, so it stays empty
    /// without touching the database.
    pub async fn load(
        pool: &PgPool,
        viewer: Option<i64>,
        account_ids: &[i64],
    ) -> Result<Self, ApiError> {
        let Some(viewer_id) = viewer else {
            return Ok(Self::default());
        };
        Ok(Self {
            follows_out: follow::accepted_out_batch(pool, viewer_id, account_ids).await?,
            followed_by: follow::accepted_in_batch(pool, viewer_id, account_ids).await?,
        })
    }
}

/// Mastodon's `feature_policy_for_account`: the viewer's current feature
/// approval state for `target`, resolving accepted-follow facts lazily off the
/// pool. The batched list paths call [`feature_policy_with`] directly with a
/// prefetched [`AccountRenderCtx`].
pub async fn feature_policy_for_account(
    pool: &PgPool,
    target: &Account,
    viewer: Option<i64>,
) -> Result<&'static str, ApiError> {
    feature_policy_with(&AccountRenderCtx::Live(pool), target, viewer).await
}

/// The shared feature-policy decision, parameterized on where accepted-follow
/// facts come from. Preserves the lazy evaluation order of Mastodon's
/// `feature_policy_for_account` (a fact is only queried when the branch that
/// needs it is reached) — with a `Prefetched` source every lookup is a free
/// set membership test.
async fn feature_policy_with(
    src: &AccountRenderCtx<'_>,
    target: &Account,
    viewer: Option<i64>,
) -> Result<&'static str, ApiError> {
    let Some(viewer_id) = viewer else {
        return Ok("denied");
    };
    if target.is_local() {
        if !target.discoverable.unwrap_or(false) {
            return Ok("denied");
        }
        if !target.locked || target.id == viewer_id {
            return Ok("automatic");
        }
        return Ok(if src.viewer_follows(viewer_id, target.id).await? {
            "automatic"
        } else {
            "denied"
        });
    }
    if target.id == viewer_id {
        return Ok("automatic");
    }
    if target.feature_approval_policy == 0 {
        return Ok("missing");
    }

    let policy = plamenu_ap::quote_policy::QuotePolicy::from_bitmap(target.feature_approval_policy);
    let automatic = policy.automatic();
    let manual = policy.manual();
    let mut viewer_follows_target = None;
    let mut target_follows_viewer = None;

    if automatic.public() {
        return Ok("automatic");
    }
    if automatic.followers() {
        let allowed = src.viewer_follows(viewer_id, target.id).await?;
        viewer_follows_target = Some(allowed);
        if allowed {
            return Ok("automatic");
        }
    }
    if automatic.following() {
        let allowed = src.target_follows(target.id, viewer_id).await?;
        target_follows_viewer = Some(allowed);
        if allowed {
            return Ok("automatic");
        }
    }

    if manual.public() {
        return Ok("manual");
    }
    if manual.followers() {
        let allowed = match viewer_follows_target {
            Some(allowed) => allowed,
            None => src.viewer_follows(viewer_id, target.id).await?,
        };
        if allowed {
            return Ok("manual");
        }
    }
    if manual.following() {
        let allowed = match target_follows_viewer {
            Some(allowed) => allowed,
            None => src.target_follows(target.id, viewer_id).await?,
        };
        if allowed {
            return Ok("manual");
        }
    }

    if automatic.unsupported() || manual.unsupported() {
        return Ok("unknown");
    }
    Ok("denied")
}

async fn feature_approval_json(
    src: &AccountRenderCtx<'_>,
    account: &Account,
    viewer: Option<i64>,
) -> Result<Value, ApiError> {
    let policy = plamenu_ap::quote_policy::QuotePolicy::from_bitmap(feature_policy_bitmap(account));
    Ok(json!({
        "automatic": policy.automatic().keys(),
        "manual": policy.manual().keys(),
        "current_user": feature_policy_with(src, account, viewer).await?,
    }))
}

/// The human web URL of a status (`url`), distinct from its AP id (`uri`):
/// `/@username/{id}` for local statuses, the origin's published `url` for
/// remote ones (falling back to the AP id when it was never captured).
#[must_use]
pub fn status_web_url(domain: &str, item: &Status, author_username: &str) -> String {
    if let Some(url) = &item.url {
        return url.clone();
    }
    match &item.uri {
        Some(uri) => uri.clone(),
        None => LocalStatusUrls::new(domain, author_username, item.id).web_url,
    }
}

/// The Mastodon `Report` entity. Plamenu has no collections, so
/// `collection_ids` is always empty.
pub async fn report_json(pool: &PgPool, domain: &str, report: &Report) -> Result<Value, ApiError> {
    let target = account::find_by_id(pool, report.target_account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let action_taken_at = match report.action_taken_at {
        Some(at) => Value::String(rfc3339(at)?),
        None => Value::Null,
    };
    let rule_ids = match &report.rule_ids {
        Some(ids) => Value::Array(ids.iter().map(|id| json!(id.to_string())).collect()),
        None => Value::Null,
    };
    let status_ids: Vec<String> = report.status_ids.iter().map(i64::to_string).collect();
    Ok(json!({
        "id": report.id.to_string(),
        "action_taken": report.action_taken_at.is_some(),
        "action_taken_at": action_taken_at,
        "category": report.category,
        "comment": report.comment,
        "forwarded": report.forwarded.unwrap_or(false),
        "created_at": rfc3339(report.created_at)?,
        "status_ids": status_ids,
        "rule_ids": rule_ids,
        "collection_ids": [],
        "target_account": account_json(pool, domain, &target, None).await?,
    }))
}

/// The Mastodon `Rule` entity (`REST::RuleSerializer`): `id`, `text`, `hint`
/// and a `translations` map keyed by language (`{ text, hint }`). The
/// translations passed in are those belonging to this rule, sorted by language.
fn rule_json(rule: &Rule, translations: &[RuleTranslation]) -> Value {
    let translations: serde_json::Map<String, Value> = translations
        .iter()
        .map(|t| {
            (
                t.language.clone(),
                json!({ "text": t.text, "hint": t.hint }),
            )
        })
        .collect();
    json!({
        "id": rule.id.to_string(),
        "text": rule.text,
        "hint": rule.hint,
        "translations": translations,
    })
}

/// Serializes a set of rules with their translations into the `Rule` entity
/// array, loading the translations in one query and grouping them per rule.
pub async fn rules_json(pool: &PgPool, rules: &[Rule]) -> Result<Vec<Value>, ApiError> {
    let ids: Vec<i64> = rules.iter().map(|r| r.id).collect();
    let translations = rule::translations_for(pool, &ids).await?;
    let mut by_rule: HashMap<i64, Vec<RuleTranslation>> = HashMap::new();
    for t in translations {
        by_rule.entry(t.rule_id).or_default().push(t);
    }
    Ok(rules
        .iter()
        .map(|r| rule_json(r, by_rule.get(&r.id).map_or(&[][..], Vec::as_slice)))
        .collect())
}

/// The live instance rules in display order, as `Rule` entities — backs
/// `GET /api/v1/instance/rules` and the instance entity's `rules` field.
pub async fn instance_rules_json(pool: &PgPool) -> Result<Vec<Value>, ApiError> {
    let rules = rule::list_ordered(pool).await?;
    rules_json(pool, &rules).await
}

/// The minimal account shape Mastodon embeds in an announcement's `mentions`
/// (`AnnouncementSerializer::AccountSerializer`): id, username, url, acct.
#[must_use]
pub fn announcement_mention_json(domain: &str, account: &Account) -> Value {
    let acct = account_acct(domain, account);
    json!({
        "id": account.id.to_string(),
        "username": account.username,
        "url": account_web_url(domain, account),
        "acct": acct,
    })
}

/// One emoji's `Reaction` entity (`REST::ReactionSerializer`): `name`, `count`,
/// `me`, plus `url`/`static_url` for a custom emoji.
fn reaction_object(
    domain: &str,
    name: &str,
    custom: Option<&CustomEmoji>,
    count: i64,
    me: bool,
) -> Value {
    let mut obj = json!({ "name": name, "count": count, "me": me });
    if let Some(emoji) = custom {
        // Announcement reactions only reference local custom emoji, which are
        // always cached — the URL is a `/media/` copy either way.
        let rendered = crate::emoji::custom_emoji_json(domain, emoji, false);
        obj["url"] = rendered["url"].clone();
        obj["static_url"] = rendered["static_url"].clone();
    }
    obj
}

/// An announcement's `reactions` array, in first-reacted order, resolving the
/// local custom-emoji image for custom-emoji reactions.
pub async fn announcement_reactions_json(
    pool: &PgPool,
    domain: &str,
    groups: &[ReactionGroup],
) -> Result<Value, ApiError> {
    let custom_ids: Vec<i64> = groups.iter().filter_map(|g| g.custom_emoji_id).collect();
    let by_id: HashMap<i64, CustomEmoji> = custom_emoji::find_managed_by_ids(pool, &custom_ids)
        .await?
        .into_iter()
        .map(|emoji| (emoji.id, emoji.as_emoji()))
        .collect();
    Ok(announcement_reactions_values(domain, groups, &by_id))
}

/// [`announcement_reactions_json`] against a preloaded emoji set — the batched
/// listing looks every custom reaction name up once and selects per
/// announcement from the result.
fn announcement_reactions_values(
    domain: &str,
    groups: &[ReactionGroup],
    by_id: &HashMap<i64, CustomEmoji>,
) -> Value {
    let reactions: Vec<Value> = groups
        .iter()
        .map(|g| {
            let custom = g.custom_emoji_id.and_then(|id| by_id.get(&id));
            reaction_object(domain, &g.name, custom, g.count, g.me)
        })
        .collect();
    Value::Array(reactions)
}

/// One `announcement.reaction` streaming payload: the emoji's current tally
/// (`me` is always `false` in the broadcast, like Mastodon) tagged with its
/// `announcement_id`.
pub async fn announcement_reaction_json(
    pool: &PgPool,
    domain: &str,
    announcement_id: i64,
    name: &str,
) -> Result<Value, ApiError> {
    let count = announcement::reaction_count(pool, announcement_id, name).await?;
    let by_shortcode = lookup_local_emoji(pool, &[name.to_owned()]).await?;
    let mut obj = reaction_object(domain, name, by_shortcode.get(name), count, false);
    obj["announcement_id"] = json!(announcement_id.to_string());
    Ok(obj)
}

/// The Mastodon `Announcement` entity. `content` is the linkified text; the
/// `mentions`/`statuses`/`tags`/`emojis` are re-derived from the text (no
/// network, like `Account.from_text`); `reactions` come pre-grouped; `read` is
/// whether the viewer has dismissed it.
pub async fn announcement_json(
    state: &AppState,
    ann: &announcement::Announcement,
    viewer: Option<i64>,
    groups: &[ReactionGroup],
    read: bool,
) -> Result<Value, ApiError> {
    let mut reactions = HashMap::new();
    reactions.insert(ann.id, groups.to_vec());
    let read_ids: HashSet<i64> = if read {
        HashSet::from([ann.id])
    } else {
        HashSet::new()
    };
    let mut values = announcement_json_many(
        state,
        std::slice::from_ref(ann),
        viewer,
        &reactions,
        &read_ids,
    )
    .await?;
    Ok(values.pop().expect("one announcement in, one entity out"))
}

/// The batched [`announcement_json`]: one cited-status render, one text-emoji
/// lookup and one reaction-emoji lookup serve the whole listing, however many
/// announcements are published. A one-viewer set of
/// [`announcement_json_for_viewer_set`], so there is no second implementation
/// to drift.
pub(crate) async fn announcement_json_many(
    state: &AppState,
    announcements: &[announcement::Announcement],
    viewer: Option<i64>,
    reactions: &HashMap<i64, Vec<ReactionGroup>>,
    read: &HashSet<i64>,
) -> Result<Vec<Value>, ApiError> {
    let reactions_by_viewer: HashMap<(Option<i64>, i64), Vec<ReactionGroup>> = reactions
        .iter()
        .map(|(ann_id, groups)| ((viewer, *ann_id), groups.clone()))
        .collect();
    let read_by_viewer: HashSet<(Option<i64>, i64)> =
        read.iter().map(|ann_id| (viewer, *ann_id)).collect();
    let mut per_viewer = announcement_json_for_viewer_set(
        state,
        announcements,
        std::slice::from_ref(&viewer),
        &reactions_by_viewer,
        &read_by_viewer,
    )
    .await?;
    per_viewer.remove(&viewer).ok_or(ApiError::NotFound)
}

/// [`announcement_json_many`] across a set of viewers (N+1 decisions): the
/// cited-status render (through the viewer-set status renderer), the
/// text-emoji lookup, the reaction-emoji lookup and each announcement's
/// `compose_local` all run once for the whole set. Only the `read` flag, the
/// reaction groups' `me` bit and the cited statuses' viewer dimensions differ
/// per viewer — and they arrive prefetched, keyed by `(viewer, announcement)`.
pub(crate) async fn announcement_json_for_viewer_set(
    state: &AppState,
    announcements: &[announcement::Announcement],
    set: &[Option<i64>],
    reactions: &HashMap<(Option<i64>, i64), Vec<ReactionGroup>>,
    read: &HashSet<(Option<i64>, i64)>,
) -> Result<HashMap<Option<i64>, Vec<Value>>, ApiError> {
    let mut set: Vec<Option<i64>> = set.to_vec();
    set.sort_unstable();
    set.dedup();
    if announcements.is_empty() || set.is_empty() {
        return Ok(set.into_iter().map(|viewer| (viewer, Vec::new())).collect());
    }
    let domain = &state.config.domain;

    // Cited statuses, only those still publicly distributable (Mastodon's
    // `Status.distributable_visibility`), rendered once across announcements
    // — and once across viewers, through the viewer-set renderer.
    let mut all_status_ids: Vec<i64> = announcements
        .iter()
        .filter_map(|ann| ann.status_ids.as_ref())
        .flatten()
        .copied()
        .collect();
    all_status_ids.sort_unstable();
    all_status_ids.dedup();
    let status_by_id_of: HashMap<Option<i64>, HashMap<i64, Value>> = if all_status_ids.is_empty() {
        set.iter().map(|viewer| (*viewer, HashMap::new())).collect()
    } else {
        let mut cited = status::find_by_ids(&state.pool, &all_status_ids).await?;
        cited.retain(|s| matches!(s.visibility.as_str(), "public" | "unlisted"));
        render_statuses_for_viewer_set(&state.pool, domain, &cited, &set, 0)
            .await?
            .into_iter()
            .map(|(viewer, values)| {
                let by_id = values
                    .into_iter()
                    .filter_map(|value| {
                        value["id"]
                            .as_str()
                            .and_then(|id| id.parse::<i64>().ok())
                            .map(|id| (id, value))
                    })
                    .collect();
                (viewer, by_id)
            })
            .collect()
    };

    // Announcement emoji are local (always cached), so no fallback marker;
    // one lookup covers every text.
    let texts: Vec<&str> = announcements.iter().map(|ann| ann.text.as_str()).collect();
    let codes = crate::emoji::shortcodes_of(&texts);
    let mut requests: HashMap<(Option<&str>, Option<i64>), Vec<String>> = HashMap::new();
    if !codes.is_empty() {
        requests.insert((None, None), codes);
    }
    let emoji_table = load_emoji_table(&state.pool, &requests).await?;
    let local_emoji = emoji_table.get(&(None, None));

    // One lookup covers every custom reaction row on the page, across every
    // viewer's groups (they only differ in `me`, but a hostile map cannot
    // sneak an unresolved name past the batch this way).
    let mut custom_ids: Vec<i64> = reactions
        .values()
        .flatten()
        .filter_map(|group| group.custom_emoji_id)
        .collect();
    custom_ids.sort_unstable();
    custom_ids.dedup();
    let reaction_emoji: HashMap<i64, CustomEmoji> =
        custom_emoji::find_managed_by_ids(&state.pool, &custom_ids)
            .await?
            .into_iter()
            .map(|emoji| (emoji.id, emoji.as_emoji()))
            .collect();

    // The composed text is viewer-independent: one `compose_local` per
    // announcement serves the whole set.
    let mut composed_per_ann = Vec::with_capacity(announcements.len());
    for ann in announcements {
        composed_per_ann.push(compose_local(state, &ann.text).await?);
    }

    let empty = HashMap::new();
    let mut out: HashMap<Option<i64>, Vec<Value>> = HashMap::with_capacity(set.len());
    for viewer in &set {
        let status_by_id = status_by_id_of.get(viewer).unwrap_or(&empty);
        let mut rendered = Vec::with_capacity(announcements.len());
        for (ann, composed) in announcements.iter().zip(&composed_per_ann) {
            let groups = reactions
                .get(&(*viewer, ann.id))
                .map_or(&[][..], Vec::as_slice);
            rendered.push(assemble_announcement(
                domain,
                ann,
                composed,
                status_by_id,
                local_emoji,
                &reaction_emoji,
                groups,
                read.contains(&(*viewer, ann.id)),
            )?);
        }
        out.insert(*viewer, rendered);
    }
    Ok(out)
}

/// One announcement's rendered entity for one viewer — pure assembly from the
/// shared composed text/emoji context and the viewer's prefetched cited
/// renders, reaction groups and read flag.
#[allow(clippy::too_many_arguments)] // pure assembly over the prefetched context
fn assemble_announcement(
    domain: &str,
    ann: &announcement::Announcement,
    composed: &crate::compose::Composed,
    status_by_id: &HashMap<i64, Value>,
    local_emoji: Option<&HashMap<String, CustomEmoji>>,
    reaction_emoji: &HashMap<i64, CustomEmoji>,
    groups: &[ReactionGroup],
    read: bool,
) -> Result<Value, ApiError> {
    let mentions: Vec<Value> = composed
        .mentions
        .iter()
        .map(|account| announcement_mention_json(domain, account))
        .collect();
    let tags: Vec<Value> = composed
        .hashtags
        .iter()
        .map(|name| shallow_tag_json(domain, name))
        .collect();
    let statuses: Vec<Value> = match &ann.status_ids {
        Some(ids) => ids
            .iter()
            .filter_map(|id| status_by_id.get(id).cloned())
            .collect(),
        None => Vec::new(),
    };
    let emojis = emojis_from_table(domain, local_emoji, &[ann.text.as_str()], false);
    let reaction_values = announcement_reactions_values(domain, groups, reaction_emoji);
    Ok(json!({
        "id": ann.id.to_string(),
        "content": composed.html,
        "starts_at": optional_rfc3339(ann.starts_at)?,
        "ends_at": optional_rfc3339(ann.ends_at)?,
        "all_day": ann.all_day,
        "published_at": optional_rfc3339(ann.published_at)?,
        "updated_at": rfc3339(ann.updated_at)?,
        "read": read,
        "mentions": mentions,
        "statuses": statuses,
        "tags": tags,
        "emojis": emojis,
        "reactions": reaction_values,
    }))
}

/// Renders `MediaAttachment` entities for a preview from already-uploaded
/// (unattached) media rows, in the given id order. Mirrors the per-attachment
/// rendering in [`media_maps`] but keyed on ids rather than a status.
pub(crate) async fn preview_media_json(
    pool: &PgPool,
    domain: &str,
    media_ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Value>, ApiError> {
    if media_ids.is_empty() {
        return Ok(Vec::new());
    }
    let allow_direct = allow_direct_media(pool, viewer).await;
    let mut media = plamenu_db::media::find_by_ids(pool, media_ids).await?;
    // Never render another account's media: a preview only shows uploads the
    // viewer owns (the composer always passes its own; this guards the API).
    if let Some(viewer) = viewer {
        media.retain(|item| item.account_id == viewer);
    }
    // `find_by_ids` returns rows in an arbitrary order; restore the order the
    // composer submitted so the preview matches the eventual post.
    media.sort_by_key(|item| {
        media_ids
            .iter()
            .position(|id| *id == item.id)
            .unwrap_or(usize::MAX)
    });
    let ladders = plamenu_db::media::renditions_for(pool, media_ids).await?;
    Ok(media
        .iter()
        .map(|item| {
            media_json_hls(
                domain,
                item,
                allow_direct,
                ladders.get(&item.id).map(Vec::as_slice),
            )
        })
        .collect())
}

/// The non-text draft attributes a status [preview](preview_status_json)
/// echoes back — everything the rendered `content` doesn't already carry.
pub struct PreviewStatusOpts<'a> {
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub visibility: &'a str,
    pub language: Option<&'a str>,
    pub in_reply_to_id: Option<i64>,
    pub quote_approval_policy: i32,
    /// Already-uploaded (unattached) media to render into `media_attachments`,
    /// in display order. Empty for a text-only preview.
    pub media_ids: &'a [i64],
    /// A group submission's thread title and link target, folded into
    /// `content` so the preview card matches the posted status. `None` for a
    /// plain post.
    pub title: Option<&'a str>,
    pub external_url: Option<&'a str>,
    /// The kind the draft would be posted as, so the preview reports the same
    /// `object_type` the stored status will. Previously re-derived from "does it
    /// have a title" — which cannot tell a `Page` from an `Article`.
    pub kind: plamenu_ap::activity::PostKind,
}

/// A `Status` entity for a draft that was rendered but never persisted
/// `POST /api/v1/statuses/preview`). The text-derived fields — `content`,
/// `mentions`, `tags`, `emojis` — render exactly as the stored post would (the
/// point of the endpoint is to kill client/server Markdown drift), assembled
/// from [`Composed`] the same way [`announcement_json`] builds its shape. The
/// identity/engagement fields have no real values, so they carry Mastodon's
/// unsaved-record stand-ins (`id` = `"0"`, null `uri`/`url`, zero counts,
/// false flags). `media_attachments` are rendered from any `opts.media_ids`
/// (the web composer preview shows attachments; an API client that already
/// holds the entities can pass none); `poll` stays null — poll options are
/// plain text and never Markdown-rendered, so there is nothing to preview.
pub async fn preview_status_json(
    state: &AppState,
    account: &Account,
    composed: &Composed,
    opts: PreviewStatusOpts<'_>,
) -> Result<Value, ApiError> {
    let domain = &state.config.domain;
    let author = account_json(&state.pool, domain, account, Some(account.id)).await?;
    let media = preview_media_json(&state.pool, domain, opts.media_ids, Some(account.id)).await?;
    let mentions: Vec<Value> = composed
        .mentions
        .iter()
        .map(|account| announcement_mention_json(domain, account))
        .collect();
    let tags: Vec<Value> = composed
        .hashtags
        .iter()
        .map(|name| shallow_tag_json(domain, name))
        .collect();
    // Custom emoji are re-derived from the rendered text, exactly like a stored
    // status' `emoji_map`. The author is local, so no direct-URL fallback.
    let emojis = crate::emoji::emojis_json_for_account(
        &state.pool,
        domain,
        account.id,
        &[opts.spoiler_text, composed.html.as_str()],
        false,
    )
    .await?;
    let now = rfc3339(OffsetDateTime::now_utc())?;
    // Mirrors `quote_current_user` for the author viewing their own draft:
    // a direct/local post can never be quoted, otherwise the author always may.
    let policy = plamenu_ap::quote_policy::QuotePolicy::from_bitmap(opts.quote_approval_policy);
    let current_user = if matches!(opts.visibility, "direct" | "local") {
        "denied"
    } else {
        "automatic"
    };
    // Built flat then extended with the nested/computed keys via indexing, the
    // way `render_plain` does — an inline `json!` with the nested objects blows
    // the macro recursion limit.
    let mut entity = json!({
        "id": "0",
        "created_at": now,
        "edited_at": null,
        "in_reply_to_id": opts.in_reply_to_id.map(|id| id.to_string()),
        "in_reply_to_account_id": null,
        "sensitive": opts.sensitive,
        "spoiler_text": opts.spoiler_text,
        "visibility": opts.visibility,
        "language": opts.language,
        "uri": null,
        "url": null,
        "replies_count": 0,
        "reblogs_count": 0,
        "favourites_count": 0,
        "quotes_count": 0,
        "favourited": false,
        "reblogged": false,
        "muted": false,
        "bookmarked": false,
        "pinned": false,
        "content": fold_typed_content(&composed.html, opts.title, opts.external_url),
        "reblog": null,
        "application": null,
        "account": author,
        "media_attachments": media,
        "mentions": mentions,
        "tags": tags,
        "tagged_collections": [],
        "emojis": emojis,
        "card": null,
        "poll": null,
        "quote": null,
        // Extension fields ride on every Status shape (null/false when
        // absent) so clients can feature-detect on shape; a preview carries a
        // group submission's title/link but no typed object, group attribution
        // or votes.
        "title": opts.title,
        "object_type": match opts.kind {
            plamenu_ap::activity::PostKind::Note | plamenu_ap::activity::PostKind::Question => None,
            typed => Some(typed.as_str()),
        },
        "external_url": opts.external_url,
        "event": null,
        "group_post": false,
        "downvotes_count": 0,
        "downvoted": false,
        "group_locked": false,
        // `filtered` rides on every status for an authenticated viewer; the
        // author is always the viewer of their own preview.
        "filtered": [],
    });
    entity["groups"] = json!([]);
    entity["quote_approval"] = json!({
        "automatic": policy.automatic().keys(),
        "manual": policy.manual().keys(),
        "current_user": current_user,
    });
    // Pleroma emoji-reaction mirrors, always present (empty for a fresh draft).
    entity["emoji_reactions"] = json!([]);
    entity["pleroma"] = json!({ "emoji_reactions": [] });
    Ok(entity)
}

fn optional_rfc3339(at: Option<time::OffsetDateTime>) -> Result<Value, ApiError> {
    match at {
        Some(at) => Ok(Value::String(rfc3339(at)?)),
        None => Ok(Value::Null),
    }
}

/// Local custom emoji keyed by shortcode, for the given names.
async fn lookup_local_emoji(
    pool: &PgPool,
    names: &[String],
) -> Result<HashMap<String, CustomEmoji>, ApiError> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let found = custom_emoji::lookup(pool, names, None).await?;
    Ok(found
        .into_iter()
        .map(|emoji| (emoji.shortcode.clone(), emoji))
        .collect())
}

/// The Mastodon `Admin::Report` entity (`REST::Admin::ReportSerializer`): the
/// report plus the four `Admin::Account`-serialized parties, the cited statuses
/// and the cited `rules` (resolved from `rule_ids`, including discarded rules).
pub async fn admin_report_json(
    pool: &PgPool,
    domain: &str,
    report: &Report,
) -> Result<Value, ApiError> {
    // The reporter/target always exist; the assigned/acting moderators are
    // optional and serialize as `null` when unset.
    let account = admin_account_for(pool, domain, Some(report.account_id)).await?;
    let target_account = admin_account_for(pool, domain, Some(report.target_account_id)).await?;
    let assigned_account = admin_account_for(pool, domain, report.assigned_account_id).await?;
    let action_taken_by_account =
        admin_account_for(pool, domain, report.action_taken_by_account_id).await?;

    let statuses = status::find_by_ids(pool, &report.status_ids).await?;
    let statuses = render_statuses(pool, domain, &statuses, None).await?;

    // `rule_ids` cites violation rules; resolve them (Mastodon includes
    // discarded rules here so historical reports still render).
    let rules = match &report.rule_ids {
        Some(ids) if !ids.is_empty() => {
            let cited = rule::find_by_ids(pool, ids).await?;
            rules_json(pool, &cited).await?
        }
        _ => Vec::new(),
    };

    let action_taken_at = match report.action_taken_at {
        Some(at) => Value::String(rfc3339(at)?),
        None => Value::Null,
    };
    Ok(json!({
        "id": report.id.to_string(),
        "action_taken": report.action_taken_at.is_some(),
        "action_taken_at": action_taken_at,
        "category": report.category,
        "comment": report.comment,
        "forwarded": report.forwarded.unwrap_or(false),
        "created_at": rfc3339(report.created_at)?,
        "updated_at": rfc3339(report.updated_at)?,
        "account": account,
        "target_account": target_account,
        "assigned_account": assigned_account,
        "action_taken_by_account": action_taken_by_account,
        "statuses": statuses,
        "rules": rules,
    }))
}

/// The Mastodon `Admin::DomainBlock` entity.
pub fn admin_domain_block_json(block: &DomainBlock) -> Result<Value, ApiError> {
    Ok(json!({
        "id": block.id.to_string(),
        "domain": block.domain,
        "digest": sha256_hex(&block.domain),
        "created_at": rfc3339(block.created_at)?,
        "severity": block.severity,
        "reject_media": block.reject_media,
        "reject_reports": block.reject_reports,
        "private_comment": block.private_comment,
        "public_comment": block.public_comment,
        "obfuscate": block.obfuscate,
    }))
}

/// Mastodon's existing-domain-block 422 payload.
pub fn existing_domain_block_error_json(block: &DomainBlock) -> Result<Value, ApiError> {
    Ok(json!({
        "error": format!("A block for {} already exists", block.domain),
        "existing_domain_block": admin_domain_block_json(block)?,
    }))
}

/// The Mastodon `Admin::DomainAllow` entity.
pub fn admin_domain_allow_json(allow: &DomainAllow) -> Result<Value, ApiError> {
    Ok(json!({
        "id": allow.id.to_string(),
        "domain": allow.domain,
        "created_at": rfc3339(allow.created_at)?,
    }))
}

/// The Mastodon `Admin::EmailDomainBlock` entity. Plamenu does not track
/// per-block sign-up history yet, so `history` is the empty series.
pub fn admin_email_domain_block_json(block: &EmailDomainBlock) -> Result<Value, ApiError> {
    Ok(json!({
        "id": block.id.to_string(),
        "domain": block.domain,
        "created_at": rfc3339(block.created_at)?,
        "history": [],
        "allow_with_approval": block.allow_with_approval,
    }))
}

/// The Mastodon `Admin::IpBlock` entity.
pub fn admin_ip_block_json(block: &IpBlock) -> Result<Value, ApiError> {
    let expires_at = match block.expires_at {
        Some(at) => Value::String(rfc3339(at)?),
        None => Value::Null,
    };
    Ok(json!({
        "id": block.id.to_string(),
        "ip": block.ip,
        "severity": block.severity,
        "comment": block.comment,
        "created_at": rfc3339(block.created_at)?,
        "expires_at": expires_at,
    }))
}

/// The Mastodon `Admin::CanonicalEmailBlock` entity.
#[must_use]
pub fn admin_canonical_email_block_json(block: &CanonicalEmailBlock) -> Value {
    json!({
        "id": block.id.to_string(),
        "canonical_email_hash": block.canonical_email_hash,
    })
}

/// Renders an optional account id through the `Admin::Account` serializer,
/// yielding `null` when the id is absent or no longer resolves.
async fn admin_account_for(
    pool: &PgPool,
    domain: &str,
    account_id: Option<i64>,
) -> Result<Value, ApiError> {
    let Some(id) = account_id else {
        return Ok(Value::Null);
    };
    match admin_account::show(pool, id).await? {
        Some(view) => admin_account_json(pool, domain, &view).await,
        None => Ok(Value::Null),
    }
}

/// A client-facing media-proxy URL: `/media/proxy/{kind}/{id}` (`/small` for
/// the preview variant). `allow_direct` bakes in the viewer's preference so the
/// proxy may fall back to the origin when it cannot cache the file — the only
/// path by which a client ever reaches off-instance. Never emit an origin URL
/// to a client directly; route everything through here.
#[must_use]
pub fn media_proxy_url(
    domain: &str,
    kind: &str,
    id: i64,
    small: bool,
    allow_direct: bool,
) -> String {
    let variant = if small { "/small" } else { "" };
    let marker = if allow_direct { "?d=1" } else { "" };
    format!("https://{domain}/media/proxy/{kind}/{id}{variant}{marker}")
}

/// Resolves the viewer's direct-remote-media preference once per render, to bake
/// into every proxy URL. Anonymous or remote viewers never get the fallback
/// marker (fail closed).
pub async fn allow_direct_media(pool: &PgPool, viewer: Option<i64>) -> bool {
    match viewer {
        Some(v) => user::allows_direct_remote_media(pool, v)
            .await
            .unwrap_or(false),
        None => false,
    }
}

/// The public URL of an account's avatar: our cached `/media/` copy, or the
/// media proxy (never the origin) while it is still un-downloaded. `None` when
/// the account has no avatar at all.
#[must_use]
pub fn avatar_url(domain: &str, account: &Account, allow_direct: bool) -> Option<String> {
    profile_image_url(
        domain,
        "avatar",
        account.id,
        account.avatar_remote_url.as_deref(),
        account.avatar_file_name.as_deref(),
        allow_direct,
    )
}

/// The public URL of an account's header. `None` when unset.
#[must_use]
pub fn header_url(domain: &str, account: &Account, allow_direct: bool) -> Option<String> {
    profile_image_url(
        domain,
        "header",
        account.id,
        account.header_remote_url.as_deref(),
        account.header_file_name.as_deref(),
        allow_direct,
    )
}

fn profile_image_url(
    domain: &str,
    kind: &str,
    account_id: i64,
    remote_url: Option<&str>,
    file_name: Option<&str>,
    allow_direct: bool,
) -> Option<String> {
    // Our cached copy wins. Until it downloads, a remote actor's image routes
    // through the proxy (never the origin URL). A local account with no file
    // simply has no image.
    if let Some(f) = file_name {
        return Some(format!("https://{domain}/media/{f}"));
    }
    remote_url.map(|_| media_proxy_url(domain, kind, account_id, false, allow_direct))
}

/// A stored profile field value as display HTML: local values are raw text
/// (whole-URL values become links, like Mastodon), remote values are
/// already-sanitized HTML.
#[must_use]
pub fn field_value_html(account: &Account, value: &str) -> String {
    if !account.is_local() {
        return value.to_owned();
    }
    let trimmed = value.trim();
    if trimmed.starts_with("https://") && !trimmed.contains(char::is_whitespace) {
        let escaped = plamenu_ap::text::escape_html(trimmed);
        format!(
            r#"<a href="{escaped}" rel="nofollow noopener noreferrer me" target="_blank">{escaped}</a>"#
        )
    } else {
        plamenu_ap::text::escape_html(trimmed)
    }
}

/// The `fields` array of an Account entity, values rendered to HTML.
#[must_use]
pub fn fields_json(account: &Account) -> Vec<Value> {
    let Some(stored) = account.fields.as_array() else {
        return Vec::new();
    };
    stored
        .iter()
        .map(|field| {
            let name = field.get("name").and_then(Value::as_str).unwrap_or("");
            let value = field.get("value").and_then(Value::as_str).unwrap_or("");
            // `verified_at` is stamped by the rel="me" verification worker
            // absent until a field's URL links back to this profile.
            let verified_at = field.get("verified_at").and_then(Value::as_str);
            json!({
                "name": name,
                "value": field_value_html(account, value),
                "verified_at": verified_at,
            })
        })
        .collect()
}

/// The text an account's custom emoji are scanned out of — the bio, the
/// display name and the profile fields, Mastodon's `emojifiable_text`.
#[must_use]
pub fn account_emojifiable_text(account: &Account) -> String {
    let mut text = format!("{} {}", account.note, account.display_name);
    if let Some(stored) = account.fields.as_array() {
        for field in stored {
            for key in ["name", "value"] {
                if let Some(part) = field.get(key).and_then(Value::as_str) {
                    text.push(' ');
                    text.push_str(part);
                }
            }
        }
    }
    text
}

/// The follower/following/status counters an `Account` entity carries —
/// split out so a batch of accounts can be counted in a handful of queries
/// (see [`account_counts_batch`]) instead of one round trip per account.
#[derive(Default, Clone, Copy)]
pub struct AccountCounts {
    pub followers: u64,
    pub following: u64,
    pub statuses: u64,
    pub last_status_at: Option<OffsetDateTime>,
}

/// [`AccountCounts`] for every id in `account_ids`, in four batched queries
/// total rather than four per account. Ids absent from an underlying batch
/// query (no followers/following/statuses) count as the zero/`None` default.
pub async fn account_counts_batch(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<HashMap<i64, AccountCounts>, ApiError> {
    let followers = follow::count_followers_batch(pool, account_ids).await?;
    let following = follow::count_following_batch(pool, account_ids).await?;
    // Remote accounts whose outbox `totalItems` has been synced report the
    // origin's authoritative total instead of the locally-known slice
    // (Mastodon's `statuses_count`); the fallback logic lives inside the
    // batch query.
    let statuses = status::count_by_account_batch(pool, account_ids).await?;
    let last_status_at = status::last_created_at_batch(pool, account_ids).await?;
    Ok(account_ids
        .iter()
        .map(|&id| {
            let counts = AccountCounts {
                followers: followers.get(&id).copied().unwrap_or(0),
                following: following.get(&id).copied().unwrap_or(0),
                statuses: statuses.get(&id).copied().unwrap_or(0),
                last_status_at: last_status_at.get(&id).copied(),
            };
            (id, counts)
        })
        .collect())
}

/// The Mastodon `Account` entity.
pub async fn account_json(
    pool: &PgPool,
    domain: &str,
    account: &Account,
    viewer: Option<i64>,
) -> Result<Value, ApiError> {
    let counts = account_counts_batch(pool, &[account.id]).await?;
    let counts = counts.get(&account.id).copied().unwrap_or_default();
    let allow_direct = allow_direct_media(pool, viewer).await;
    account_json_with_counts(
        pool,
        domain,
        account,
        viewer,
        &counts,
        allow_direct,
        &AccountRenderCtx::Live(pool),
    )
    .await
}

/// Renders a batch of `Account` entities in a fixed number of queries
/// regardless of page size: the follower/following/status counters, the
/// viewer's direct-media preference and the viewer's accepted-follow edges (for
/// `feature_approval`) are all prefetched once, so each row is assembled
/// without its own round trips. This is the batched counterpart of
/// [`account_json`] — route every account *list* endpoint through it.
pub async fn render_accounts(
    pool: &PgPool,
    domain: &str,
    accounts: &[Account],
    viewer: Option<i64>,
) -> Result<Vec<Value>, ApiError> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let moved = moved_targets_map(pool, domain, &movers_of(accounts.iter()), viewer).await?;
    render_accounts_inner(pool, domain, accounts, viewer, &moved).await
}

/// The `(account id, moved_to_uri)` pairs of a batch — the input
/// [`moved_targets_map`] resolves and pre-renders.
fn movers_of<'a>(accounts: impl Iterator<Item = &'a Account>) -> Vec<(i64, String)> {
    accounts
        .filter_map(|account| account.moved_to_uri.clone().map(|uri| (account.id, uri)))
        .collect()
}

/// Profile custom emoji for a batch of accounts, resolved in one query for
/// the whole set — the `emojis` map an [`AccountRenderCtx::Batched`] answers
/// from. Accounts whose profile text carries no `:shortcode:` never
/// contribute to the lookup, so an emoji-free page costs zero queries here.
async fn profile_emoji_map<'a>(
    pool: &PgPool,
    domain: &str,
    accounts: impl Iterator<Item = &'a Account>,
    allow_direct: bool,
) -> Result<HashMap<i64, Vec<Value>>, ApiError> {
    let texts: Vec<(i64, Option<String>, String)> = accounts
        .map(|a| (a.id, a.domain.clone(), account_emojifiable_text(a)))
        .collect();
    let mut shortcodes_by_domain: HashMap<(Option<&str>, Option<i64>), Vec<String>> =
        HashMap::new();
    for (account_id, domain_key, text) in &texts {
        for code in plamenu_ap::emoji::scan_shortcodes(text) {
            let codes = shortcodes_by_domain
                .entry((
                    domain_key.as_deref(),
                    domain_key.is_none().then_some(*account_id),
                ))
                .or_default();
            if !codes.iter().any(|c| c == code) {
                codes.push(code.to_owned());
            }
        }
    }
    let table = load_emoji_table(pool, &shortcodes_by_domain).await?;
    let mut out = HashMap::with_capacity(texts.len());
    for (id, domain_key, text) in &texts {
        let rendered = emojis_from_table(
            domain,
            table.get(&(domain_key.clone(), domain_key.is_none().then_some(*id))),
            &[text.as_str()],
            allow_direct,
        );
        if !rendered.is_empty() {
            out.insert(*id, rendered);
        }
    }
    Ok(out)
}

/// [`render_accounts`]' body, parameterized on the pre-rendered migration
/// targets — the nested render of those targets themselves passes an empty
/// map, so it cannot recurse.
async fn render_accounts_inner(
    pool: &PgPool,
    domain: &str,
    accounts: &[Account],
    viewer: Option<i64>,
    moved: &HashMap<i64, Value>,
) -> Result<Vec<Value>, ApiError> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = accounts.iter().map(|a| a.id).collect();
    let counts = account_counts_batch(pool, &ids).await?;
    let allow_direct = allow_direct_media(pool, viewer).await;
    let relations = FeatureRelations::load(pool, viewer, &ids).await?;
    let overlays = user::account_entity_overlay_batch(pool, &ids).await?;
    let silenced_domains = silenced_domain_set(pool).await?;
    let emojis = profile_emoji_map(pool, domain, accounts.iter(), allow_direct).await?;
    let ctx = AccountRenderCtx::Batched {
        relations: &relations,
        overlays: &overlays,
        silenced_domains: &silenced_domains,
        moved,
        emojis: &emojis,
    };
    let mut rendered = Vec::with_capacity(accounts.len());
    for account in accounts {
        let counts = counts.get(&account.id).copied().unwrap_or_default();
        rendered.push(
            account_json_with_counts(pool, domain, account, viewer, &counts, allow_direct, &ctx)
                .await?,
        );
    }
    Ok(rendered)
}

/// [`render_accounts`] over a list of account ids: fetches the rows in one
/// query, restores the requested order and silently drops ids with no row
/// (matching the `if let Some(_) = find_by_id` skip the per-row callers used).
pub async fn render_accounts_by_ids(
    pool: &PgPool,
    domain: &str,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Value>, ApiError> {
    let mut accounts = account::find_by_ids(pool, ids).await?;
    accounts.sort_by_key(|a| ids.iter().position(|id| *id == a.id).unwrap_or(usize::MAX));
    render_accounts(pool, domain, &accounts, viewer).await
}

/// The set of domains under a `silence` block, prefetched once for the batched
/// account renderer's `limited` flag. Usually empty or tiny (one row per
/// admin-silenced server).
async fn silenced_domain_set(pool: &PgPool) -> Result<HashSet<String>, ApiError> {
    Ok(plamenu_db::instance_policy::silenced_domains(pool)
        .await?
        .into_iter()
        .collect())
}

/// [`account_json`]'s body, parameterized on pre-fetched [`AccountCounts`] and
/// an [`AccountRenderCtx`] so a caller rendering many accounts at once (like the
/// status-list renderer or [`render_accounts`]) can batch the counters and
/// accepted-follow edges up front instead of querying them per account.
#[allow(
    clippy::too_many_lines,
    reason = "the Mastodon account entity is assembled in one place so unavailable fields cannot drift"
)]
async fn account_json_with_counts(
    pool: &PgPool,
    domain: &str,
    account: &Account,
    viewer: Option<i64>,
    counts: &AccountCounts,
    allow_direct: bool,
    follow_src: &AccountRenderCtx<'_>,
) -> Result<Value, ApiError> {
    let AccountCounts {
        followers,
        following,
        statuses,
        last_status_at,
    } = *counts;
    let acct = account_acct(domain, account);
    // `uri` is the ActivityPub id; `url` is the human web page (`/@name`).
    let uri = account_uri(domain, account);
    let url = account_web_url(domain, account);
    let unavailable = account.suspended();
    // Clients fall back to their own placeholder when avatar/header 404.
    let missing_image = format!("https://{domain}/static/missing.png");
    let avatar = if unavailable {
        missing_image.clone()
    } else {
        avatar_url(domain, account, allow_direct).unwrap_or_else(|| missing_image.clone())
    };
    let header = if unavailable {
        missing_image.clone()
    } else {
        header_url(domain, account, allow_direct).unwrap_or_else(|| missing_image.clone())
    };
    let emojis = if unavailable {
        Vec::new()
    } else {
        follow_src
            .profile_emojis(pool, domain, account, allow_direct)
            .await?
    };
    let mut entity = json!({
        "id": account.id.to_string(),
        "username": account.username,
        "acct": acct,
        "display_name": if unavailable { "" } else { &account.display_name },
        "locked": if unavailable { false } else { account.locked },
        "bot": if unavailable { false } else { account.is_bot },
        "discoverable": if unavailable { Some(false) } else { account.discoverable },
        "indexable": if unavailable { false } else { account.indexable },
        "hide_collections": account.hide_collections,
        "feature_approval": feature_approval_json(follow_src, account, viewer).await?,
        "group": account.is_group(),
        "created_at": rfc3339(account.created_at)?,
        "note": if unavailable { "" } else { &account.note },
        "url": url,
        "uri": uri,
        "avatar": avatar,
        "avatar_static": avatar,
        "avatar_description": if unavailable { "" } else { &account.avatar_description },
        "header": header,
        "header_static": header,
        "header_description": if unavailable { "" } else { &account.header_description },
        "followers_count": followers,
        "following_count": following,
        "statuses_count": statuses,
        "last_status_at": bare_date(last_status_at),
        "show_media": account.show_media,
        "show_media_replies": account.show_media_replies,
        "show_featured": account.show_featured,
        "emojis": emojis,
        "fields": if unavailable {
            Vec::new()
        } else {
            fields_json(account)
        },
    });
    // Mastodon's `AccountSerializer` emits `moved` only when the account has
    // actually migrated (`if: :moved_and_not_nested?`) — the key is absent
    // otherwise. A `moved: null` we used to always send is a shape divergence
    // 3rd-party clients notice.
    if !unavailable && let Some(moved) = follow_src.moved(pool, domain, account, viewer).await? {
        entity["moved"] = moved;
    }
    // Local accounts carry the owner's `noindex` preference and their role
    // when it is publicly highlighted (Mastodon's `if: :local?` attributes);
    // a local account with no user row serializes `noindex` as null and
    // `roles` empty, like Mastodon's nil-tolerant delegates.
    if account.is_local() {
        let overlay = follow_src.overlay(pool, account.id).await?;
        entity["noindex"] = overlay.as_ref().map_or(Value::Null, |o| o.noindex.into());
        entity["roles"] = if unavailable {
            json!([])
        } else {
            overlay.and_then(|o| o.highlighted_role).map_or_else(
                || json!([]),
                |role| {
                    json!([{
                        "id": role.id.to_string(),
                        "name": role.name,
                        "color": role.color,
                    }])
                },
            )
        };
    }
    // Mastodon's `AccountSerializer` only emits these when set: `suspended`
    // when the account is suspended, `limited` when it is silenced,
    // `memorial` when the account is memorialized.
    if account.suspended() {
        entity["suspended"] = Value::Bool(true);
    }
    if account.silenced()
        || follow_src
            .domain_silenced(if account.is_portable_on(domain) {
                None
            } else {
                account.domain.as_deref()
            })
            .await?
    {
        entity["limited"] = Value::Bool(true);
    }
    if account.memorial {
        entity["memorial"] = Value::Bool(true);
    }
    Ok(entity)
}

/// The `moved` attachment of an account entity: `Some(target account)` when
/// this one migrated (`movedTo`), else `None` (the caller omits the key). The
/// target is rendered under [`AccountRenderCtx::Nested`], so it never computes a
/// `moved` of its own — a chained or cyclic redirect cannot recurse (the old
/// shape stripped the nested key only after rendering it, so a `movedTo`
/// cycle recursed without bound).
async fn moved_account_json(
    pool: &PgPool,
    domain: &str,
    account: &Account,
    viewer: Option<i64>,
) -> Result<Option<Value>, ApiError> {
    let Some(uri) = account.moved_to_uri.as_deref() else {
        return Ok(None);
    };
    let Some(target) = account::find_by_uri(pool, uri).await? else {
        return Ok(None);
    };
    let counts = account_counts_batch(pool, &[target.id]).await?;
    let counts = counts.get(&target.id).copied().unwrap_or_default();
    let allow_direct = allow_direct_media(pool, viewer).await;
    let entity = Box::pin(account_json_with_counts(
        pool,
        domain,
        &target,
        viewer,
        &counts,
        allow_direct,
        &AccountRenderCtx::Nested(pool),
    ))
    .await?;
    Ok(Some(entity))
}

/// Pre-rendered migration targets for every `(account id, moved_to_uri)` pair
/// in `movers`, keyed by the *moving* account's id. The targets are fetched in
/// one query and rendered as one nested batch — deduplicated, so ten accounts
/// that moved to the same target share one render — making a list page pay a
/// fixed number of extra queries when any row migrated, and none otherwise.
async fn moved_targets_map(
    pool: &PgPool,
    domain: &str,
    movers: &[(i64, String)],
    viewer: Option<i64>,
) -> Result<HashMap<i64, Value>, ApiError> {
    if movers.is_empty() {
        return Ok(HashMap::new());
    }
    let mut uris: Vec<&str> = movers.iter().map(|(_, uri)| uri.as_str()).collect();
    uris.sort_unstable();
    uris.dedup();
    let targets = account::find_by_uris(pool, &uris).await?;
    // The targets render with an empty moved map of their own: an embedded
    // migration target never carries its own `moved`, so this cannot recurse.
    let entities = Box::pin(render_accounts_inner(
        pool,
        domain,
        &targets,
        viewer,
        &HashMap::new(),
    ))
    .await?;
    let by_uri: HashMap<&str, &Value> = targets
        .iter()
        .zip(&entities)
        .filter_map(|(target, entity)| target.uri.as_deref().map(|uri| (uri, entity)))
        .collect();
    Ok(movers
        .iter()
        .filter_map(|(id, uri)| {
            by_uri
                .get(uri.as_str())
                .map(|entity| (*id, (*entity).clone()))
        })
        .collect())
}

/// The Mastodon `Role` entity (`REST::RoleSerializer`). `permissions` is the
/// bitmask rendered as a decimal string, matching Mastodon's wire format.
#[must_use]
pub fn role_json(role: &plamenu_db::role::Role) -> Value {
    json!({
        "id": role.id.to_string(),
        "name": role.name,
        "permissions": role.permissions.to_string(),
        "color": role.color,
        "highlighted": role.highlighted,
        // Mastodon's `RoleSerializer` emits `collection_limit`; Plamenu enforces
        // one global constant rather than a per-role column.
        "collection_limit": plamenu_db::collection::PER_ACCOUNT_LIMIT,
    })
}

fn admin_account_ip_json(
    ip: &plamenu_db::admin_account::AdminAccountIp,
) -> Result<Value, ApiError> {
    Ok(json!({
        "ip": ip.ip.as_str(),
        "used_at": rfc3339(ip.used_at)?,
    }))
}

/// The Mastodon `Admin::Account` entity (`REST::Admin::AccountSerializer`):
/// the public account plus the moderation overlay.
pub async fn admin_account_json(
    pool: &PgPool,
    domain: &str,
    view: &AdminAccountView,
) -> Result<Value, ApiError> {
    let account = account_json(pool, domain, &view.account, None).await?;
    // Mastodon's `confirmed`/`approved`/`disabled` come from the local user;
    // remote accounts have none, so the attributes serialize as `null`.
    let (confirmed, approved, disabled) = if view.has_user {
        (
            Value::Bool(view.confirmed_at.is_some()),
            Value::Bool(view.approved),
            Value::Bool(view.disabled),
        )
    } else {
        (Value::Null, Value::Null, Value::Null)
    };
    let ips = view
        .ips
        .iter()
        .map(admin_account_ip_json)
        .collect::<Result<Vec<_>, _>>()?;
    let ip = view
        .ips
        .first()
        .map_or(Value::Null, |ip| Value::String(ip.ip.clone()));
    Ok(json!({
        "id": view.account.id.to_string(),
        "username": view.account.username,
        "domain": if view.portable { Value::Null } else { json!(view.account.domain) },
        "portable": view.portable,
        "created_at": rfc3339(view.account.created_at)?,
        "email": view.email,
        "ip": ip,
        "ips": ips,
        "locale": view.locale,
        "invite_request": view.invite_request_text,
        "role": view.role.as_ref().map(role_json),
        "confirmed": confirmed,
        "approved": approved,
        "disabled": disabled,
        "suspended": view.account.suspended(),
        "silenced": view.account.silenced(),
        "sensitized": view.account.sensitized(),
        "account": account,
    }))
}

/// The Mastodon `Relationship` entity between the viewer and `target`.
/// `showing_reblogs`/`notifying`/`languages` carry the per-follow settings
/// of the outgoing edge — a pending request included, like Mastodon's
/// serializer falling back from `following_map` to `requested_map`. The
/// `showing_replies` field beside them is our extension.
pub async fn relationship_json(
    pool: &PgPool,
    viewer_id: i64,
    target: &Account,
) -> Result<Value, ApiError> {
    Ok(
        render_relationships(pool, viewer_id, std::slice::from_ref(target))
            .await?
            .pop()
            .unwrap_or_else(|| json!({})),
    )
}

/// Every per-target fact a batch of `Relationship` entities needs, prefetched
/// in a fixed number of queries (one per relation kind) instead of ~9 per
/// target. Feeds [`relationship_entity`], which cannot query — so the batched
/// and single (`relationship_json`) paths share one assembler and cannot drift.
struct RelationshipBatch {
    follows_out: HashMap<i64, follow::FollowEdge>,
    incoming_pending: HashMap<i64, bool>,
    blocking_out: HashSet<i64>,
    blocked_by: HashSet<i64>,
    mutes: HashMap<i64, mute::Mute>,
    domain_blocks: HashSet<String>,
    notes: HashMap<i64, String>,
    endorsed: HashSet<i64>,
}

impl RelationshipBatch {
    async fn load(pool: &PgPool, viewer_id: i64, ids: &[i64]) -> Result<Self, ApiError> {
        Ok(Self {
            follows_out: follow::find_out_batch(pool, viewer_id, ids).await?,
            incoming_pending: follow::pending_state_in_batch(pool, viewer_id, ids).await?,
            blocking_out: block::blocking_out_batch(pool, viewer_id, ids).await?,
            blocked_by: block::blocked_by_batch(pool, viewer_id, ids).await?,
            mutes: mute::active_batch(pool, viewer_id, ids).await?,
            domain_blocks: plamenu_db::account_domain_block::all_domains(pool, viewer_id).await?,
            notes: account_note::get_batch(pool, viewer_id, ids).await?,
            endorsed: endorsement::exists_batch(pool, viewer_id, ids).await?,
        })
    }
}

/// The Mastodon `Relationship` entity assembled purely from a prefetched
/// [`RelationshipBatch`] — no queries.
fn relationship_entity(target: &Account, batch: &RelationshipBatch) -> Result<Value, ApiError> {
    let outgoing = batch.follows_out.get(&target.id);
    let following = outgoing.is_some_and(|edge| !edge.pending);
    let requested = outgoing.is_some_and(|edge| edge.pending);
    let incoming = batch.incoming_pending.get(&target.id).copied();
    let followed_by = incoming == Some(false);
    let requested_by = incoming == Some(true);
    let blocking = batch.blocking_out.contains(&target.id);
    let blocked_by = batch.blocked_by.contains(&target.id);
    let mute = batch.mutes.get(&target.id);
    let domain_blocking = target
        .domain
        .as_deref()
        .is_some_and(|domain| batch.domain_blocks.contains(domain));
    let muting_expires_at = match mute.and_then(|m| m.expires_at) {
        Some(at) => Value::String(rfc3339(at)?),
        None => Value::Null,
    };
    let note = batch.notes.get(&target.id).cloned().unwrap_or_default();
    let endorsed = batch.endorsed.contains(&target.id);
    // `showing_replies` is ours, not Mastodon's: local viewer state
    // for a relationship, so it rides the same entity rather than needing a
    // second endpoint. Absent an edge it reads `false`, like `showing_reblogs`.
    let (showing_reblogs, showing_replies, notifying, languages) = match outgoing {
        Some(edge) => (
            edge.show_reblogs,
            edge.with_replies,
            edge.notify,
            edge.languages
                .as_ref()
                .map_or(Value::Null, |langs| json!(langs)),
        ),
        None => (false, false, false, Value::Null),
    };
    Ok(json!({
        "id": target.id.to_string(),
        "following": following,
        "showing_reblogs": showing_reblogs,
        "showing_replies": showing_replies,
        "notifying": notifying,
        "languages": languages,
        "followed_by": followed_by,
        "blocking": blocking,
        "blocked_by": blocked_by,
        "muting": mute.is_some(),
        "muting_notifications": mute.is_some_and(|m| m.hide_notifications),
        "muting_expires_at": muting_expires_at,
        "requested": requested,
        "requested_by": requested_by,
        "domain_blocking": domain_blocking,
        "endorsed": endorsed,
        "note": note,
    }))
}

/// Renders a batch of `Relationship` entities in a fixed number of queries
/// regardless of page size — the batched counterpart of [`relationship_json`],
/// for `/api/v1/accounts/relationships` and any other multi-target list.
pub async fn render_relationships(
    pool: &PgPool,
    viewer_id: i64,
    targets: &[Account],
) -> Result<Vec<Value>, ApiError> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = targets.iter().map(|t| t.id).collect();
    let batch = RelationshipBatch::load(pool, viewer_id, &ids).await?;
    targets
        .iter()
        .map(|target| relationship_entity(target, &batch))
        .collect()
}

/// The Mastodon shallow `Tag` shape (`REST::ShallowTagSerializer`) used inline
/// in status and announcement entities: just the normalized `name` and its
/// `url`, with no history or viewer flags.
#[must_use]
pub fn shallow_tag_json(domain: &str, name: &str) -> Value {
    let lower = name.to_lowercase();
    json!({
        "name": lower,
        "url": format!("https://{domain}/tags/{lower}"),
    })
}

/// The full Mastodon `Tag` entity (`REST::TagSerializer`). `name` carries the
/// tag's display casing while `url` uses its normalized form; `history` is the
/// 7-day usage series built by [`tag_history_series`]. `following`/`featuring`
/// are sent to authenticated viewers only (`Some`) and omitted for anonymous
/// requests (`None`). `id` is empty for a tag that has never been stored, like
/// Mastodon serializing an unsaved `Tag`'s nil id.
#[must_use]
pub fn hashtag_json(
    domain: &str,
    id: Option<i64>,
    name: &str,
    display_name: &str,
    following: Option<bool>,
    featuring: Option<bool>,
    history: Vec<Value>,
) -> Value {
    let lower = name.to_lowercase();
    let mut entity = json!({
        "id": id.map(|v| v.to_string()).unwrap_or_default(),
        "name": display_name,
        "url": format!("https://{domain}/tags/{lower}"),
    });
    entity["history"] = Value::Array(history);
    if let Some(following) = following {
        entity["following"] = Value::Bool(following);
    }
    if let Some(featuring) = featuring {
        entity["featuring"] = Value::Bool(featuring);
    }
    entity
}

/// The 7-entry `history` array — today back through six days ago, newest first
/// — from a tag's non-empty daily counts, mirroring Mastodon's
/// `Trends::History#as_json`: each entry is `{day, uses, accounts}` with `day`
/// the UTC-midnight Unix timestamp and every value stringified. Missing days
/// render as zeros.
#[must_use]
pub fn tag_history_series(counts: &[tag::DayCount], today: time::Date) -> Vec<Value> {
    let by_day: HashMap<time::Date, (i64, i64)> = counts
        .iter()
        .map(|c| (c.day, (c.uses, c.accounts)))
        .collect();
    history_series(&by_day, today)
}

/// The shared 7-entry `{day, uses, accounts}` series (newest first, zero-filled)
/// used by both tag and link Trends entities.
fn history_series(by_day: &HashMap<time::Date, (i64, i64)>, today: time::Date) -> Vec<Value> {
    (0..7)
        .map(|i| {
            let day = today - time::Duration::days(i);
            let (uses, accounts) = by_day.get(&day).copied().unwrap_or((0, 0));
            let secs = day
                .with_time(time::Time::MIDNIGHT)
                .assume_utc()
                .unix_timestamp();
            json!({
                "day": secs.to_string(),
                "uses": uses.to_string(),
                "accounts": accounts.to_string(),
            })
        })
        .collect()
}

/// The Mastodon `Trends::Link` entity — a `PreviewCard` plus its 7-day usage
/// `history`. When `admin` is set, adds the `id` and `requires_review` fields of
/// `Admin::Trends::LinkSerializer`.
pub fn trends_link_json(
    domain: &str,
    card: &PreviewCard,
    counts: &[preview_card_trend::DayCount],
    today: time::Date,
    admin: Option<bool>,
) -> Result<Value, ApiError> {
    // Trends are a public/anonymous surface, so no direct-fallback marker.
    let mut entity = preview_card_json(domain, card, "", false)?;
    let by_day: HashMap<time::Date, (i64, i64)> = counts
        .iter()
        .map(|c| (c.day, (c.uses, c.accounts)))
        .collect();
    entity["history"] = Value::Array(history_series(&by_day, today));
    if let Some(requires_review) = admin {
        entity["id"] = Value::String(card.id.to_string());
        entity["requires_review"] = Value::Bool(requires_review);
    }
    Ok(entity)
}

/// The `Admin::Trends::Links::PreviewCardProvider` entity.
pub fn preview_card_provider_json(
    provider: &plamenu_db::preview_card_provider::Provider,
) -> Result<Value, ApiError> {
    let reviewed_at = match provider.reviewed_at {
        Some(at) => Some(rfc3339(at)?),
        None => None,
    };
    let requested_review_at = match provider.requested_review_at {
        Some(at) => Some(rfc3339(at)?),
        None => None,
    };
    Ok(json!({
        "id": provider.id.to_string(),
        "domain": provider.domain,
        "trendable": provider.trendable,
        "reviewed_at": reviewed_at,
        "requested_review_at": requested_review_at,
        "requires_review": provider.requires_review(),
    }))
}

/// The inclusive UTC date window backing a tag's 7-day history: `(oldest,
/// today)`, six days apart.
fn tag_history_window() -> (time::Date, time::Date) {
    let today = OffsetDateTime::now_utc().date();
    (today - time::Duration::days(6), today)
}

/// Full `Tag` entity for a stored tag, resolving its 7-day history.
pub async fn tag_json(
    pool: &PgPool,
    domain: &str,
    tag: &tag::Tag,
    following: Option<bool>,
    featuring: Option<bool>,
) -> Result<Value, ApiError> {
    let (from, today) = tag_history_window();
    let counts = tag::history(pool, tag.id, from).await?;
    let history = tag_history_series(&counts, today);
    Ok(hashtag_json(
        domain,
        Some(tag.id),
        &tag.name,
        tag.display(),
        following,
        featuring,
        history,
    ))
}

/// Full `Tag` entity for a well-formed name that is not stored: an empty id and
/// an all-zero history, like Mastodon serializing an unsaved `Tag`.
#[must_use]
pub fn tag_json_absent(
    domain: &str,
    name: &str,
    following: Option<bool>,
    featuring: Option<bool>,
) -> Value {
    let (_, today) = tag_history_window();
    hashtag_json(
        domain,
        None,
        name,
        name,
        following,
        featuring,
        tag_history_series(&[], today),
    )
}

/// A stored tag row that can be rendered as a `Tag` entity in a page.
pub trait TagRow {
    fn tag_id(&self) -> i64;
    fn tag_name(&self) -> &str;
    fn tag_display(&self) -> &str;
}

impl TagRow for tag::Tag {
    fn tag_id(&self) -> i64 {
        self.id
    }
    fn tag_name(&self) -> &str {
        &self.name
    }
    fn tag_display(&self) -> &str {
        self.display()
    }
}

impl TagRow for tag::FollowedTag {
    fn tag_id(&self) -> i64 {
        self.id
    }
    fn tag_name(&self) -> &str {
        &self.name
    }
    fn tag_display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

impl TagRow for featured_tag::TagSuggestion {
    fn tag_id(&self) -> i64 {
        self.tag_id
    }
    fn tag_name(&self) -> &str {
        &self.name
    }
    fn tag_display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// Builds `Tag` entities for a page of stored tags, resolving every tag's 7-day
/// history in one batch query. `following`/`featuring` are the viewer's follow
/// and feature sets; when `authed` is false both flags are omitted (anonymous).
pub async fn tag_json_page<T: TagRow, S: std::hash::BuildHasher>(
    pool: &PgPool,
    domain: &str,
    rows: &[T],
    following: &HashSet<i64, S>,
    featuring: &HashSet<i64, S>,
    authed: bool,
) -> Result<Vec<Value>, ApiError> {
    let (from, today) = tag_history_window();
    let ids: Vec<i64> = rows.iter().map(TagRow::tag_id).collect();
    let histories = tag::history_batch(pool, &ids, from).await?;
    let empty: Vec<tag::DayCount> = Vec::new();
    Ok(rows
        .iter()
        .map(|row| {
            let id = row.tag_id();
            let counts = histories.get(&id).unwrap_or(&empty);
            hashtag_json(
                domain,
                Some(id),
                row.tag_name(),
                row.tag_display(),
                authed.then(|| following.contains(&id)),
                authed.then(|| featuring.contains(&id)),
                tag_history_series(counts, today),
            )
        })
        .collect())
}

/// Builds `Admin::Tag` entities for a page of tags (`REST::Admin::TagSerializer`
/// = the full Tag entity plus the moderation registry). `following`/`featuring`
/// are resolved for the moderator (admin endpoints are always authenticated);
/// the three registry booleans fall back to their defaults when unset
/// (usable/listable → true, trendable → the operator's `trendable_by_default`).
pub async fn admin_tag_json_page(
    pool: &PgPool,
    domain: &str,
    rows: &[tag::AdminTag],
    viewer_account_id: i64,
    trendable_default: bool,
) -> Result<Vec<Value>, ApiError> {
    let (from, today) = tag_history_window();
    let ids: Vec<i64> = rows.iter().map(|t| t.id).collect();
    let histories = tag::history_batch(pool, &ids, from).await?;
    let following = tag::followed_ids(pool, viewer_account_id, &ids).await?;
    let featuring = featured_tag::featured_ids(pool, viewer_account_id, &ids).await?;
    let empty: Vec<tag::DayCount> = Vec::new();
    Ok(rows
        .iter()
        .map(|tag| {
            let counts = histories.get(&tag.id).unwrap_or(&empty);
            let mut entity = hashtag_json(
                domain,
                Some(tag.id),
                &tag.name,
                tag.display(),
                Some(following.contains(&tag.id)),
                Some(featuring.contains(&tag.id)),
                tag_history_series(counts, today),
            );
            entity["trendable"] = Value::Bool(tag.trendable.unwrap_or(trendable_default));
            entity["usable"] = Value::Bool(tag.usable.unwrap_or(true));
            entity["listable"] = Value::Bool(tag.listable.unwrap_or(true));
            entity["requires_review"] = Value::Bool(tag.requires_review());
            entity
        })
        .collect())
}

/// The `Admin::Tag` entity for a single tag (show/update/approve/reject).
pub async fn admin_tag_json(
    pool: &PgPool,
    domain: &str,
    tag: &tag::AdminTag,
    viewer_account_id: i64,
    trendable_default: bool,
) -> Result<Value, ApiError> {
    let mut page = admin_tag_json_page(
        pool,
        domain,
        std::slice::from_ref(tag),
        viewer_account_id,
        trendable_default,
    )
    .await?;
    Ok(page.pop().unwrap_or(Value::Null))
}

/// `verify_credentials` returns an Account with a `source` attachment:
/// the unrendered bio and fields the user typed, plus the pending
/// follow-request count (capped at 40, like Mastodon's serializer).
pub async fn with_source(
    pool: &PgPool,
    mut account_entity: Value,
    account: &Account,
    settings: &UserSettings,
) -> Result<Value, ApiError> {
    let raw_fields: Vec<Value> = account
        .fields
        .as_array()
        .map(|stored| {
            stored
                .iter()
                .map(|field| {
                    json!({
                        "name": field.get("name").and_then(Value::as_str).unwrap_or(""),
                        "value": field.get("value").and_then(Value::as_str).unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    account_entity["source"] = json!({
        "privacy": settings.resolved_visibility(account.locked),
        "sensitive": settings.posting_default_sensitive,
        "language": settings.default_language(),
        "quote_policy": settings.posting_default_quote_policy.as_str(),
        // Plamenu extension: Mastodon keeps this setting web-UI-only.
        "show_application": settings.show_application,
        "note": account.note_source,
        "fields": raw_fields,
        "follow_requests_count": follow::count_requests(pool, account.id, 40).await?,
        "discoverable": account.discoverable,
        "indexable": account.indexable,
        "attribution_domains": plamenu_db::account::attribution_domains(pool, account.id).await?,
        "hide_collections": account.hide_collections,
    });
    // The owner's full role, falling back to the implicit "Everyone" role
    // like Mastodon's `User#role` (`CredentialAccountSerializer#role`).
    let role = plamenu_db::role::for_account(pool, account.id)
        .await?
        .unwrap_or_else(plamenu_db::role::everyone);
    account_entity["role"] = role_json(&role);
    Ok(account_entity)
}

/// The Mastodon `Profile` entity (`REST::ProfileSerializer`) — the
/// profile-editing view returned by `GET`/`PATCH /api/v1/profile`. It shares
/// avatar/header rendering with the Account entity but exposes the raw
/// (source) note and fields the owner typed alongside their formatted forms.
pub async fn profile_json(
    pool: &PgPool,
    domain: &str,
    account: &Account,
) -> Result<Value, ApiError> {
    let missing_image = format!("https://{domain}/static/missing.png");
    // The profile view is the local owner's own account: avatar/header are
    // always local uploads, so the direct-fallback marker is irrelevant.
    let avatar = avatar_url(domain, account, false).unwrap_or_else(|| missing_image.clone());
    let header = header_url(domain, account, false).unwrap_or_else(|| missing_image.clone());
    // The raw source fields the owner typed (name/value), carrying the same
    // `verified_at` as their formatted forms.
    let raw_fields: Vec<Value> = account
        .fields
        .as_array()
        .map(|stored| {
            stored
                .iter()
                .map(|field| {
                    json!({
                        "name": field.get("name").and_then(Value::as_str).unwrap_or(""),
                        "value": field.get("value").and_then(Value::as_str).unwrap_or(""),
                        "verified_at": field.get("verified_at").and_then(Value::as_str),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let featured = featured_tag::list(pool, account.id).await?;
    let featured_tags: Vec<Value> = featured
        .iter()
        .map(|tag| featured_tag_json(domain, account, tag))
        .collect();
    Ok(json!({
        "id": account.id.to_string(),
        "display_name": account.display_name,
        "note": account.note_source,
        "fields": raw_fields,
        "formatted_note": account.note,
        "formatted_fields": fields_json(account),
        "avatar": avatar,
        "avatar_static": avatar,
        "avatar_description": account.avatar_description,
        "header": header,
        "header_static": header,
        "header_description": account.header_description,
        "locked": account.locked,
        "bot": account.is_bot,
        "hide_collections": account.hide_collections,
        "discoverable": account.discoverable,
        "indexable": account.indexable,
        "show_media": account.show_media,
        "show_media_replies": account.show_media_replies,
        "show_featured": account.show_featured,
        "attribution_domains": plamenu_db::account::attribution_domains(pool, account.id).await?,
        "featured_tags": featured_tags,
    }))
}

/// The Mastodon `FeaturedTag` entity. `statuses_count` is a string and
/// `last_status_at` a bare date (or null), like Mastodon's serializer; the
/// `url` points at the owner's tagged-statuses page.
#[must_use]
pub fn featured_tag_json(
    domain: &str,
    account: &Account,
    tag: &featured_tag::FeaturedTag,
) -> Value {
    let lower = tag.name.to_lowercase();
    json!({
        "id": tag.id.to_string(),
        "name": lower,
        "url": format!("{}/tagged/{lower}", account_web_url(domain, account)),
        "statuses_count": tag.statuses_count.to_string(),
        "last_status_at": bare_date(tag.last_status_at),
    })
}

/// Mastodon's `last_status_at` wire format: a bare date (`2026-07-03`), or
/// null when the subject has never posted.
fn bare_date(at: Option<OffsetDateTime>) -> Value {
    match at {
        Some(at) => Value::String(format!(
            "{:04}-{:02}-{:02}",
            at.year(),
            u8::from(at.month()),
            at.day()
        )),
        None => Value::Null,
    }
}

/// The public URL of an attachment: our cached copy when we have one, else the
/// remote source (Mastodon serves `local ?? remote_url`).
fn media_url(domain: &str, item: &Media) -> String {
    match (&item.file_name, &item.remote_url) {
        (Some(file), _) => format!("https://{domain}/media/{file}"),
        (None, Some(remote)) => remote.clone(),
        (None, None) => String::new(),
    }
}

/// The image-style `meta` block (`original`/`small`): dimensions plus
/// Mastodon's `size`/`aspect`.
fn geometry_meta(width: i32, height: i32) -> Value {
    json!({
        "width": width,
        "height": height,
        "size": format!("{width}x{height}"),
        "aspect": f64::from(width) / f64::from(height.max(1)),
    })
}

/// The Mastodon `MediaAttachment` entity. `allow_direct` bakes the viewer's
/// direct-remote preference into any proxy URL this attachment emits.
#[must_use]
pub fn media_json(domain: &str, item: &Media, allow_direct: bool) -> Value {
    media_json_hls(domain, item, allow_direct, None)
}

/// The HLS extension for an HLS-native video attachment: the caching-proxy
/// master playlist plus the rendition ladder, so the first-party player and
/// capable clients get a quality selector. Dumb Mastodon-API clients ignore it
/// and use the single-URL progressive `url`.
fn hls_ext(domain: &str, media_id: i64, renditions: &[MediaRendition]) -> Value {
    let renditions: Vec<Value> = renditions
        .iter()
        .map(|r| {
            json!({
                "height": r.height,
                "width": r.width,
                "frame_rate": r.frame_rate,
                "audio_only": r.is_audio,
            })
        })
        .collect();
    json!({
        "master": format!("https://{domain}/media/hls/{media_id}/master.m3u8"),
        "renditions": renditions,
    })
}

/// [`media_json`] plus the optional HLS rendition ladder (from
/// [`plamenu_db::media::renditions_for`], loaded in a batch by the status
/// serializer). Non-HLS attachments get exactly the Mastodon shape.
#[must_use]
pub fn media_json_hls(
    domain: &str,
    item: &Media,
    allow_direct: bool,
    renditions: Option<&[MediaRendition]>,
) -> Value {
    let kind = item.kind_or_derived();
    let local_url = |file: &str| format!("https://{domain}/media/{file}");
    let proxy = |small: bool| media_proxy_url(domain, "attachment", item.id, small, allow_direct);
    // A live broadcast is HLS-native with an empty ladder: PeerTube publishes
    // no file Links for one, so its qualities exist only inside the playlist.
    // It is playable only while actually on air — an announced live has no
    // playlist yet, and a finished one's segments are deleted out from under
    // the master it still advertises.
    let on_air = item.live_state.as_deref() == Some("live") && item.hls_master_url.is_some();
    let hls_native = renditions.is_some_and(|rows| !rows.is_empty()) || on_air;
    // A live cannot be a sparse-range MP4 facade (there is no complete file to
    // index), so it gets the remuxing gateway instead: one endless fragmented
    // MP4 that plays in a bare <video>, in ExoPlayer and in VLC alike.
    let progressive = || {
        if item.live_state.is_some() {
            format!("https://{domain}/media/live/{}/stream.mp4", item.id)
        } else {
            format!("https://{domain}/media/play/{}/video.mp4", item.id)
        }
    };
    let hls_master = || format!("https://{domain}/media/hls/{}/master.m3u8", item.id);
    // HLS-native media keeps one stable compatibility URL; that gateway uses
    // an old cached copy when present, otherwise sparse origin ranges. For an
    // ordinary remote attachment the cached copy wins, with uncached media
    // routed through our proxy. A spooled local upload (still transcoding, no
    // remote_url) has no public URL yet, like Mastodon's `not_processed?`.
    let url: Option<String> = if item.live_state.is_some() {
        // Off air there is nothing to hand a player. Mastodon already has a
        // shape for "this attachment has no bytes yet" — a null `url` with a
        // preview — and clients render it as the poster instead of a video
        // element that would only fail. `live.state` says why.
        on_air.then(progressive)
    } else if hls_native && item.remote_url.is_some() {
        Some(progressive())
    } else if !item.not_processed() && item.file_name.is_some() {
        item.file_name.as_deref().map(local_url)
    } else {
        item.remote_url.as_ref().map(|_| proxy(false))
    };
    let preview_url: Option<String> = if let Some(small) = item.small_file_name.as_deref() {
        Some(local_url(small))
    } else if item.remote_url.is_some() {
        // Remote, no cached preview yet: the proxy regenerates one when it
        // downloads and re-processes the original.
        Some(proxy(true))
    } else if kind == "image" {
        // Local image, no generated preview: show the full (local) image.
        url.clone()
    } else {
        // Local audio/video without a generated preview has none.
        None
    };

    let mut meta = serde_json::Map::new();
    if item.duration.is_some() || item.bitrate.is_some() {
        // Video/audio metadata, compacted like Mastodon's `video_metadata`.
        let mut original = serde_json::Map::new();
        if let Some(width) = item.width {
            original.insert("width".into(), json!(width));
        }
        if let Some(height) = item.height {
            original.insert("height".into(), json!(height));
        }
        if let Some(frame_rate) = &item.frame_rate {
            original.insert("frame_rate".into(), json!(frame_rate));
        }
        if let Some(duration) = item.duration {
            original.insert("duration".into(), json!(duration));
        }
        if let Some(bitrate) = item.bitrate {
            original.insert("bitrate".into(), json!(bitrate));
        }
        meta.insert("original".into(), Value::Object(original));
    } else if let (Some(width), Some(height)) = (item.width, item.height) {
        meta.insert("original".into(), geometry_meta(width, height));
    }
    if let (Some(width), Some(height)) = (item.small_width, item.small_height) {
        meta.insert("small".into(), geometry_meta(width, height));
    }
    if let (Some(x), Some(y)) = (item.focus_x, item.focus_y) {
        meta.insert("focus".into(), json!({ "x": x, "y": y }));
    }

    let mut value = json!({
        "id": item.id.to_string(),
        "type": kind,
        "url": url,
        "preview_url": preview_url,
        // Mastodon exposes the true origin here; we deliberately re-point these
        // at our proxy so a client reading `remote_url` (Phanpy and others
        // hotlink it) still never contacts the origin. Local media stays null.
        "remote_url": item.remote_url.as_ref().map(|_| {
            if hls_native { hls_master() } else { proxy(false) }
        }),
        "preview_remote_url": item.thumbnail_remote_url.as_ref().map(|_| proxy(true)),
        "text_url": null,
        "description": item.description,
        "blurhash": item.blurhash,
        "meta": meta,
    });
    // An HLS-native video carries the master + ladder as an additive extension;
    // non-HLS attachments keep exactly the Mastodon shape. A live on air has no
    // stored ladder — its qualities live in the playlist the player is about to
    // fetch — but it still needs the master advertised.
    let ladder = renditions.unwrap_or_default();
    if let (true, Value::Object(map)) = (!ladder.is_empty() || on_air, &mut value) {
        map.insert("hls".into(), hls_ext(domain, item.id, ladder));
    }
    // What a client needs to render a broadcast that has no bytes to play.
    // Conditional, never a null placeholder, so a non-live attachment keeps
    // exactly the Mastodon shape.
    if let (Some(state), Value::Object(map)) = (item.live_state.as_deref(), &mut value) {
        map.insert(
            "live".into(),
            json!({ "state": state, "permanent": item.live_permanent }),
        );
    }
    value
}

/// Seconds as the ISO 8601 duration Mastodon's `NoteSerializer` emits
/// (`ActiveSupport::Duration#iso8601`: a bare seconds part, floats kept).
fn iso8601_duration(seconds: f64) -> String {
    if seconds.fract() == 0.0 {
        format!("PT{seconds:.1}S")
    } else {
        format!("PT{seconds}S")
    }
}

/// Attachments of a status as `ActivityPub` `Document` objects.
#[must_use]
pub fn ap_attachments(domain: &str, items: &[Media]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let mut document = json!({
                "type": "Document",
                "mediaType": item.content_type,
                "url": media_url(domain, item),
                "name": item.description,
                "blurhash": item.blurhash,
            });
            if let (Some(x), Some(y)) = (item.focus_x, item.focus_y) {
                document["focalPoint"] = json!([x, y]);
            }
            if let Some(width) = item.width {
                document["width"] = json!(width);
            }
            if let Some(height) = item.height {
                document["height"] = json!(height);
            }
            if let Some(duration) = item.duration {
                document["duration"] = json!(iso8601_duration(duration));
            }
            document
        })
        .collect()
}

/// The viewer-independent maps of one status render pass — the shared layer
/// of the two-layer renderer (N+1 decisions), computed once per batch
/// however many viewers the render serves.
struct SharedMaps {
    webxdc_invitations: HashMap<i64, Value>,
    targets: HashMap<i64, Status>,
    /// The author `Account` rows (statuses, boost targets and attributing
    /// communities). The rendered entities are viewer-relative
    /// (`feature_approval`, `moved`) and live in [`ViewerMaps::author_entities`].
    author_rows: HashMap<i64, Account>,
    engagement: HashMap<i64, Engagement>,
    /// Status ids that are group posts — attributed to a local group or
    /// boosted by a Group actor. Gates the vote fields' meaning and the web
    /// vote buttons.
    group_attributed: HashSet<i64>,
    /// Group account ids that attribute each status, from local audience
    /// mentions and remote Group Announce rows. The corresponding rendered
    /// Account entities live in the author maps, so status assembly stays
    /// query-free.
    group_attributions: HashMap<i64, Vec<i64>>,
    /// Status ids whose thread is locked in a group (no new comments).
    group_locked: HashSet<i64>,
    /// Reply-parent author account ids, keyed by *parent* status id — fills
    /// `in_reply_to_account_id`. A missing parent (deleted, never fetched)
    /// leaves the field null, like Mastodon.
    reply_parents: HashMap<i64, i64>,
    mentions: HashMap<i64, Vec<Value>>,
    tags: HashMap<i64, Vec<Value>>,
    /// Pleroma emoji-reaction groups, keyed by status id. The per-viewer `me`
    /// flag is derived from each group's reactor ids at assembly, so one pass
    /// serves every viewer.
    reactions: HashMap<i64, Vec<reaction::ReactionGroup>>,
    /// Accepted quote target links keyed by quoting status: both the `ActivityPub`
    /// object id and the target's canonical human-facing URL when they differ.
    /// The stored content retains its cross-software `RE:` fallback for
    /// failed/pending states; successful native quote renders remove it from
    /// client-facing content.
    accepted_quote_links: HashMap<i64, Vec<String>>,
    applications: HashMap<i64, Value>,
    show_application: HashMap<i64, bool>,
    /// Status ids that are soft-deleted stubs — rendered as a "deleted status"
    /// placeholder so a deleted middle post still threads (`GtS`). Batched once so
    /// the placeholder branch adds no per-row query.
    deleted: HashSet<i64>,
    /// Reply-parent AP URIs keyed by *child* status id, for replies whose parent
    /// was never fetched (`in_reply_to_id` NULL, `in_reply_to_uri` set). Emitted
    /// as the `in_reply_to_uri` extension so the web client can show an
    /// "unfetched parent" notice and link out, instead of rendering the reply as
    /// a standalone post. Batched, like `deleted`.
    unresolved_parent_uris: HashMap<i64, String>,
}

/// The maps whose proxy URLs bake in the viewer's direct-remote-media preference
/// — computed once per distinct opt-in value among the viewer set (at most
/// twice), never per viewer.
struct VariantMaps {
    attachments: HashMap<i64, Vec<Value>>,
    /// Rendered `PreviewCard` entities, keyed by status id.
    cards: HashMap<i64, Value>,
    /// Rendered `CustomEmoji` entities, keyed by status id.
    emojis: HashMap<i64, Vec<Value>>,
    /// The loaded emoji table the per-viewer poll assembly selects its
    /// options' emoji from — no query of its own.
    emoji_table: EmojiTable,
    /// Client-facing (proxied) reaction custom-emoji URLs, keyed by status id
    /// then reaction name; a missing entry means the reaction shows no image.
    reaction_emoji_urls: HashMap<i64, HashMap<String, String>>,
}

/// The per-viewer dimensions of one render pass. Each is resolved as one
/// batched query across the whole viewer set and assembled here per viewer
/// without further round trips.
struct ViewerMaps {
    viewer: Option<i64>,
    /// This viewer's rendered author entities (`feature_approval` and the
    /// `moved` attachment are viewer-relative), keyed by account id.
    author_entities: HashMap<i64, Value>,
    favourited: HashSet<i64>,
    /// Status ids the viewer downvoted (group votes).
    disliked: HashSet<i64>,
    reblogged: HashSet<i64>,
    bookmarked: HashSet<i64>,
    pinned: HashSet<i64>,
    /// Status ids whose conversation the viewer mutes.
    muted: HashSet<i64>,
    /// Batch author ids the viewer actively follows — the `followers` branch of
    /// a status' quote policy (`quote_approval.current_user`).
    quote_followed_authors: HashSet<i64>,
    /// Batch author ids that actively follow the viewer — the `following`
    /// branch of a remote status' quote policy.
    quote_following_authors: HashSet<i64>,
    /// Rendered `Collection` entities the status references (FEP-7aa9
    /// `tagged_collections`), keyed by status id — member visibility is
    /// viewer-relative.
    tagged_collections: HashMap<i64, Vec<Value>>,
    quotes: HashMap<i64, Value>,
    /// Rendered `Poll` entities (`voted`/`own_votes`), keyed by status id.
    polls: HashMap<i64, Value>,
    /// The viewer's `filtered` (`FilterResult` array) per status id — empty
    /// for anonymous viewers, who never carry the attribute.
    filtered: HashMap<i64, Value>,
    /// Rendered event sidecars (`Event` objects, RSVP state is
    /// viewer-relative), keyed by status id.
    events: HashMap<i64, Value>,
}

/// One viewer's complete render scope: the shared layer, the direct-media
/// variant matching the viewer's preference, and the viewer's own dimensions.
struct RenderMaps<'a> {
    shared: &'a SharedMaps,
    variant: &'a VariantMaps,
    viewer_maps: &'a ViewerMaps,
}

/// The Mastodon `Poll` entity. Option tallies are hidden (`null`) while a
/// `hide_totals` poll is still running; `voted`/`own_votes` appear only for
/// authenticated viewers, like Mastodon. `emojis` carries the author's
/// custom emoji referenced in the options.
pub fn poll_json(
    item: &Poll,
    viewer: Option<i64>,
    own_votes: &[i32],
    emojis: &[Value],
) -> Result<Value, ApiError> {
    let expired = item.expired();
    let show_totals = expired || !item.hide_totals;
    let options: Vec<Value> = item
        .options
        .iter()
        .enumerate()
        .map(|(index, title)| {
            let count = show_totals.then(|| item.cached_tallies.get(index).copied().unwrap_or(0));
            json!({ "title": title, "votes_count": count })
        })
        .collect();
    let expires_at = match item.expires_at {
        Some(at) => Some(rfc3339(at)?),
        None => None,
    };
    let mut entity = json!({
        "id": item.id.to_string(),
        "expires_at": expires_at,
        "expired": expired,
        "multiple": item.multiple,
        "votes_count": item.votes_count(),
        "voters_count": item.voters_count,
        "options": options,
        "emojis": emojis,
    });
    if let Some(viewer_id) = viewer {
        entity["voted"] = Value::Bool(viewer_id == item.account_id || !own_votes.is_empty());
        entity["own_votes"] = json!(own_votes);
    }
    Ok(entity)
}

/// Pre-rendered `Poll` entities for a batch of statuses, assembled for one
/// viewer from the set-wide prefetched votes. `author_rows` supplies each
/// poll author's domain, which selects the options' custom emoji out of the
/// page's already-loaded [`EmojiTable`] (poll options are part of
/// [`emoji_source_texts`], so the table always covers them) — no per-poll
/// emoji query, and no query at all in this assembly.
fn assemble_poll_map(
    domain: &str,
    by_status: &HashMap<i64, Poll>,
    author_rows: &HashMap<i64, Account>,
    emoji_table: &EmojiTable,
    viewer: Option<i64>,
    own_votes: &HashMap<i64, Vec<i32>>,
    allow_direct: bool,
) -> Result<HashMap<i64, Value>, ApiError> {
    if by_status.is_empty() {
        return Ok(HashMap::new());
    }
    let mut rendered = HashMap::with_capacity(by_status.len());
    for (status_id, item) in by_status {
        let votes = own_votes.get(&item.id).map_or(&[][..], Vec::as_slice);
        let author = author_rows.get(&item.account_id);
        let author_domain = author.and_then(|author| author.domain.clone());
        let author_owner = author
            .filter(|author| author.domain.is_none())
            .map(|author| author.id);
        let texts: Vec<&str> = item.options.iter().map(String::as_str).collect();
        let emojis = emojis_from_table(
            domain,
            emoji_table.get(&(author_domain, author_owner)),
            &texts,
            allow_direct,
        );
        rendered.insert(*status_id, poll_json(item, viewer, votes, &emojis)?);
    }
    Ok(rendered)
}

/// The `CustomEmoji` entities a poll's options reference.
pub async fn poll_emojis_json(
    pool: &PgPool,
    domain: &str,
    author_domain: Option<&str>,
    item: &Poll,
    allow_direct: bool,
) -> Result<Vec<Value>, ApiError> {
    let texts: Vec<&str> = item.options.iter().map(String::as_str).collect();
    crate::emoji::emojis_json(pool, domain, author_domain, &texts, allow_direct).await
}

/// The Mastodon `PreviewCard` entity. `url` is the link as it appeared in
/// the status when recorded (Mastodon's `original_url`), falling back to the
/// card's canonical URL. `image` is our cached copy or the media proxy (never
/// the origin), so `blurhash` is always null.
pub fn preview_card_json(
    domain: &str,
    card: &PreviewCard,
    original_url: &str,
    allow_direct: bool,
) -> Result<Value, ApiError> {
    // A verified `fediverse:creator` author keeps the row even with no
    // og:author metadata; the account entity is patched in by `card_map`
    // (this function stays sync for the trends path).
    let authors = if card.author_name.is_empty()
        && card.author_url.is_empty()
        && card.author_account_id.is_none()
    {
        json!([])
    } else {
        json!([{ "name": card.author_name, "url": card.author_url, "account": null }])
    };
    let published_at = match card.published_at {
        Some(at) => Some(rfc3339(at)?),
        None => None,
    };
    // Our cached copy wins; an un-downloaded image routes through the proxy.
    // A card with no image at all stays null.
    let image = if let Some(file) = &card.image_file_name {
        Some(format!("https://{domain}/media/{file}"))
    } else {
        card.image_url
            .as_ref()
            .map(|_| media_proxy_url(domain, "card", card.id, false, allow_direct))
    };
    Ok(json!({
        "url": if original_url.is_empty() { card.url.as_str() } else { original_url },
        "title": card.title,
        "description": card.description,
        "language": card.language,
        "type": card.kind,
        "author_name": card.author_name,
        "author_url": card.author_url,
        "provider_name": card.provider_name,
        "provider_url": card.provider_url,
        "html": card.html,
        "width": card.width,
        "height": card.height,
        "image": image,
        "image_description": card.image_description,
        "embed_url": card.embed_url,
        "blurhash": null,
        "published_at": published_at,
        "authors": authors,
    }))
}

/// Which batch authors the viewer actively follows — the `followers` branch of
/// a status' quote policy (`quote_approval.current_user`). Read straight off
/// the page's prefetched accepted-follow edges (no per-author query); an
/// anonymous viewer, whose `relations` are empty, follows nobody.
fn followed_authors(
    viewer: Option<i64>,
    relations: &FeatureRelations,
    author_rows: &HashMap<i64, Account>,
) -> HashSet<i64> {
    let Some(viewer_id) = viewer else {
        return HashSet::new();
    };
    author_rows
        .keys()
        .copied()
        .filter(|&author_id| author_id != viewer_id && relations.follows_out.contains(&author_id))
        .collect()
}

/// Which batch authors actively follow the viewer — the inverse accepted edge
/// used by an inbound `following` quote-policy audience.
fn following_authors(
    viewer: Option<i64>,
    relations: &FeatureRelations,
    author_rows: &HashMap<i64, Account>,
) -> HashSet<i64> {
    let Some(viewer_id) = viewer else {
        return HashSet::new();
    };
    author_rows
        .keys()
        .copied()
        .filter(|&author_id| author_id != viewer_id && relations.followed_by.contains(&author_id))
        .collect()
}

/// The viewer-independent half of the FEP-7aa9 `tagged_collections` render:
/// the referenced collections, their owners and their tag entities, fetched
/// once for the whole viewer set. Member lists are viewer-relative and load
/// separately (one batched query across the set).
struct TaggedCollectionsShared {
    by_status: HashMap<i64, Vec<plamenu_db::collection::Collection>>,
    owners: HashMap<i64, Account>,
    tag_jsons: HashMap<i64, Value>,
    collection_ids: Vec<i64>,
}

async fn tagged_collections_shared(
    pool: &PgPool,
    domain: &str,
    all_ids: &[i64],
) -> Result<TaggedCollectionsShared, ApiError> {
    let by_status = tagged_object::collections_for_statuses(pool, all_ids).await?;
    // Owners, tags and member items are all fetched once for the whole page
    // and each distinct collection is rendered once — a
    // collection tagged onto twenty statuses used to re-fetch its owner and
    // item list twenty times.
    let owner_ids: Vec<i64> = by_status
        .values()
        .flatten()
        .map(|collection| collection.account_id)
        .collect();
    let owners: HashMap<i64, Account> = if owner_ids.is_empty() {
        HashMap::new()
    } else {
        account::find_by_ids(pool, &owner_ids)
            .await?
            .into_iter()
            .map(|owner| (owner.id, owner))
            .collect()
    };
    let mut tag_jsons: HashMap<i64, Value> = HashMap::new();
    for collection in by_status.values().flatten() {
        if let Some(tag_id) = collection.tag_id
            && let std::collections::hash_map::Entry::Vacant(slot) = tag_jsons.entry(tag_id)
        {
            slot.insert(crate::collections::shallow_tag_json(pool, domain, tag_id).await?);
        }
    }
    let mut collection_ids: Vec<i64> = by_status
        .values()
        .flatten()
        .map(|collection| collection.id)
        .collect();
    collection_ids.sort_unstable();
    collection_ids.dedup();
    Ok(TaggedCollectionsShared {
        by_status,
        owners,
        tag_jsons,
        collection_ids,
    })
}

/// One viewer's rendered `Collection` entities per status id, serialized for
/// `viewer` like Mastodon's `REST::CollectionSerializer` — assembly only, the
/// member lists having been prefetched for the whole viewer set.
fn assemble_tagged_collections(
    domain: &str,
    shared: &TaggedCollectionsShared,
    items: &HashMap<i64, Vec<plamenu_db::collection::CollectionItem>>,
) -> Result<HashMap<i64, Vec<Value>>, ApiError> {
    let mut entities_by_collection: HashMap<i64, Value> = HashMap::new();
    for collection in shared.by_status.values().flatten() {
        if entities_by_collection.contains_key(&collection.id) {
            continue;
        }
        let owner = shared
            .owners
            .get(&collection.account_id)
            .ok_or(ApiError::NotFound)?;
        let tag = collection
            .tag_id
            .and_then(|tag_id| shared.tag_jsons.get(&tag_id).cloned())
            .unwrap_or(Value::Null);
        let members = items.get(&collection.id).map_or(&[][..], Vec::as_slice);
        entities_by_collection.insert(
            collection.id,
            crate::collections::collection_json_preloaded(
                domain, collection, owner, &tag, members,
            )?,
        );
    }
    let mut rendered = HashMap::with_capacity(shared.by_status.len());
    for (status_id, collections) in &shared.by_status {
        let entities = collections
            .iter()
            .map(|collection| {
                entities_by_collection
                    .get(&collection.id)
                    .cloned()
                    .ok_or(ApiError::NotFound)
            })
            .collect::<Result<Vec<Value>, ApiError>>()?;
        rendered.insert(*status_id, entities);
    }
    Ok(rendered)
}

/// Pre-rendered `PreviewCard` entities for a batch of statuses.
async fn card_map(
    pool: &PgPool,
    domain: &str,
    all_ids: &[i64],
    allow_direct: bool,
) -> Result<HashMap<i64, Value>, ApiError> {
    let mut cards = HashMap::new();
    let page = preview_card::for_statuses(pool, all_ids).await?;
    // Verified `fediverse:creator` authors are rare; fetch the distinct set in
    // one query and render them as one batch — a fixed number of queries per
    // page instead of a live `account_json` per distinct author.
    let author_ids: Vec<i64> = page
        .values()
        .filter_map(|(card, _)| card.author_account_id)
        .collect();
    let mut author_entities: HashMap<i64, Value> = HashMap::new();
    if !author_ids.is_empty() {
        let authors = account::find_by_ids(pool, &author_ids).await?;
        let rendered = render_accounts(pool, domain, &authors, None).await?;
        for (author, entity) in authors.iter().zip(rendered) {
            author_entities.insert(author.id, entity);
        }
    }
    for (status_id, (card, original_url)) in page {
        let mut entity = preview_card_json(domain, &card, &original_url, allow_direct)?;
        if let Some(author_id) = card.author_account_id
            && let Some(rendered) = author_entities.get(&author_id)
        {
            entity["authors"][0]["account"] = rendered.clone();
        }
        cards.insert(status_id, entity);
    }
    Ok(cards)
}

/// Renders a batch of statuses as Mastodon `Status` entities, resolving boost
/// targets, authors, engagement counters and the viewer's own flags.
pub async fn render_statuses(
    pool: &PgPool,
    domain: &str,
    items: &[Status],
    viewer: Option<i64>,
) -> Result<Vec<Value>, ApiError> {
    render_statuses_inner(pool, domain, items, viewer, 0).await
}

/// The viewer-independent half of the `quote` render: the quote rows and the
/// accepted quoted statuses, fetched once for the whole viewer set. The
/// quoted statuses themselves render through one recursive viewer-set pass —
/// nesting stays capped exactly as before (past `depth` 0 the quoted status
/// is referenced by id only, which also breaks quote cycles).
struct QuoteShared {
    rows: Vec<(i64, quote::Quote)>,
    quoted_by_id: HashMap<i64, Status>,
    /// The quoted statuses as one stable batch — the recursive render's item
    /// order, so each viewer's rendered values zip back onto status ids.
    quoted_batch: Vec<Status>,
}

async fn quote_shared_fetch(pool: &PgPool, all_ids: &[i64]) -> Result<QuoteShared, ApiError> {
    let rows: Vec<(i64, quote::Quote)> = quote::for_statuses(pool, all_ids)
        .await?
        .into_iter()
        // A non-accepted legacy quote (Misskey-style, no consent handshake)
        // renders as a plain post, not an eternal "pending" placeholder —
        // Mastodon's `Quote#acceptable?` gate.
        .filter(|(_, row)| !(row.legacy && row.state != "accepted"))
        .collect();
    let quoted_ids: Vec<i64> = rows
        .iter()
        .filter(|(_, row)| row.state == "accepted")
        .filter_map(|(_, row)| row.quoted_status_id)
        .collect();
    let quoted_by_id: HashMap<i64, Status> = if quoted_ids.is_empty() {
        HashMap::new()
    } else {
        status::find_by_ids(pool, &quoted_ids)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect()
    };
    let quoted_batch: Vec<Status> = quoted_by_id.values().cloned().collect();
    Ok(QuoteShared {
        rows,
        quoted_by_id,
        quoted_batch,
    })
}

/// One viewer's pre-rendered `quote` entities per status id, assembled from
/// the shared quote rows and that viewer's recursively rendered quoted batch.
fn assemble_quote_map(
    shared: &QuoteShared,
    quoted_values: &[Value],
    depth: u8,
) -> HashMap<i64, Value> {
    let mut rendered_quoted: HashMap<i64, Value> = HashMap::new();
    if depth == 0 {
        for (status, value) in shared.quoted_batch.iter().zip(quoted_values) {
            rendered_quoted.insert(status.id, value.clone());
        }
    }
    let mut quotes = HashMap::new();
    for (status_id, row) in &shared.rows {
        let value = if row.state == "accepted" {
            match row
                .quoted_status_id
                .filter(|id| shared.quoted_by_id.contains_key(id))
            {
                Some(quoted_id) if depth == 0 => json!({
                    "state": "accepted",
                    "quoted_status": rendered_quoted.get(&quoted_id).cloned(),
                }),
                Some(quoted_id) => json!({
                    "state": "accepted",
                    "quoted_status_id": quoted_id.to_string(),
                }),
                None => json!({ "state": "deleted", "quoted_status": null }),
            }
        } else {
            json!({ "state": row.state, "quoted_status": null })
        };
        quotes.insert(*status_id, value);
    }
    quotes
}

/// One viewer's per-status flags: favourited, downvoted, reblogged,
/// bookmarked, pinned and conversation-muted id sets (all empty for
/// anonymous viewers).
#[derive(Default)]
struct ViewerFlags {
    favourited: HashSet<i64>,
    disliked: HashSet<i64>,
    reblogged: HashSet<i64>,
    bookmarked: HashSet<i64>,
    pinned: HashSet<i64>,
    muted: HashSet<i64>,
}

/// Every signed-in viewer's per-status flags, each dimension one batched
/// query across the whole viewer set (N+1 decisions).
async fn viewer_flags_for_set(
    pool: &PgPool,
    signed_in: &[i64],
    all_ids: &[i64],
) -> Result<HashMap<i64, ViewerFlags>, ApiError> {
    if signed_in.is_empty() {
        return Ok(HashMap::new());
    }
    // Six independent reads issued concurrently: the latency of
    // this step is the slowest round trip, not the sum of all six.
    let (favourited, disliked, reblogged, bookmarked, pinned, muted) = tokio::try_join!(
        favourite::favourited_of_viewers(pool, signed_in, all_ids),
        plamenu_db::dislike::disliked_of_viewers(pool, signed_in, all_ids),
        status::reblogged_of_viewers(pool, signed_in, all_ids),
        bookmark::bookmarked_of_viewers(pool, signed_in, all_ids),
        pin::pinned_of_viewers(pool, signed_in, all_ids),
        conversation::muted_status_ids_for_viewers(pool, signed_in, all_ids),
    )?;
    let mut flags: HashMap<i64, ViewerFlags> = signed_in
        .iter()
        .map(|&viewer| (viewer, ViewerFlags::default()))
        .collect();
    let fold = |flags: &mut HashMap<i64, ViewerFlags>,
                pairs: Vec<(i64, i64)>,
                pick: fn(&mut ViewerFlags) -> &mut HashSet<i64>| {
        for (viewer, status_id) in pairs {
            if let Some(entry) = flags.get_mut(&viewer) {
                pick(entry).insert(status_id);
            }
        }
    };
    fold(&mut flags, favourited, |f| &mut f.favourited);
    fold(&mut flags, disliked, |f| &mut f.disliked);
    fold(&mut flags, reblogged, |f| &mut f.reblogged);
    fold(&mut flags, bookmarked, |f| &mut f.bookmarked);
    fold(&mut flags, pinned, |f| &mut f.pinned);
    fold(&mut flags, muted, |f| &mut f.muted);
    Ok(flags)
}

async fn application_maps(
    pool: &PgPool,
    items: &[Status],
    targets: &HashMap<i64, Status>,
    author_rows: &HashMap<i64, Account>,
) -> Result<(HashMap<i64, Value>, HashMap<i64, bool>), ApiError> {
    let author_ids: Vec<i64> = author_rows.keys().copied().collect();
    let show_application = user::show_application_by_account_ids(pool, &author_ids).await?;

    // Every distinct client application on the page in one query; ids whose
    // app row is gone are simply absent (the entity omits `application`).
    let mut app_ids: Vec<i64> = items
        .iter()
        .chain(targets.values())
        .filter_map(|s| s.application_id)
        .collect();
    app_ids.sort_unstable();
    app_ids.dedup();
    let applications = oauth::find_apps_by_ids(pool, &app_ids)
        .await?
        .iter()
        .map(|app| (app.id, status_application_json(app)))
        .collect();

    Ok((applications, show_application))
}

async fn media_maps(
    pool: &PgPool,
    domain: &str,
    all_ids: &[i64],
    allow_direct: bool,
) -> Result<(HashMap<i64, Vec<Value>>, HashMap<i64, Vec<String>>), ApiError> {
    let mut attachments: HashMap<i64, Vec<Value>> = HashMap::new();
    let mut descriptions: HashMap<i64, Vec<String>> = HashMap::new();
    let per_status = media::for_statuses(pool, all_ids).await?;
    // The HLS rendition ladders for every attachment in the batch (empty for
    // non-HLS media) — one query, then attached per attachment below.
    let media_ids: Vec<i64> = per_status.values().flatten().map(|f| f.id).collect();
    // Skip the round-trip entirely on the common attachment-less batch.
    let ladders = if media_ids.is_empty() {
        HashMap::new()
    } else {
        media::renditions_for(pool, &media_ids).await?
    };
    for (status_id, files) in per_status {
        descriptions.insert(
            status_id,
            files
                .iter()
                .map(|file| file.description.clone().unwrap_or_default())
                .collect(),
        );
        attachments.insert(
            status_id,
            files
                .iter()
                .map(|file| {
                    media_json_hls(
                        domain,
                        file,
                        allow_direct,
                        ladders.get(&file.id).map(Vec::as_slice),
                    )
                })
                .collect(),
        );
    }
    Ok((attachments, descriptions))
}

/// Reply-parent author account ids for a batch, keyed by parent status id —
/// what `in_reply_to_account_id` reads. Parents that no longer resolve are
/// simply absent (the field stays null).
async fn reply_parent_map(
    pool: &PgPool,
    items: &[Status],
    targets: &HashMap<i64, Status>,
) -> Result<HashMap<i64, i64>, ApiError> {
    let parent_ids: Vec<i64> = items
        .iter()
        .chain(targets.values())
        .filter_map(|s| s.in_reply_to_id)
        .collect();
    let mut reply_parents = HashMap::new();
    if !parent_ids.is_empty() {
        for parent in status::find_by_ids(pool, &parent_ids).await? {
            reply_parents.insert(parent.id, parent.account_id);
        }
    }
    Ok(reply_parents)
}

/// The text a status' custom emoji are scanned out of: spoiler, body and
/// (if it has one) its poll's options — Mastodon's `Status#emojis` source.
fn emoji_source_texts<'a>(item: &'a Status, poll_rows: &'a HashMap<i64, Poll>) -> Vec<&'a str> {
    let mut texts: Vec<&str> = vec![&item.spoiler_text, &item.content];
    if let Some(poll) = poll_rows.get(&item.id) {
        texts.extend(poll.options.iter().map(String::as_str));
    }
    texts
}

/// The page's loaded custom-emoji rows, keyed by author domain then
/// shortcode — the single `custom_emoji::lookup_many` result the status,
/// poll and profile renders all select from.
type EmojiTable = HashMap<(Option<String>, Option<i64>), HashMap<String, CustomEmoji>>;

/// Loads every `(author domain, shortcode)` pair in one query and buckets
/// the rows by domain. `requests` yields one `(domain, codes)` entry per
/// domain bucket the caller scanned; duplicate pairs are fine.
async fn load_emoji_table(
    pool: &PgPool,
    requests: &HashMap<(Option<&str>, Option<i64>), Vec<String>>,
) -> Result<EmojiTable, ApiError> {
    let mut codes: Vec<String> = Vec::new();
    let mut domains: Vec<Option<String>> = Vec::new();
    let mut owners: Vec<Option<i64>> = Vec::new();
    for (&(domain_key, owner_id), bucket) in requests {
        for code in bucket {
            codes.push(code.clone());
            domains.push(domain_key.map(str::to_owned));
            owners.push(owner_id);
        }
    }
    let mut table: EmojiTable = HashMap::new();
    for requested in custom_emoji::lookup_many_for_authors(pool, &codes, &domains, &owners).await? {
        let key = (
            requested.request_domain.clone(),
            requested.request_owner_account_id,
        );
        let emoji = requested.into_emoji();
        table
            .entry(key)
            .or_default()
            .insert(emoji.shortcode.clone(), emoji);
    }
    Ok(table)
}

/// The `CustomEmoji` entities referenced by `texts`, selected out of a
/// preloaded [`EmojiTable`] bucket in the order the texts mention them —
/// the batched counterpart of [`crate::emoji::emojis_json`].
fn emojis_from_table(
    domain: &str,
    by_shortcode: Option<&HashMap<String, CustomEmoji>>,
    texts: &[&str],
    allow_direct: bool,
) -> Vec<Value> {
    let mut seen: Vec<&str> = Vec::new();
    let mut rendered = Vec::new();
    for code in texts
        .iter()
        .flat_map(|text| plamenu_ap::emoji::scan_shortcodes(text))
    {
        if seen.contains(&code) {
            continue;
        }
        seen.push(code);
        if let Some(emoji) = by_shortcode.and_then(|m| m.get(code)) {
            rendered.push(crate::emoji::custom_emoji_json(domain, emoji, allow_direct));
        }
    }
    rendered
}

/// Custom emoji re-scanned out of each status' text (spoiler, body, poll
/// options) and resolved against the author's domain, like Mastodon's
/// `Status#emojis`. The lookup is one query for the whole page regardless of
/// how many author domains contribute (`custom_emoji::lookup_many` over the
/// `(domain, shortcode)` pairs); statuses without any `:shortcode:`
/// reference never contribute to the lookup set. Also returns the loaded
/// [`EmojiTable`] so the poll renderer can select its options' emoji from
/// the same rows without a query of its own.
async fn emoji_map(
    pool: &PgPool,
    domain: &str,
    items: &[Status],
    targets: &HashMap<i64, Status>,
    author_rows: &HashMap<i64, Account>,
    poll_rows: &HashMap<i64, Poll>,
    allow_direct: bool,
) -> Result<(HashMap<i64, Vec<Value>>, EmojiTable), ApiError> {
    let mut shortcodes_by_domain: HashMap<(Option<&str>, Option<i64>), Vec<String>> =
        HashMap::new();
    for item in items.iter().chain(targets.values()) {
        let author = &author_rows[&item.account_id];
        for code in emoji_source_texts(item, poll_rows)
            .iter()
            .flat_map(|text| plamenu_ap::emoji::scan_shortcodes(text))
        {
            let codes = shortcodes_by_domain
                .entry((
                    author.domain.as_deref(),
                    author.domain.is_none().then_some(author.id),
                ))
                .or_default();
            if !codes.iter().any(|c| c == code) {
                codes.push(code.to_owned());
            }
        }
    }
    let table = load_emoji_table(pool, &shortcodes_by_domain).await?;
    let mut emojis: HashMap<i64, Vec<Value>> = HashMap::new();
    for item in items.iter().chain(targets.values()) {
        let author = &author_rows[&item.account_id];
        let rendered = emojis_from_table(
            domain,
            table.get(&(
                author.domain.clone(),
                author.domain.is_none().then_some(author.id),
            )),
            &emoji_source_texts(item, poll_rows),
            allow_direct,
        );
        if !rendered.is_empty() {
            emojis.insert(item.id, rendered);
        }
    }
    Ok((emojis, table))
}

/// One viewer's author view out of the shared account pass: the rendered
/// entities (viewer-relative `feature_approval` and `moved`) and the viewer's
/// accepted-follow edges against the batch.
struct AuthorView {
    entities: HashMap<i64, Value>,
    relations: FeatureRelations,
}

/// Builds the author `Account` rows for a status page plus every viewer's
/// rendered author entities. All facts are resolved in a fixed number of
/// queries regardless of author count *and* viewer count (N+1 decisions):
/// counters, overlays, silences and profile emoji are shared; the
/// accepted-follow edges — the only per-viewer fact — load as one batched
/// pair of queries across the whole viewer set. Migration targets join the
/// same pass (they used to be a second `render_accounts_inner` round), so a
/// moved author costs no per-viewer round trips either.
async fn author_pass_for_viewer_set(
    pool: &PgPool,
    domain: &str,
    items: &[Status],
    targets: &HashMap<i64, Status>,
    group_attributions: &HashMap<i64, Vec<i64>>,
    set: &[Option<i64>],
    allow_direct_of: &HashMap<Option<i64>, bool>,
) -> Result<(HashMap<i64, Account>, HashMap<Option<i64>, AuthorView>), ApiError> {
    let (author_rows, movers, moved_rows, union_ids) =
        author_batch_rows(pool, items, targets, group_attributions).await?;
    let signed_in: Vec<i64> = set.iter().copied().flatten().collect();
    let (author_counts, overlays, silenced_domains, edges_out, edges_in) = tokio::try_join!(
        account_counts_batch(pool, &union_ids),
        async {
            user::account_entity_overlay_batch(pool, &union_ids)
                .await
                .map_err(ApiError::from)
        },
        silenced_domain_set(pool),
        async {
            if signed_in.is_empty() {
                return Ok(HashSet::new());
            }
            follow::accepted_out_edges(pool, &signed_in, &union_ids)
                .await
                .map_err(ApiError::from)
        },
        async {
            if signed_in.is_empty() {
                return Ok(HashSet::new());
            }
            follow::accepted_in_edges(pool, &signed_in, &union_ids)
                .await
                .map_err(ApiError::from)
        },
    )?;
    // Profile emoji per distinct direct-media preference among the viewers (at
    // most two lookups, usually one).
    let mut variant_values: Vec<bool> = allow_direct_of.values().copied().collect();
    variant_values.sort_unstable();
    variant_values.dedup();
    let mut emoji_variants: HashMap<bool, HashMap<i64, Vec<Value>>> = HashMap::new();
    for &allow_direct in &variant_values {
        emoji_variants.insert(
            allow_direct,
            profile_emoji_map(
                pool,
                domain,
                author_rows.values().chain(moved_rows.iter()),
                allow_direct,
            )
            .await?,
        );
    }

    let shared = AuthorPassShared {
        author_rows,
        movers,
        moved_rows,
        counts: author_counts,
        overlays,
        silenced_domains,
    };
    let mut views: HashMap<Option<i64>, AuthorView> = HashMap::with_capacity(set.len());
    for viewer in set {
        let relations = match viewer {
            Some(viewer_id) => FeatureRelations {
                follows_out: edges_out
                    .iter()
                    .filter(|(from, _)| from == viewer_id)
                    .map(|(_, to)| *to)
                    .collect(),
                followed_by: edges_in
                    .iter()
                    .filter(|(to, _)| to == viewer_id)
                    .map(|(_, from)| *from)
                    .collect(),
            },
            None => FeatureRelations::default(),
        };
        let allow_direct = allow_direct_of.get(viewer).copied().unwrap_or(false);
        let emojis = &emoji_variants[&allow_direct];
        let view = author_view_for_viewer(
            pool,
            domain,
            &shared,
            *viewer,
            relations,
            allow_direct,
            emojis,
        )
        .await?;
        views.insert(*viewer, view);
    }
    Ok((shared.author_rows, views))
}

/// The account rows of one status page: the distinct authors (statuses,
/// reblog targets, attributing communities) and their migration targets,
/// fetched in one query each. (Previously one `find_by_id` per distinct
/// author — a residual N+1 on the hottest path in the app.) Including groups
/// here lets the Status extension reuse the same batched Account rendering
/// rather than starting a second account pass; migration targets fold into
/// the same counters/overlay/edge batches, so a chained `movedTo` cannot
/// recurse and a moved author costs no extra render round.
type AuthorBatchRows = (
    HashMap<i64, Account>,
    Vec<(i64, String)>,
    Vec<Account>,
    Vec<i64>,
);

async fn author_batch_rows(
    pool: &PgPool,
    items: &[Status],
    targets: &HashMap<i64, Status>,
    group_attributions: &HashMap<i64, Vec<i64>>,
) -> Result<AuthorBatchRows, ApiError> {
    let mut author_ids: Vec<i64> = items
        .iter()
        .map(|s| s.account_id)
        .chain(targets.values().map(|s| s.account_id))
        .chain(
            group_attributions
                .values()
                .flat_map(|group_ids| group_ids.iter().copied()),
        )
        .collect();
    author_ids.sort_unstable();
    author_ids.dedup();
    let author_rows: HashMap<i64, Account> = account::find_by_ids(pool, &author_ids)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    // Every author referenced by a status must exist (FK); a gap is a data
    // integrity error, matching the old per-row `ok_or(NotFound)`.
    if author_rows.len() != author_ids.len() {
        return Err(ApiError::NotFound);
    }
    let movers = movers_of(author_rows.values());
    let moved_rows: Vec<Account> = if movers.is_empty() {
        Vec::new()
    } else {
        let mut uris: Vec<&str> = movers.iter().map(|(_, uri)| uri.as_str()).collect();
        uris.sort_unstable();
        uris.dedup();
        account::find_by_uris(pool, &uris).await?
    };
    let mut union_ids = author_ids;
    union_ids.extend(moved_rows.iter().map(|target| target.id));
    union_ids.sort_unstable();
    union_ids.dedup();
    Ok((author_rows, movers, moved_rows, union_ids))
}

/// The shared facts of the viewer-set account pass, loaded once for the set.
struct AuthorPassShared {
    author_rows: HashMap<i64, Account>,
    /// `(account id, moved_to_uri)` pairs of the migrated authors.
    movers: Vec<(i64, String)>,
    /// The resolved migration-target rows.
    moved_rows: Vec<Account>,
    counts: HashMap<i64, AccountCounts>,
    overlays: HashMap<i64, user::AccountEntityOverlay>,
    silenced_domains: HashSet<String>,
}

/// One viewer's rendered author entities out of the shared account pass —
/// query-free: every fact comes prefetched, only `feature_approval` and the
/// `moved` attachment are computed against this viewer's relations.
async fn author_view_for_viewer(
    pool: &PgPool,
    domain: &str,
    shared: &AuthorPassShared,
    viewer: Option<i64>,
    relations: FeatureRelations,
    allow_direct: bool,
    emojis: &HashMap<i64, Vec<Value>>,
) -> Result<AuthorView, ApiError> {
    // The migration targets render first, with no `moved` of their own — the
    // same shape the nested `render_accounts_inner` pass produced.
    let empty_moved: HashMap<i64, Value> = HashMap::new();
    let mut target_entities: HashMap<i64, Value> = HashMap::new();
    if !shared.moved_rows.is_empty() {
        let ctx = AccountRenderCtx::Batched {
            relations: &relations,
            overlays: &shared.overlays,
            silenced_domains: &shared.silenced_domains,
            moved: &empty_moved,
            emojis,
        };
        for target in &shared.moved_rows {
            let counts = shared.counts.get(&target.id).copied().unwrap_or_default();
            let entity =
                account_json_with_counts(pool, domain, target, viewer, &counts, allow_direct, &ctx)
                    .await?;
            target_entities.insert(target.id, entity);
        }
    }
    let by_uri: HashMap<&str, &Value> = shared
        .moved_rows
        .iter()
        .filter_map(|target| target.uri.as_deref().zip(target_entities.get(&target.id)))
        .collect();
    let moved: HashMap<i64, Value> = shared
        .movers
        .iter()
        .filter_map(|(id, uri)| {
            by_uri
                .get(uri.as_str())
                .map(|entity| (*id, (*entity).clone()))
        })
        .collect();
    let ctx = AccountRenderCtx::Batched {
        relations: &relations,
        overlays: &shared.overlays,
        silenced_domains: &shared.silenced_domains,
        moved: &moved,
        emojis,
    };
    let mut entities: HashMap<i64, Value> = HashMap::with_capacity(shared.author_rows.len());
    for (account_id, author) in &shared.author_rows {
        let counts = shared.counts.get(account_id).copied().unwrap_or_default();
        let entity =
            account_json_with_counts(pool, domain, author, viewer, &counts, allow_direct, &ctx)
                .await?;
        entities.insert(*account_id, entity);
    }
    Ok(AuthorView {
        entities,
        relations,
    })
}

async fn render_statuses_inner(
    pool: &PgPool,
    domain: &str,
    items: &[Status],
    viewer: Option<i64>,
    depth: u8,
) -> Result<Vec<Value>, ApiError> {
    // The singular render *is* a one-viewer set — there is no second
    // implementation to drift from the batched one (N+1 decisions).
    let mut per_viewer =
        render_statuses_for_viewer_set(pool, domain, items, std::slice::from_ref(&viewer), depth)
            .await?;
    per_viewer.remove(&viewer).ok_or(ApiError::NotFound)
}

/// The two-layer batched renderer (N+1 decisions): the viewer-independent
/// maps are computed once for the whole batch, and every viewer-dependent
/// dimension resolves as one batched query across the entire viewer set — so
/// a streaming fan-out to hundreds of connected viewers costs the same number
/// of statements as a single-viewer page render. Each viewer's documents are
/// byte-identical to a singular [`render_statuses`] call for that viewer
/// (the singular path is a one-viewer set of this same function).
#[allow(clippy::too_many_lines)] // one batched pass building every status map
async fn render_statuses_for_viewer_set(
    pool: &PgPool,
    domain: &str,
    items: &[Status],
    set: &[Option<i64>],
    depth: u8,
) -> Result<HashMap<Option<i64>, Vec<Value>>, ApiError> {
    let mut set: Vec<Option<i64>> = set.to_vec();
    set.sort_unstable();
    set.dedup();
    if set.is_empty() {
        return Ok(HashMap::new());
    }
    let signed_in: Vec<i64> = set.iter().copied().flatten().collect();

    // Resolve boost targets first; they join the working set. Fetched in one
    // query over the distinct target ids (a timeline page can be mostly boosts,
    // so a per-boost `find_by_id` was a residual N+1 on this hot path).
    let target_ids: Vec<i64> = items.iter().filter_map(|s| s.reblog_of_id).collect();
    let targets: HashMap<i64, Status> = if target_ids.is_empty() {
        HashMap::new()
    } else {
        status::find_by_ids(pool, &target_ids)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect()
    };
    // A boost whose target no longer resolves is a data-integrity gap, matching
    // the old per-row `ok_or(NotFound)`.
    if items
        .iter()
        .filter_map(|s| s.reblog_of_id)
        .any(|id| !targets.contains_key(&id))
    {
        return Err(ApiError::NotFound);
    }

    let mut all_ids: Vec<i64> = items.iter().map(|s| s.id).collect();
    all_ids.extend(targets.keys().copied());
    // Independent reads run concurrently: this step costs the
    // slowest round trip, not the sum of all of them.
    let (engagement, mut flags_by_viewer, group_attribution_rows, group_locked_rows, direct_optins) =
        tokio::try_join!(
            async {
                status::engagement_for(pool, &all_ids)
                    .await
                    .map_err(ApiError::from)
            },
            viewer_flags_for_set(pool, &signed_in, &all_ids),
            async {
                plamenu_db::group::group_attributions_of(pool, &all_ids)
                    .await
                    .map_err(ApiError::from)
            },
            async {
                plamenu_db::group::locked_of(pool, &all_ids)
                    .await
                    .map_err(ApiError::from)
            },
            async {
                // Each viewer's direct-remote-media preference, resolved once for
                // the set and baked into that viewer's proxy URLs. Fails
                // closed to `false`, exactly like the singular resolver.
                if signed_in.is_empty() {
                    return Ok(HashSet::new());
                }
                Ok(user::allows_direct_remote_media_of(pool, &signed_in)
                    .await
                    .unwrap_or_default())
            },
        )?;
    let mut group_attributions: HashMap<i64, Vec<i64>> = HashMap::new();
    for (status_id, group_id) in group_attribution_rows {
        group_attributions
            .entry(status_id)
            .or_default()
            .push(group_id);
    }
    let group_attributed: HashSet<i64> = group_attributions.keys().copied().collect();
    let group_locked: HashSet<i64> = group_locked_rows.into_iter().collect();
    let allow_direct_of: HashMap<Option<i64>, bool> = set
        .iter()
        .map(|viewer| {
            (
                *viewer,
                viewer.is_some_and(|id| direct_optins.contains(&id)),
            )
        })
        .collect();
    let mut variant_values: Vec<bool> = allow_direct_of.values().copied().collect();
    variant_values.sort_unstable();
    variant_values.dedup();

    // One author `Account` row per distinct account, with every viewer's
    // rendered entities and accepted-follow edges batched across the set (see
    // [`author_pass_for_viewer_set`]) — no per-author and no per-viewer round
    // trips.
    let (author_rows, mut author_views) = author_pass_for_viewer_set(
        pool,
        domain,
        items,
        &targets,
        &group_attributions,
        &set,
        &allow_direct_of,
    )
    .await?;
    let (applications, show_application) =
        application_maps(pool, items, &targets, &author_rows).await?;

    // The per-artifact maps are independent of one another: issue them
    // concurrently so a page's latency approaches the slowest artifact rather
    // than the sum of every round trip. The direct-media-
    // dependent maps run once per distinct opt-in value among the viewers (at
    // most twice, usually once) — never per viewer.
    let (
        mut media_variants,
        reply_parents,
        (mentions, tags),
        tagged_shared,
        quote_shared,
        mut card_variants,
        (poll_rows, mut emoji_variants),
        (reactions, mut reaction_url_variants),
        deleted,
        unresolved_parent_uris,
    ) = tokio::try_join!(
        async {
            let mut variants = HashMap::new();
            for &allow_direct in &variant_values {
                variants.insert(
                    allow_direct,
                    media_maps(pool, domain, &all_ids, allow_direct).await?,
                );
            }
            Ok(variants)
        },
        reply_parent_map(pool, items, &targets),
        mention_and_tag_maps(pool, domain, &all_ids),
        tagged_collections_shared(pool, domain, &all_ids),
        quote_shared_fetch(pool, &all_ids),
        async {
            let mut variants = HashMap::new();
            for &allow_direct in &variant_values {
                variants.insert(
                    allow_direct,
                    card_map(pool, domain, &all_ids, allow_direct).await?,
                );
            }
            Ok(variants)
        },
        async {
            // One page-wide emoji lookup per opt-in variant; polls select
            // their options' emoji from the same loaded table, so they cost
            // no query of their own.
            let poll_rows = poll::for_statuses(pool, &all_ids)
                .await
                .map_err(ApiError::from)?;
            let mut variants = HashMap::new();
            for &allow_direct in &variant_values {
                variants.insert(
                    allow_direct,
                    emoji_map(
                        pool,
                        domain,
                        items,
                        &targets,
                        &author_rows,
                        &poll_rows,
                        allow_direct,
                    )
                    .await?,
                );
            }
            Ok((poll_rows, variants))
        },
        async {
            let reactions = reaction::for_statuses(pool, &all_ids).await?;
            let mut variants = HashMap::new();
            for &allow_direct in &variant_values {
                variants.insert(
                    allow_direct,
                    reaction_emoji_url_map(pool, domain, allow_direct, &reactions).await?,
                );
            }
            Ok((reactions, variants))
        },
        async {
            status::deleted_ids(pool, &all_ids)
                .await
                .map_err(ApiError::from)
        },
        async {
            Ok(status::unresolved_reply_parents(pool, &all_ids)
                .await?
                .into_iter()
                .collect::<HashMap<i64, String>>())
        },
    )?;

    // The remaining per-viewer dimensions, each one batched query across the
    // whole set, issued concurrently. The quoted statuses render through one
    // recursive viewer-set pass for the whole page, depth-capped as before.
    let poll_ids: Vec<i64> = poll_rows.values().map(|p| p.id).collect();
    let (
        votes_rows,
        mut quoted_by_viewer,
        mut viewer_items,
        mut anon_items,
        filters_by_viewer,
        event_states_ctx,
    ) = tokio::try_join!(
        async {
            if signed_in.is_empty() || poll_ids.is_empty() {
                return Ok(HashMap::new());
            }
            poll::votes_by_for_polls_viewers(pool, &signed_in, &poll_ids)
                .await
                .map_err(ApiError::from)
        },
        async {
            if depth == 0 && !quote_shared.quoted_batch.is_empty() {
                Box::pin(render_statuses_for_viewer_set(
                    pool,
                    domain,
                    &quote_shared.quoted_batch,
                    &set,
                    depth + 1,
                ))
                .await
            } else {
                Ok(HashMap::new())
            }
        },
        async {
            if signed_in.is_empty() || tagged_shared.collection_ids.is_empty() {
                return Ok(HashMap::new());
            }
            collection::items_for_many_viewers(pool, &tagged_shared.collection_ids, &signed_in)
                .await
                .map_err(ApiError::from)
        },
        async {
            if set.contains(&None) && !tagged_shared.collection_ids.is_empty() {
                collection::items_for_many(pool, &tagged_shared.collection_ids, None)
                    .await
                    .map_err(ApiError::from)
            } else {
                Ok(HashMap::new())
            }
        },
        crate::filters::compiled_filters_for_set(pool, &signed_in),
        async {
            let Some(ctx) = event_context(pool, items, &targets, &all_ids).await? else {
                return Ok(None);
            };
            let states = if signed_in.is_empty() {
                HashMap::new()
            } else {
                let event_ids: Vec<i64> = ctx.rows.keys().copied().collect();
                plamenu_db::status_participation::states_for_viewers(pool, &signed_in, &event_ids)
                    .await?
            };
            Ok(Some((ctx, states)))
        },
    )?;
    let mut votes_by_viewer: HashMap<i64, HashMap<i64, Vec<i32>>> = HashMap::new();
    for ((viewer_id, poll_id), votes) in votes_rows {
        votes_by_viewer
            .entry(viewer_id)
            .or_default()
            .insert(poll_id, votes);
    }
    let media_descriptions: HashMap<i64, Vec<String>> = media_variants
        .values()
        .next()
        .map(|(_, descriptions)| descriptions.clone())
        .unwrap_or_default();
    // The searchable text of each item is viewer-independent: one precompute
    // serves every viewer's filter pass.
    let searchables = filter_searchables(items, &targets, &poll_rows, &media_descriptions);
    let accepted_quote_links = quote_shared
        .rows
        .iter()
        .filter(|(_, row)| row.state == "accepted")
        .filter_map(|(status_id, row)| {
            let mut links = Vec::with_capacity(3);
            if let Some(uri) = &row.quoted_uri {
                links.push(uri.clone());
            }
            if let Some(target) = row
                .quoted_status_id
                .and_then(|quoted_id| quote_shared.quoted_by_id.get(&quoted_id))
            {
                links.extend(target.uri.iter().cloned());
                links.extend(target.url.iter().cloned());
            }
            links.sort_unstable();
            links.dedup();
            (!links.is_empty()).then_some((*status_id, links))
        })
        .collect();

    let webxdc_invitations = plamenu_db::webxdc::invitation_cards(pool, &all_ids)
        .await?
        .into_iter()
        .map(|(status, (id, uri, name))| {
            (status, crate::webxdc::invitation_entity(id, &uri, &name))
        })
        .collect();
    let shared = SharedMaps {
        webxdc_invitations,
        targets,
        author_rows,
        engagement,
        group_attributed,
        group_attributions,
        group_locked,
        reply_parents,
        mentions,
        tags,
        reactions,
        accepted_quote_links,
        applications,
        show_application,
        deleted,
        unresolved_parent_uris,
    };
    let mut variants: HashMap<bool, VariantMaps> = HashMap::new();
    for &allow_direct in &variant_values {
        let (attachments, _descriptions) = media_variants.remove(&allow_direct).unwrap_or_default();
        let (emojis, emoji_table) = emoji_variants.remove(&allow_direct).unwrap_or_default();
        variants.insert(
            allow_direct,
            VariantMaps {
                attachments,
                cards: card_variants.remove(&allow_direct).unwrap_or_default(),
                emojis,
                emoji_table,
                reaction_emoji_urls: reaction_url_variants
                    .remove(&allow_direct)
                    .unwrap_or_default(),
            },
        );
    }

    // Per-viewer assembly: pure in-memory work from here on — every map is a
    // lookup into the prefetched shared, variant and per-viewer data.
    let mut out: HashMap<Option<i64>, Vec<Value>> = HashMap::with_capacity(set.len());
    for viewer in &set {
        let allow_direct = allow_direct_of.get(viewer).copied().unwrap_or(false);
        let variant = variants.get(&allow_direct).ok_or(ApiError::NotFound)?;
        let view = author_views.remove(viewer).ok_or(ApiError::NotFound)?;
        let flags = viewer
            .and_then(|id| flags_by_viewer.remove(&id))
            .unwrap_or_default();
        let quote_values = quoted_by_viewer.remove(viewer).unwrap_or_default();
        let own_votes = viewer
            .and_then(|id| votes_by_viewer.remove(&id))
            .unwrap_or_default();
        let filtered = match viewer.and_then(|id| filters_by_viewer.get(&id)) {
            Some(compiled) => assemble_filtered(compiled, &searchables)?,
            None => HashMap::new(),
        };
        let collection_items: HashMap<i64, Vec<plamenu_db::collection::CollectionItem>> =
            match viewer {
                Some(viewer_id) => tagged_shared
                    .collection_ids
                    .iter()
                    .filter_map(|&collection_id| {
                        viewer_items
                            .remove(&(*viewer_id, collection_id))
                            .map(|members| (collection_id, members))
                    })
                    .collect(),
                None => std::mem::take(&mut anon_items),
            };
        let events = match &event_states_ctx {
            Some((ctx, states)) => assemble_event_map(ctx, *viewer, states)?,
            None => HashMap::new(),
        };
        let viewer_maps = ViewerMaps {
            viewer: *viewer,
            author_entities: view.entities,
            favourited: flags.favourited,
            disliked: flags.disliked,
            reblogged: flags.reblogged,
            bookmarked: flags.bookmarked,
            pinned: flags.pinned,
            muted: flags.muted,
            quote_followed_authors: followed_authors(*viewer, &view.relations, &shared.author_rows),
            quote_following_authors: following_authors(
                *viewer,
                &view.relations,
                &shared.author_rows,
            ),
            tagged_collections: assemble_tagged_collections(
                domain,
                &tagged_shared,
                &collection_items,
            )?,
            quotes: assemble_quote_map(&quote_shared, &quote_values, depth),
            polls: assemble_poll_map(
                domain,
                &poll_rows,
                &shared.author_rows,
                &variant.emoji_table,
                *viewer,
                &own_votes,
                allow_direct,
            )?,
            filtered,
            events,
        };
        let maps = RenderMaps {
            shared: &shared,
            variant,
            viewer_maps: &viewer_maps,
        };
        let rendered = items
            .iter()
            .map(|item| render_one(domain, item, &maps))
            .collect::<Result<Vec<Value>, ApiError>>()?;
        out.insert(*viewer, rendered);
    }
    Ok(out)
}

/// The rendered `event` extension objects for a status batch: the typed
/// sidecar of statuses ingested from `Event` objects, `null`-free — statuses
/// without a sidecar simply carry `"event": null`.
///
/// Mastodon has no event API at all, so every key here is a Plamenu extension
/// nested under the one allowlisted `event` object — which is deliberate: it
/// keeps the whole calendar surface out of the top-level `Status` shape that
/// `test_shape_parity` polices.
///
/// A field the origin never sent stays `null` rather than becoming a default.
/// `join_mode` is the one that matters most: absent means "this dialect does
/// not model join modes", and a client must not read that as `free` and offer
/// an RSVP button that generates a `Join` nobody answers.
///
/// Two keys are viewer-scoped: `participation` (this viewer's own RSVP state, or
/// `null`) and `can_participate`/`participation_refusal`, which say whether an
/// RSVP is offered at all and, when not, why — "invite only", "full" and
/// "happens elsewhere" send a viewer to completely different next actions, so
/// they must not collapse into one disabled button.
///
/// `participants_count` switches source with the event's home: our own accepted
/// rows for an event we host, the origin's number for one we don't. See
/// `events` for why those are never mixed.
/// The viewer-independent half of the event-sidecar render: the sidecar rows,
/// which events are locally hosted, and the local accepted counts. `None`
/// when the batch carries no `Event`-typed status at all (the common case —
/// an ordinary timeline pays nothing here).
struct EventContext {
    rows: HashMap<i64, plamenu_db::status_event::StatusEvent>,
    local: HashSet<i64>,
    local_counts: HashMap<i64, i64>,
}

async fn event_context(
    pool: &PgPool,
    items: &[Status],
    targets: &HashMap<i64, Status>,
    status_ids: &[i64],
) -> Result<Option<EventContext>, ApiError> {
    // Events are a small minority of statuses and `object_type` is already loaded,
    // so an ordinary timeline pays nothing here. Without this the sidecar query
    // ran on every render of every endpoint in `budgets.toml`.
    //
    // Relies on the invariant that a status carrying an event sidecar is typed
    // `Event`: both writers (local authoring in `actions::post_status`, inbound
    // ingest in `ingest::store_event_sidecar`) set the column, so a sidecar
    // without it cannot arise outside a test that builds the row by hand.
    let any_event = |item: &Status| item.object_type.as_deref() == Some("Event");
    if !items.iter().any(|item| {
        any_event(item)
            || item
                .reblog_of_id
                .and_then(|id| targets.get(&id))
                .is_some_and(any_event)
    }) {
        return Ok(None);
    }
    let rows = plamenu_db::status_event::for_statuses(pool, status_ids).await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let event_ids: Vec<i64> = rows.keys().copied().collect();
    let local: HashSet<i64> =
        plamenu_db::status_participation::locally_authored_of(pool, &event_ids)
            .await?
            .into_iter()
            .collect();
    let local_ids: Vec<i64> = local.iter().copied().collect();
    let local_counts = plamenu_db::status_participation::accepted_counts(pool, &local_ids).await?;
    Ok(Some(EventContext {
        rows,
        local,
        local_counts,
    }))
}

/// One viewer's rendered event sidecars, assembled from the shared
/// [`EventContext`] and the set-wide prefetched RSVP states — no queries.
fn assemble_event_map(
    ctx: &EventContext,
    viewer: Option<i64>,
    states: &HashMap<(i64, i64), plamenu_db::status_participation::Participation>,
) -> Result<HashMap<i64, Value>, ApiError> {
    let EventContext {
        rows,
        local,
        local_counts,
    } = ctx;
    let mut events = HashMap::new();
    for (status_id, row) in rows {
        let status_id = *status_id;
        let row = row.clone();
        let my_state = viewer
            .and_then(|viewer_id| states.get(&(viewer_id, status_id)))
            .map(|p| p.state);
        let invited = my_state == Some(plamenu_db::status_participation::State::Invited);
        let refusal = crate::events::rsvp_refusal(&row, invited);
        // An event we host counts from our own rows (authoritative); a remote one
        // reports the origin's number.
        let count = if local.contains(&status_id) {
            Some(local_counts.get(&status_id).copied().unwrap_or(0))
        } else {
            row.participant_count.map(i64::from)
        };
        events.insert(
            status_id,
            json!({
                "participation": my_state.map(plamenu_db::status_participation::State::as_str),
                // Anonymous viewers get `false`: there is no account to RSVP with,
                // and a button that leads to a login prompt is not a can-do.
                "can_participate": viewer.is_some() && refusal.is_none(),
                "participation_refusal": refusal.map(crate::events::RsvpRefusal::as_str),
                "start_time": row.start_time.map(rfc3339).transpose()?,
                "end_time": row.end_time.map(rfc3339).transpose()?,
                "location": row.location_name,
                "timezone": row.timezone,
                "status": row.event_status,
                "join_mode": row.join_mode,
                // Our own accepted rows for an event we host; the origin's number
                // for one we don't, where we see only the participation
                // activities addressed to us and counting locally would
                // systematically undercount.
                "participants_count": count,
                "max_attendees": row.max_attendees,
                "remaining_attendees": row.remaining_attendees,
                "external_participation_url": row.external_participation_url,
                "anonymous_participation": row.anonymous_participation,
                "is_online": row.is_online,
                "comments_enabled": row.comments_enabled,
                "category": row.category,
                "location_url": row.location_url,
                "location_street": row.location_street,
                "location_locality": row.location_locality,
                "location_region": row.location_region,
                "location_country": row.location_country,
                "location_postal_code": row.location_postal_code,
            }),
        );
    }
    Ok(events)
}

/// One item's precompiled filter-matching input — the searchable text and the
/// ids a match keys to. Viewer-independent, so the whole viewer set shares
/// one precompute pass.
struct FilterSearchable {
    item_id: i64,
    target_id: Option<i64>,
    searchable: String,
    match_ids: Vec<i64>,
}

/// The searchable text of each item for the `filtered` pass. A boost is
/// matched on its target's text but keyed to both the boost and target ids,
/// like Mastodon's `build_filters_map`.
fn filter_searchables(
    items: &[Status],
    targets: &HashMap<i64, Status>,
    poll_rows: &HashMap<i64, Poll>,
    media_descriptions: &HashMap<i64, Vec<String>>,
) -> Vec<FilterSearchable> {
    items
        .iter()
        .map(|item| {
            // The "proper" status (the boost target, or the status itself)
            // supplies the searchable text; pinned-status matching checks the
            // boost and the target id, in that order.
            let (proper, match_ids) = match item.reblog_of_id {
                Some(target_id) => (&targets[&target_id], vec![item.id, target_id]),
                None => (item, vec![item.id]),
            };
            let options = poll_rows.get(&proper.id).map_or(&[][..], |p| &p.options);
            let descriptions: Vec<&str> = media_descriptions
                .get(&proper.id)
                .map(|d| d.iter().map(String::as_str).collect())
                .unwrap_or_default();
            let searchable = crate::filters::searchable_text(
                &proper.spoiler_text,
                &proper.content,
                options,
                &descriptions,
            );
            FilterSearchable {
                item_id: item.id,
                target_id: item.reblog_of_id,
                searchable,
                match_ids,
            }
        })
        .collect()
}

/// One viewer's `filtered` (`FilterResult` array) per status id, assembled
/// from that viewer's compiled filters and the shared searchable texts —
/// empty when the viewer has no active filter (so the attribute stays `[]`).
fn assemble_filtered(
    compiled: &[crate::filters::CompiledFilter],
    searchables: &[FilterSearchable],
) -> Result<HashMap<i64, Value>, ApiError> {
    if compiled.is_empty() {
        return Ok(HashMap::new());
    }
    let mut map = HashMap::new();
    for entry in searchables {
        let value = crate::filters::filtered_value(compiled, &entry.searchable, &entry.match_ids)?;
        map.insert(entry.item_id, value.clone());
        if let Some(target_id) = entry.target_id {
            map.insert(target_id, value);
        }
    }
    Ok(map)
}

type StatusValueMaps = (HashMap<i64, Vec<Value>>, HashMap<i64, Vec<Value>>);

/// Pre-rendered mention and tag entities for a batch of statuses.
async fn mention_and_tag_maps(
    pool: &PgPool,
    domain: &str,
    all_ids: &[i64],
) -> Result<StatusValueMaps, ApiError> {
    let mut mentions: HashMap<i64, Vec<Value>> = HashMap::new();
    for (status_id, mentioned) in mention::for_statuses(pool, all_ids, true).await? {
        mentions.insert(
            status_id,
            mentioned
                .iter()
                .map(|account| {
                    let acct = account_acct(domain, account);
                    // Mastodon's REST mention `url` is the account web page.
                    let url = account_web_url(domain, account);
                    json!({
                        "id": account.id.to_string(),
                        "username": account.username,
                        "url": url,
                        "acct": acct,
                    })
                })
                .collect(),
        );
    }
    let mut tags: HashMap<i64, Vec<Value>> = HashMap::new();
    for (status_id, names) in tag::names_for_statuses(pool, all_ids).await? {
        tags.insert(
            status_id,
            names
                .iter()
                .map(|name| shallow_tag_json(domain, name))
                .collect(),
        );
    }
    Ok((mentions, tags))
}

fn render_one(domain: &str, item: &Status, maps: &RenderMaps<'_>) -> Result<Value, ApiError> {
    if let Some(target_id) = item.reblog_of_id {
        // A boost: an empty wrapper with the target nested under `reblog`.
        let target = &maps.shared.targets[&target_id];
        let nested = render_plain(domain, target, maps)?;
        let booster = &maps.shared.author_rows[&item.account_id];
        let booster_entity = &maps.viewer_maps.author_entities[&item.account_id];
        let uri = item.uri.clone().unwrap_or_else(|| {
            format!(
                "{}/statuses/{}/activity",
                account_uri(domain, booster),
                item.id
            )
        });
        let mut entity = json!({
            "id": item.id.to_string(),
            "created_at": rfc3339(item.created_at)?,
            "edited_at": null,
            "in_reply_to_id": null,
            "in_reply_to_account_id": null,
            "sensitive": false,
            "spoiler_text": "",
            "visibility": item.visibility,
            "language": null,
            "uri": uri,
            "url": uri,
            "replies_count": 0,
            "reblogs_count": 0,
            "favourites_count": 0,
            "quotes_count": 0,
            // Viewer flags mirror the target, like Mastodon's boost wrappers.
            "favourited": maps.viewer_maps.favourited.contains(&target_id),
            "reblogged": maps.viewer_maps.reblogged.contains(&target_id),
            "muted": maps.viewer_maps.muted.contains(&target_id),
            "bookmarked": maps.viewer_maps.bookmarked.contains(&target_id),
            "content": "",
            "reblog": nested,
            "application": null,
            "account": null,
            "media_attachments": [],
            "mentions": [],
            "tags": [],
            "tagged_collections": [],
            "emojis": [],
            "card": null,
            "poll": null,
            // A boost never quotes, but Mastodon serializes the wrapper as a
            // full Status, so `quote`/`quote_approval` are present (like
            // `render_plain`) — omitting them is a shape divergence.
            "quote": null,
            "quote_approval": quote_approval(item, maps),
            // The extension fields ride on every Status shape; a boost
            // wrapper never carries them itself (the target does).
            "title": null,
            "object_type": null,
            "external_url": null,
            "event": null,
            // The group vote fields likewise live on the target, not the
            // wrapper; only the viewer flag mirrors, like `favourited`.
            "group_post": false,
            "groups": [],
            "downvotes_count": 0,
            "downvoted": maps.viewer_maps.disliked.contains(&target_id),
            "group_locked": maps.shared.group_locked.contains(&target_id),
        });
        entity["account"] = booster_entity.clone();
        apply_application(&mut entity, item, maps);
        if maps.viewer_maps.viewer.is_some() {
            entity["filtered"] = maps
                .viewer_maps
                .filtered
                .get(&item.id)
                .cloned()
                .unwrap_or_else(|| json!([]));
        }
        Ok(entity)
    } else {
        render_plain(domain, item, maps)
    }
}

/// Rendered community Account entities for a status, in stable account-id
/// order. All accounts were folded into the main author batch, so this adds no
/// queries even on a page of posts from many communities.
fn group_entities(status_id: i64, maps: &RenderMaps<'_>) -> Vec<Value> {
    maps.shared
        .group_attributions
        .get(&status_id)
        .into_iter()
        .flatten()
        .filter_map(|group_id| maps.viewer_maps.author_entities.get(group_id))
        .cloned()
        .collect()
}

/// The link glyph the web composer's `external_link_box` uses, inlined so a
/// folded link pill renders identically in the first-party client. Keep in
/// sync with `web::view::icon("link")`. A self-contained `<svg>` (no sprite
/// `<use>`): stock Mastodon clients strip it (and every `class`) on sanitize,
/// leaving a bold `<p>` + `<a>`, while our own client keeps the styling.
///
/// Explicit `width`/`height` are load-bearing: a client that strips the
/// `class` but keeps the `<svg>` (Phanpy's `enhanceContent` does exactly this)
/// would otherwise render an unsized replaced element at the browser default
/// (~300×150), which reads as a broken full-width image card. First-party CSS
/// keys off `.icon` and still wins, so the pill is unchanged for our own UI.
const LINK_ICON_SVG: &str = concat!(
    r#"<svg class="icon" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" "#,
    r#"stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">"#,
    r#"<path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>"#,
    r#"<path d="M15 3h6v6"/><path d="M10 14L21 3"/></svg>"#,
);

/// Folds a titled/link post's `name` (title) and Lemmy-style link target into
/// the Mastodon-API `content`, the way Mastodon itself surfaces converted
/// `Article`/`Page`/`Video` types — the standard `content` is the only body
/// field a stock client reads, so a title/link that lives only in Plamenu's
/// `title`/`external_url` extension keys is invisible there (the reported gap).
///
/// The title leads as a bold paragraph and the link trails as the same pill
/// markup the web client renders, so folding is a no-op for the first-party
/// UI's look (which now reads the folded `content` instead of the dropped
/// typed boxes) while a stock client, stripping the classes/SVG, still gets a
/// bold lead line and a clickable link. (It used to lead as an `<h2>`;
/// heading-sized titles read as excessive next to ordinary posts, so it is
/// now plain bold text — a render-time change, nothing is stored folded.)
/// Custom-emoji shortcodes in the title ride through untouched, so the
/// `emojis` array renders them for every client. Untitled, link-free posts
/// (the overwhelming majority) return `content` verbatim.
pub(crate) fn fold_typed_content(
    content: &str,
    title: Option<&str>,
    external_url: Option<&str>,
) -> String {
    if title.is_none() && external_url.is_none() {
        return content.to_owned();
    }
    let mut out = String::with_capacity(content.len() + 128);
    if let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) {
        out.push_str(r#"<p class="status__title"><strong>"#);
        out.push_str(&plamenu_ap::text::escape_html(title));
        out.push_str("</strong></p>");
    }
    out.push_str(content);
    if let Some(url) = external_url.map(str::trim).filter(|u| !u.is_empty()) {
        let display = url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        out.push_str(r#"<a class="status__external-link" rel="nofollow noopener" href=""#);
        out.push_str(&plamenu_ap::text::escape_html(url));
        out.push_str("\">");
        out.push_str(LINK_ICON_SVG);
        out.push_str("<span>");
        out.push_str(&plamenu_ap::text::escape_html(display));
        out.push_str("</span></a>");
    }
    out
}

/// The GoToSocial-style placeholder body a soft-deleted stub renders as.
pub(crate) const DELETED_STATUS_PLACEHOLDER: &str = "<p><em>ℹ️ deleted status ℹ️</em></p>";

#[allow(
    clippy::too_many_lines,
    reason = "the Mastodon status entity is intentionally assembled in one place so fields cannot drift"
)]
fn render_plain(domain: &str, item: &Status, maps: &RenderMaps<'_>) -> Result<Value, ApiError> {
    let author = &maps.shared.author_rows[&item.account_id];
    let author_entity = &maps.viewer_maps.author_entities[&item.account_id];
    // A sensitive-account action is a presentation override, not a rewrite of
    // every stored status.  Preserve the author's own view of their explicit
    // choice, while forcing the warning for everybody else like Mastodon.
    let sensitive =
        item.sensitive || (maps.viewer_maps.viewer != Some(item.account_id) && author.sensitized());
    let uri = status_uri_for_account(domain, item, author);
    let url = status_web_url(domain, item, &author.username);
    // A soft-deleted stub renders as a "deleted status" placeholder (GtS): every
    // content field and sidecar is emptied, but id/uri/threading/created and the
    // author are kept so the reply tree still reads. Uses only batch-loaded data
    // (author + reply parents), so it costs no per-row query. `deleted: true` is
    // a Plamenu extension the web thread card keys its tombstone styling on.
    if maps.shared.deleted.contains(&item.id) {
        let mut entity = json!({
            "id": item.id.to_string(),
            "created_at": rfc3339(item.created_at)?,
            "edited_at": null,
            "in_reply_to_id": item.in_reply_to_id.map(|id| id.to_string()),
            "in_reply_to_account_id": item
                .in_reply_to_id
                .and_then(|parent| maps.shared.reply_parents.get(&parent))
                .map(ToString::to_string),
            "sensitive": false,
            "spoiler_text": "",
            "visibility": item.visibility,
            "language": null,
            "uri": uri,
            "url": url,
            "replies_count": 0,
            "reblogs_count": 0,
            "favourites_count": 0,
            "quotes_count": 0,
            "favourited": false,
            "reblogged": false,
            "muted": false,
            "bookmarked": false,
            "content": DELETED_STATUS_PLACEHOLDER,
            "reblog": null,
            "application": null,
            "account": author_entity.clone(),
            "media_attachments": [],
            "mentions": [],
            "tags": [],
            "tagged_collections": [],
            "emojis": [],
            "card": null,
            "poll": null,
            "quote": null,
            "quote_approval": null,
            "title": null,
            "object_type": null,
            "external_url": null,
            "event": null,
            "group_post": false,
            "downvotes_count": 0,
            "downvoted": false,
            "group_locked": false,
            "deleted": true,
        });
        entity["groups"] = json!([]);
        entity["in_reply_to_uri"] = maps
            .shared
            .unresolved_parent_uris
            .get(&item.id)
            .map_or(Value::Null, |uri| Value::String(uri.clone()));
        return Ok(entity);
    }
    let engagement = maps
        .shared
        .engagement
        .get(&item.id)
        .copied()
        .unwrap_or_default();
    let edited_at = match item.edited_at {
        Some(at) => Some(rfc3339(at)?),
        None => None,
    };
    // Remove the compatibility link only when this response actually embeds
    // the accepted quote card. An accepted row whose target is gone (or a
    // nested quote represented only by id) still needs the fallback link.
    let quote_is_shown = maps
        .viewer_maps
        .quotes
        .get(&item.id)
        .and_then(Value::as_object)
        .is_some_and(|quote| {
            quote.get("state").and_then(Value::as_str) == Some("accepted")
                && quote
                    .get("quoted_status")
                    .is_some_and(|status| !status.is_null())
        });
    let visible_content = if quote_is_shown {
        maps.shared.accepted_quote_links.get(&item.id).map_or_else(
            || item.content.clone(),
            |quoted_links| plamenu_ap::text::strip_quote_fallback(&item.content, quoted_links),
        )
    } else {
        item.content.clone()
    };
    let mut entity = json!({
        "id": item.id.to_string(),
        "created_at": rfc3339(item.created_at)?,
        "edited_at": edited_at,
        "in_reply_to_id": item.in_reply_to_id.map(|id| id.to_string()),
        "in_reply_to_account_id": item
            .in_reply_to_id
            .and_then(|parent| maps.shared.reply_parents.get(&parent))
            .map(ToString::to_string),
        "sensitive": sensitive,
        "spoiler_text": item.spoiler_text,
        "visibility": item.visibility,
        "language": item.language,
        "uri": uri,
        "url": url,
        "replies_count": engagement.replies,
        "reblogs_count": engagement.reblogs,
        "favourites_count": engagement.favourites,
        "quotes_count": engagement.quotes,
        "favourited": maps.viewer_maps.favourited.contains(&item.id),
        "reblogged": maps.viewer_maps.reblogged.contains(&item.id),
        "muted": maps.viewer_maps.muted.contains(&item.id),
        "bookmarked": maps.viewer_maps.bookmarked.contains(&item.id),
        // The title/link fold (below) makes converted `Page`/`Video`/`Article`
        // posts legible to stock Mastodon clients, which read only `content`.
        "content": fold_typed_content(&visible_content, item.title.as_deref(), item.external_url.as_deref()),
        "reblog": null,
        "application": null,
        "account": null,
        "media_attachments": maps.variant.attachments.get(&item.id).cloned().unwrap_or_default(),
        "mentions": maps.shared.mentions.get(&item.id).cloned().unwrap_or_default(),
        "tags": maps.shared.tags.get(&item.id).cloned().unwrap_or_default(),
        "tagged_collections": maps.viewer_maps.tagged_collections.get(&item.id).cloned().unwrap_or_default(),
        "emojis": maps.variant.emojis.get(&item.id).cloned().unwrap_or_default(),
        "card": maps.variant.cards.get(&item.id).cloned(),
        "poll": maps.viewer_maps.polls.get(&item.id).cloned(),
        "quote": maps.viewer_maps.quotes.get(&item.id).cloned(),
        "quote_approval": quote_approval(item, maps),
        // Content-universe extensions (Mastodon has no Status.title):
        // the hoisted inbound title, the source object's AS type, a link
        // post's target URL, and the typed Event sidecar. Always present,
        // null when absent, so clients can feature-detect on shape.
        "title": item.title,
        "object_type": item.object_type,
        "external_url": item.external_url,
        "event": maps.viewer_maps.events.get(&item.id).cloned(),
        "webxdc_invitation": maps.shared.webxdc_invitations.get(&item.id).cloned(),
        // Group votes (Plamenu extensions, like the fields above):
        // `group_post` marks a status attributed to a group; on one, the
        // favourite doubles as the upvote and the score is
        // `favourites_count - downvotes_count`. Always present so clients
        // can feature-detect on shape.
        "group_post": maps.shared.group_attributed.contains(&item.id),
        "downvotes_count": engagement.dislikes,
        "downvoted": maps.viewer_maps.disliked.contains(&item.id),
        // `group_locked` marks a thread a group moderator locked (no new
        // comments); clients hide the reply control on one.
        "group_locked": maps.shared.group_locked.contains(&item.id),
    });
    entity["account"] = author_entity.clone();
    // Full community identities, not merely `group_post`. This is what lets
    // clients retain "posted in …" when a surface renders the bare original
    // instead of the Group actor's Announce wrapper. Appended outside the
    // already-large json! literal to keep that macro under its recursion limit.
    entity["groups"] = Value::Array(group_entities(item.id, maps));
    // Plamenu extension: the AP URI of a reply parent we never fetched
    // (`in_reply_to_id` null). Lets the web client show an "unfetched parent"
    // notice and link out rather than orphaning the reply. Null the rest of the
    // time (absent parent, or parent resolved locally). Appended after the json!
    // literal to keep that macro under the recursion limit.
    entity["in_reply_to_uri"] = maps
        .shared
        .unresolved_parent_uris
        .get(&item.id)
        .map_or(Value::Null, |uri| Value::String(uri.clone()));
    apply_application(&mut entity, item, maps);
    // Pleroma's emoji-reaction chips — a Pleroma/Misskey-family extension
    // carried alongside the Mastodon fields. Real Pleroma/Akkoma expose the
    // array in two places: a top-level `emoji_reactions` and a mirror under
    // `pleroma`. Clients differ on which they read (Phanpy takes the top-level
    // one), so we emit both. Always present (empty when none) so Pleroma-aware
    // clients find the key.
    let reactions = emoji_reactions(domain, item, maps);
    entity["emoji_reactions"] = reactions.clone();
    entity["pleroma"] = json!({ "emoji_reactions": reactions });
    // `pinned` appears only on the owner's own pinnable statuses, like
    // Mastodon's `StatusSerializer#pinnable?`.
    if maps.viewer_maps.viewer == Some(item.account_id)
        && matches!(item.visibility.as_str(), "public" | "unlisted" | "private")
    {
        entity["pinned"] = Value::Bool(maps.viewer_maps.pinned.contains(&item.id));
    }
    // `filtered` rides on every status for an authenticated viewer (omitted
    // for anonymous ones, like Mastodon's `if: :current_user?`).
    if maps.viewer_maps.viewer.is_some() {
        entity["filtered"] = maps
            .viewer_maps
            .filtered
            .get(&item.id)
            .cloned()
            .unwrap_or_else(|| json!([]));
    }
    Ok(entity)
}

/// How a reaction group's emoji is named for clients: a remote custom emoji
/// is qualified Pleroma-style as `shortcode@host` (the host taken from its
/// image URL), a local custom emoji or Unicode emoji stays bare.
pub(crate) fn displayed_reaction_name(domain: &str, group: &reaction::ReactionGroup) -> String {
    let Some(url) = &group.custom_emoji_url else {
        return group.name.clone();
    };
    let Ok(parsed) = url::Url::parse(url) else {
        return group.name.clone();
    };
    match parsed.host_str() {
        Some(host) if host != domain => format!("{}@{host}", group.name),
        _ => group.name.clone(),
    }
}

/// The **client-facing** image URL for each status' reaction custom emoji,
/// keyed by status id then reaction name — the value emitted as the reaction's
/// `url`, kept separate from `custom_emoji_url` (whose origin host still drives
/// the `shortcode@host` display name). A local reaction already carries a
/// client-safe `/media/...` URL; a remote one (a Pleroma join that reused the
/// federated image) is matched back to its `custom_emojis` row by that exact
/// image URL and re-pointed at the proxy — or dropped (`None`) when
/// unresolvable, so a client is never handed the origin.
async fn reaction_emoji_url_map(
    pool: &PgPool,
    domain: &str,
    allow_direct: bool,
    reactions: &HashMap<i64, Vec<reaction::ReactionGroup>>,
) -> Result<HashMap<i64, HashMap<String, String>>, ApiError> {
    let self_prefix = format!("https://{domain}/media/");
    // Every distinct non-local emoji URL on the page in one query. URLs with
    // no matching emoji row resolve to nothing however often they occur —
    // the unresolvable case used to be the worst one (re-queried per
    // occurrence, no negative memo).
    let mut remote_urls: Vec<String> = reactions
        .values()
        .flatten()
        .filter_map(|group| group.custom_emoji_url.clone())
        .filter(|url| !url.starts_with(&self_prefix))
        .collect();
    remote_urls.sort_unstable();
    remote_urls.dedup();
    let client_urls: HashMap<String, String> =
        custom_emoji::find_by_remote_image_urls(pool, &remote_urls)
            .await?
            .into_iter()
            .filter_map(|emoji| {
                let remote = emoji.image_remote_url.clone()?;
                let client = crate::emoji::client_image_url(domain, &emoji, allow_direct);
                (!client.is_empty()).then_some((remote, client))
            })
            .collect();
    let mut out: HashMap<i64, HashMap<String, String>> = HashMap::new();
    for (status_id, groups) in reactions {
        for group in groups {
            let Some(url) = group.custom_emoji_url.as_deref() else {
                continue;
            };
            let client_url = if url.starts_with(&self_prefix) {
                Some(url.to_owned())
            } else {
                client_urls.get(url).cloned()
            };
            if let Some(client_url) = client_url {
                out.entry(*status_id)
                    .or_default()
                    .insert(reaction_group_key(group), client_url);
            }
        }
    }
    Ok(out)
}

/// Pleroma's `emoji_reactions` array for a status: one entry per emoji with
/// its `count`, the reactor `account_ids`, whether the viewer reacted (`me`),
/// and — for a custom emoji — its image `url`.
fn emoji_reactions(domain: &str, item: &Status, maps: &RenderMaps<'_>) -> Value {
    let Some(groups) = maps.shared.reactions.get(&item.id) else {
        return json!([]);
    };
    let entries: Vec<Value> = groups
        .iter()
        .map(|group| {
            let me = maps
                .viewer_maps
                .viewer
                .is_some_and(|viewer| group.account_ids.contains(&viewer));
            let account_ids: Vec<String> = group.account_ids.iter().map(i64::to_string).collect();
            let mut entry = json!({
                "name": displayed_reaction_name(domain, group),
                "count": group.count,
                "me": me,
                "account_ids": account_ids,
            });
            if let Some(url) = maps
                .variant
                .reaction_emoji_urls
                .get(&item.id)
                .and_then(|by_name| by_name.get(&reaction_group_key(group)))
            {
                entry["url"] = json!(url);
            }
            entry
        })
        .collect();
    Value::Array(entries)
}

fn reaction_group_key(group: &reaction::ReactionGroup) -> String {
    group.custom_emoji_origin_id.map_or_else(
        || format!("name:{}", group.name),
        |origin| format!("origin:{origin}"),
    )
}

fn apply_application(entity: &mut Value, item: &Status, maps: &RenderMaps<'_>) {
    let show = maps.viewer_maps.viewer == Some(item.account_id)
        || maps
            .shared
            .show_application
            .get(&item.account_id)
            .copied()
            .unwrap_or(false);
    if !show {
        if let Some(object) = entity.as_object_mut() {
            object.remove("application");
        }
        return;
    }
    if let Some(app_id) = item.application_id
        && let Some(application) = maps.shared.applications.get(&app_id)
    {
        entity["application"] = application.clone();
    }
}

/// Mastodon's `quote_approval` attachment: `automatic`/`manual` are the
/// audience key lists of the stored policy bitmap; `current_user` is how the
/// viewer would be treated if they quoted now (`automatic`/`manual`/`unknown`/
/// `denied`), mirroring `Status#quote_policy_for_account`.
fn quote_approval(item: &Status, maps: &RenderMaps<'_>) -> Value {
    let policy = plamenu_ap::quote_policy::QuotePolicy::from_bitmap(item.quote_approval_policy);
    json!({
        "automatic": policy.automatic().keys(),
        "manual": policy.manual().keys(),
        "current_user": quote_current_user(item, maps, policy),
    })
}

/// How the viewer would be treated if they quoted `item` right now. The
/// Both relationship directions come from the render pass' prefetched follow
/// edges, so inbound `followers` and `following` policies stay consistent with
/// the posting-service authorization without adding per-status queries.
fn quote_current_user(
    item: &Status,
    maps: &RenderMaps<'_>,
    policy: plamenu_ap::quote_policy::QuotePolicy,
) -> &'static str {
    let Some(viewer) = maps.viewer_maps.viewer else {
        return "denied";
    };
    if matches!(item.visibility.as_str(), "direct" | "local") || item.reblog_of_id.is_some() {
        return "denied";
    }
    if viewer == item.account_id {
        return "automatic"; // the author may always quote themselves
    }
    let follows_author = maps
        .viewer_maps
        .quote_followed_authors
        .contains(&item.account_id);
    let followed_by_author = maps
        .viewer_maps
        .quote_following_authors
        .contains(&item.account_id);
    let automatic = policy.automatic();
    let manual = policy.manual();
    if automatic.public()
        || (automatic.followers() && follows_author)
        || (automatic.following() && followed_by_author)
    {
        "automatic"
    } else if manual.public()
        || (manual.followers() && follows_author)
        || (manual.following() && followed_by_author)
    {
        "manual"
    } else if automatic.unsupported() || manual.unsupported() {
        "unknown"
    } else {
        "denied"
    }
}

#[allow(clippy::too_many_arguments)]
fn status_edit_json(
    domain: &str,
    author_entity: &Value,
    content: &str,
    spoiler_text: &str,
    sensitive: bool,
    created_at: time::OffsetDateTime,
    media: &[Media],
    emojis: &[Value],
    allow_direct: bool,
) -> Result<Value, ApiError> {
    Ok(json!({
        "content": content,
        "spoiler_text": spoiler_text,
        "sensitive": sensitive,
        "created_at": rfc3339(created_at)?,
        "account": author_entity,
        "media_attachments": media
            .iter()
            .map(|m| media_json(domain, m, allow_direct))
            .collect::<Vec<_>>(),
        "emojis": emojis,
    }))
}

/// The Mastodon `StatusEdit` entities of `/history`, oldest first: the
/// recorded versions, or — for never-edited statuses — the single current
/// version, like Mastodon synthesizes.
pub async fn render_status_history(
    pool: &PgPool,
    domain: &str,
    item: &Status,
    viewer: Option<i64>,
) -> Result<Vec<Value>, ApiError> {
    let allow_direct = allow_direct_media(pool, viewer).await;
    let author = account::find_by_id(pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let author_entity = account_json(pool, domain, &author, viewer).await?;
    let edits = plamenu_db::status_edit::for_status(pool, item.id).await?;
    if edits.is_empty() {
        let media = media::for_statuses(pool, &[item.id])
            .await?
            .remove(&item.id)
            .unwrap_or_default();
        let emojis = if author.domain.is_none() {
            let codes = crate::emoji::shortcodes_of(&[&item.spoiler_text, &item.content]);
            custom_emoji::lookup_historical_local_for_account(pool, author.id, &codes)
                .await?
                .iter()
                .map(|emoji| crate::emoji::custom_emoji_json(domain, emoji, allow_direct))
                .collect()
        } else {
            crate::emoji::emojis_json(
                pool,
                domain,
                author.domain.as_deref(),
                &[&item.spoiler_text, &item.content],
                allow_direct,
            )
            .await?
        };
        return Ok(vec![status_edit_json(
            domain,
            &author_entity,
            &item.content,
            &item.spoiler_text,
            item.sensitive,
            item.edited_at.unwrap_or(item.created_at),
            &media,
            &emojis,
            allow_direct,
        )?]);
    }
    // Every revision shares one author (one emoji domain bucket) and the
    // attachment sets overlap heavily, so both lookups are hoisted: one
    // `media::find_by_ids` over the union of every revision's ids and one
    // emoji lookup over the union of every revision's text, instead of 1-2
    // round trips per revision over an unbounded revision count.
    let mut all_media_ids: Vec<i64> = edits.iter().flat_map(|e| e.media_ids.clone()).collect();
    all_media_ids.sort_unstable();
    all_media_ids.dedup();
    let media_rows: HashMap<i64, Media> = media::find_by_ids(pool, &all_media_ids)
        .await?
        .into_iter()
        .map(|m| (m.id, m))
        .collect();
    let all_texts: Vec<&str> = edits
        .iter()
        .flat_map(|e| [e.spoiler_text.as_str(), e.content.as_str()])
        .collect();
    let codes = crate::emoji::shortcodes_of(&all_texts);
    let loaded = if author.domain.is_none() {
        custom_emoji::lookup_historical_local_for_account(pool, author.id, &codes).await?
    } else {
        custom_emoji::lookup(pool, &codes, author.domain.as_deref()).await?
    };
    let by_shortcode: HashMap<String, CustomEmoji> = loaded
        .into_iter()
        .map(|e| (e.shortcode.clone(), e))
        .collect();
    let mut entries = Vec::with_capacity(edits.len());
    for edit in &edits {
        // In id order, matching the per-revision `find_by_ids` this replaced.
        let mut media: Vec<Media> = edit
            .media_ids
            .iter()
            .filter_map(|id| media_rows.get(id).cloned())
            .collect();
        media.sort_by_key(|m| m.id);
        let emojis = emojis_from_table(
            domain,
            Some(&by_shortcode),
            &[edit.spoiler_text.as_str(), edit.content.as_str()],
            allow_direct,
        );
        entries.push(status_edit_json(
            domain,
            &author_entity,
            &edit.content,
            &edit.spoiler_text,
            edit.sensitive,
            edit.created_at,
            &media,
            &emojis,
            allow_direct,
        )?);
    }
    Ok(entries)
}

/// Convenience wrapper for a single status.
pub async fn render_status(
    pool: &PgPool,
    domain: &str,
    item: &Status,
    viewer: Option<i64>,
) -> Result<Value, ApiError> {
    let mut rendered = render_statuses(pool, domain, std::slice::from_ref(item), viewer).await?;
    rendered.pop().ok_or(ApiError::NotFound)
}

/// Renders one status for every viewer in `viewers` at once — the streaming
/// fan-out renderer (N+1 decisions). Each viewer's document is
/// byte-identical to a [`render_status`] call for that viewer; the shared
/// maps are computed once and every per-viewer dimension is one batched query
/// across the set, so the statement count is independent of how many viewers
/// are connected.
pub async fn render_status_for_viewers(
    pool: &PgPool,
    domain: &str,
    item: &Status,
    viewers: &[i64],
) -> Result<HashMap<i64, Value>, ApiError> {
    let set: Vec<Option<i64>> = viewers.iter().copied().map(Some).collect();
    let per_viewer =
        render_statuses_for_viewer_set(pool, domain, std::slice::from_ref(item), &set, 0).await?;
    let mut out = HashMap::with_capacity(per_viewer.len());
    for (viewer, mut values) in per_viewer {
        if let (Some(viewer), Some(value)) = (viewer, values.pop()) {
            out.insert(viewer, value);
        }
    }
    Ok(out)
}

/// Rewrites a rendered status the way Mastodon serializes a `DELETE`
/// response (`source_requested`): the raw source `text` replaces `content`
/// so clients can delete-and-redraft. The option propagates into the nested
/// `reblog` in Mastodon, so a deleted boost gets `text: null` on the wrapper
/// and its target's source (`null` when remote — Mastodon stores no source
/// for those) nested inside.
pub async fn apply_redraft_source(
    pool: &PgPool,
    item: &Status,
    entity: &mut Value,
) -> Result<(), ApiError> {
    let source_of = |entity: &mut Value, text: Option<String>| {
        if let Some(object) = entity.as_object_mut() {
            object.remove("content");
            object.insert("text".into(), json!(text));
        }
    };
    match item.reblog_of_id {
        None => source_of(
            entity,
            status::source_of(pool, item.id).await?.map(|s| s.text),
        ),
        Some(target_id) => {
            source_of(entity, None);
            let target_text = match status::find_by_id(pool, target_id).await? {
                Some(target) if target.uri.is_none() => {
                    status::source_of(pool, target_id).await?.map(|s| s.text)
                }
                _ => None,
            };
            source_of(&mut entity["reblog"], target_text);
        }
    }
    Ok(())
}

/// The Mastodon `Notification` entity.
/// Notification types that carry a `status` (Mastodon's `status_type?`).
const NOTIFICATION_STATUS_TYPES: &[&str] = &[
    "favourite",
    "reblog",
    "status",
    "mention",
    "poll",
    "update",
    "quoted_update",
    "quote",
    "live",
    // Pleroma's reaction notification is a status-bearing extension: its
    // contract includes the post that received the reaction.
    "pleroma:emoji_reaction",
];

/// Notification types that carry a `collection` (Mastodon's `collection_type?`).
const NOTIFICATION_COLLECTION_TYPES: &[&str] = &["added_to_collection", "collection_update"];

/// Renders a page of notifications with a query count independent of page
/// size. The per-item version issued a fresh `account_json` + `find_by_id` +
/// singular `render_status` for every row (~600 queries for a default page of
/// 40) — the one render loop the slice-2–5 N+1 hunt missed. This prefetches
/// every distinct sender (one `render_accounts_by_ids`), every distinct
/// referenced status (one `find_by_ids` + `render_statuses`) and every
/// distinct referenced collection, then assembles the rows from id-keyed
/// maps. Behind `GET /api/v1/notifications`, the web notifications page and
/// the streaming `notification` event. Guarded by `tests/nplus1_guard.rs`.
#[allow(clippy::too_many_lines)] // one prefetch block per referenced entity kind
pub async fn render_notifications(
    pool: &PgPool,
    domain: &str,
    items: &[Notification],
    viewer: i64,
) -> Result<Vec<Value>, ApiError> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let from_ids: Vec<i64> = {
        let mut ids: Vec<i64> = items.iter().map(|i| i.from_account_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let rendered_accounts = render_accounts_by_ids(pool, domain, &from_ids, Some(viewer)).await?;
    let accounts_by_id: HashMap<i64, Value> = rendered_accounts
        .into_iter()
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.parse::<i64>().ok()?;
            Some((id, value))
        })
        .collect();

    let status_ids: Vec<i64> = {
        let mut ids: Vec<i64> = items.iter().filter_map(|i| i.status_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let statuses = status::find_by_ids(pool, &status_ids).await?;
    let rendered_statuses = render_statuses(pool, domain, &statuses, Some(viewer)).await?;
    let statuses_by_id: HashMap<i64, Value> = rendered_statuses
        .into_iter()
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.parse::<i64>().ok()?;
            Some((id, value))
        })
        .collect();

    // Every referenced collection resolved in one batched pass
    // (`collections_json_map`): the composite that used to run per distinct
    // collection (find + owner + items + tag) is now four grouped queries for
    // the page.
    let collection_ids: Vec<i64> = {
        let mut ids: Vec<i64> = items
            .iter()
            .filter(|i| NOTIFICATION_COLLECTION_TYPES.contains(&i.kind.as_str()))
            .filter_map(|i| i.collection_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let collections_by_id =
        crate::collections::collections_json_map(pool, domain, &collection_ids, Some(viewer))
            .await?;

    // `moderation_warning` strikes likewise resolve in one batched pass
    // (`account_warnings_json_map`) — strike fetch, target-account render and
    // appeal fetch each grouped over the page's distinct strike ids.
    let warning_ids: Vec<i64> = {
        let mut ids: Vec<i64> = items
            .iter()
            .filter(|i| i.kind == "moderation_warning")
            .filter_map(|i| i.account_warning_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let warnings_by_id =
        account_warnings_json_map(pool, domain, &warning_ids, Some(viewer)).await?;

    // `admin.report` embeds its report (Mastodon's `REST::ReportSerializer`
    // via the notification's `report`) — staff-only and rare, so a
    // per-distinct loop like strikes above.
    let report_ids: Vec<i64> = {
        let mut ids: Vec<i64> = items
            .iter()
            .filter(|i| i.kind == "admin.report")
            .filter_map(|i| i.report_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let mut reports_by_id: HashMap<i64, Value> = HashMap::new();
    for report_id in report_ids {
        if let Some(report) = plamenu_db::report::find_by_id(pool, report_id).await? {
            reports_by_id.insert(report_id, admin_report_json(pool, domain, &report).await?);
        }
    }

    let mut result = Vec::with_capacity(items.len());
    for item in items {
        // A dangling sender is a 404, exactly like the per-item
        // `find_by_id(...).ok_or(NotFound)` this replaced.
        let from_entity = accounts_by_id
            .get(&item.from_account_id)
            .cloned()
            .ok_or(ApiError::NotFound)?;
        let mut entity = json!({
            "id": item.id.to_string(),
            "type": item.kind,
            "created_at": rfc3339(item.created_at)?,
            "group_key": plamenu_db::notification::effective_group_key(item, None),
            "account": from_entity,
        });
        // Mastodon's `NotificationSerializer` attaches `status` only for
        // status-bearing types (`status_type?`); the key is absent otherwise
        // (a `status: null` on a `follow` is a divergence clients notice).
        if NOTIFICATION_STATUS_TYPES.contains(&item.kind.as_str()) {
            entity["status"] = item
                .status_id
                .and_then(|id| statuses_by_id.get(&id).cloned())
                .unwrap_or(Value::Null);
        }
        // Likewise `collection` (full `CollectionSerializer`, incl. `items`)
        // for `added_to_collection`/`collection_update` — a client renders the
        // "remove me" action from `collection.items`, so it must be present.
        if NOTIFICATION_COLLECTION_TYPES.contains(&item.kind.as_str()) {
            entity["collection"] = item
                .collection_id
                .and_then(|id| collections_by_id.get(&id).cloned())
                .unwrap_or(Value::Null);
        }
        // Likewise `moderation_warning` embeds the strike itself.
        if item.kind == "moderation_warning" {
            entity["moderation_warning"] = item
                .account_warning_id
                .and_then(|id| warnings_by_id.get(&id).cloned())
                .unwrap_or(Value::Null);
        }
        // Likewise `admin.report` embeds the filed report.
        if item.kind == "admin.report" {
            entity["report"] = item
                .report_id
                .and_then(|id| reports_by_id.get(&id).cloned())
                .unwrap_or(Value::Null);
        }
        // Mastodon only emits `filtered` on a policy-filtered notification.
        if item.filtered {
            entity["filtered"] = json!(true);
        }
        // Pleroma's `pleroma:emoji_reaction` notification carries the reacted
        // emoji, plus `emoji_url` when it's a custom emoji — the embedded
        // status' reaction groups already hold the client-facing image URL,
        // so match the shortcode there (group names may carry an `@host`
        // suffix for remote emoji; the notification's shortcode never does).
        if let Some(emoji) = &item.emoji {
            entity["emoji"] = json!(emoji);
            let code = emoji.strip_prefix(':').and_then(|e| e.strip_suffix(':'));
            let url = code.and_then(|code| {
                item.status_id
                    .and_then(|id| statuses_by_id.get(&id))
                    .and_then(|status| status.get("emoji_reactions"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|group| {
                        group
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| name.split('@').next() == Some(code))
                    })
                    .and_then(|group| group.get("url"))
                    .cloned()
            });
            if let Some(url) = url {
                entity["emoji_url"] = url;
            }
        }
        result.push(entity);
    }
    Ok(result)
}

/// The Mastodon `AccountWarning` entity a `moderation_warning` notification
/// embeds (`REST::AccountWarningSerializer`): the strike's action/text plus
/// the full target-account entity and the appeal (with its state), `null`
/// when none was filed. `None` when the strike itself is gone.
pub async fn account_warning_json(
    pool: &PgPool,
    domain: &str,
    warning_id: i64,
    viewer: Option<i64>,
) -> Result<Option<Value>, ApiError> {
    let mut rendered = account_warnings_json_map(pool, domain, &[warning_id], viewer).await?;
    Ok(rendered.remove(&warning_id))
}

/// [`account_warning_json`] for a set of strike ids, keyed by id — the
/// batched form the notification renderers use: one strike fetch, one
/// batched target-account render and one grouped appeal fetch regardless of
/// how many strikes the page references. Strikes that are gone are simply
/// absent; a target account that is gone renders as `null`, like the
/// singular form.
pub async fn account_warnings_json_map(
    pool: &PgPool,
    domain: &str,
    warning_ids: &[i64],
    viewer: Option<i64>,
) -> Result<HashMap<i64, Value>, ApiError> {
    if warning_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let warnings = plamenu_db::account_warning::find_by_ids(pool, warning_ids).await?;
    let mut target_ids: Vec<i64> = warnings.iter().map(|w| w.target_account_id).collect();
    target_ids.sort_unstable();
    target_ids.dedup();
    let targets: HashMap<i64, Value> = render_accounts_by_ids(pool, domain, &target_ids, viewer)
        .await?
        .into_iter()
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.parse::<i64>().ok()?;
            Some((id, value))
        })
        .collect();
    let found_ids: Vec<i64> = warnings.iter().map(|w| w.id).collect();
    let appeals: HashMap<i64, Value> = plamenu_db::appeal::find_by_warnings(pool, &found_ids)
        .await?
        .into_iter()
        .map(|appeal| {
            let state = if appeal.approved_at.is_some() {
                "approved"
            } else if appeal.rejected_at.is_some() {
                "rejected"
            } else {
                "pending"
            };
            (
                appeal.account_warning_id,
                json!({ "text": appeal.text, "state": state }),
            )
        })
        .collect();
    let mut rendered = HashMap::with_capacity(warnings.len());
    for warning in &warnings {
        rendered.insert(
            warning.id,
            json!({
                "id": warning.id.to_string(),
                "action": warning.action,
                "text": warning.text,
                "status_ids": warning
                    .status_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                "created_at": rfc3339(warning.created_at)?,
                "target_account": targets
                    .get(&warning.target_account_id)
                    .cloned()
                    .unwrap_or(Value::Null),
                "appeal": appeals.get(&warning.id).cloned().unwrap_or(Value::Null),
            }),
        );
    }
    Ok(rendered)
}

/// The Mastodon `Conversation` entity: participants (the account itself for
/// a self-DM, like Mastodon's fallback), the newest visible status (`null`
/// if it has vanished), and the read flag.
pub async fn render_conversation(
    pool: &PgPool,
    domain: &str,
    viewer_id: i64,
    row: &plamenu_db::conversation::AccountConversation,
) -> Result<Value, ApiError> {
    let mut accounts =
        render_accounts_by_ids(pool, domain, &row.participant_account_ids, Some(viewer_id)).await?;
    if accounts.is_empty() {
        accounts = render_accounts_by_ids(pool, domain, &[viewer_id], Some(viewer_id)).await?;
    }
    let last_status = match row.last_status_id {
        Some(last_id) => match status::find_by_id(pool, last_id).await? {
            Some(item) => render_status(pool, domain, &item, Some(viewer_id)).await?,
            None => Value::Null,
        },
        None => Value::Null,
    };
    Ok(json!({
        "id": row.id.to_string(),
        "unread": row.unread,
        "accounts": accounts,
        "last_status": last_status,
    }))
}

/// Batched [`render_conversation`]: renders a whole page of conversation rows
/// with a query count independent of page size. The per-row version issues a
/// fresh `render_accounts_by_ids` + `find_by_id` + `render_status` for every
/// conversation (~38 queries each); this prefetches every participant account
/// and every last-status once and assembles the rows from id-keyed maps. Used
/// by `GET /api/v1/conversations`, which clients poll frequently. Guarded by
/// `tests/nplus1_guard.rs`.
pub async fn render_conversations(
    pool: &PgPool,
    domain: &str,
    viewer_id: i64,
    rows: &[plamenu_db::conversation::AccountConversation],
) -> Result<Vec<Value>, ApiError> {
    // Every account referenced across the page, deduped. The viewer is always
    // included so it can serve as the fallback for a row whose participants
    // are empty (a "note to self") or all filtered out (a hidden partner) —
    // matching the singular renderer.
    let mut account_ids: Vec<i64> = vec![viewer_id];
    for row in rows {
        account_ids.extend_from_slice(&row.participant_account_ids);
    }
    account_ids.sort_unstable();
    account_ids.dedup();

    let rendered_accounts =
        render_accounts_by_ids(pool, domain, &account_ids, Some(viewer_id)).await?;
    let accounts_by_id: HashMap<i64, Value> = rendered_accounts
        .into_iter()
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.parse::<i64>().ok()?;
            Some((id, value))
        })
        .collect();

    // Every distinct last-status, fetched and rendered in one batch.
    let status_ids: Vec<i64> = {
        let mut ids: Vec<i64> = rows.iter().filter_map(|r| r.last_status_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let statuses = status::find_by_ids(pool, &status_ids).await?;
    let rendered_statuses = render_statuses(pool, domain, &statuses, Some(viewer_id)).await?;
    let statuses_by_id: HashMap<i64, Value> = rendered_statuses
        .into_iter()
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.parse::<i64>().ok()?;
            Some((id, value))
        })
        .collect();

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut accounts: Vec<Value> = row
            .participant_account_ids
            .iter()
            .filter_map(|id| accounts_by_id.get(id).cloned())
            .collect();
        if accounts.is_empty() {
            accounts.extend(accounts_by_id.get(&viewer_id).cloned());
        }
        let last_status = row
            .last_status_id
            .and_then(|id| statuses_by_id.get(&id).cloned())
            .unwrap_or(Value::Null);
        out.push(json!({
            "id": row.id.to_string(),
            "unread": row.unread,
            "accounts": accounts,
            "last_status": last_status,
        }));
    }
    Ok(out)
}

/// Whether `viewer` may see a status: `private` is limited to the author,
/// accepted followers and mentioned accounts; `direct` to the author and
/// mentioned accounts only — a follow must never expose someone's DMs. An
/// author hides every visibility from viewers they block, including remote
/// viewers from domains they block (Mastodon's `StatusPolicy`); the blocked
/// side gets a 404, never a tell-tale 403.
pub async fn can_view(pool: &PgPool, item: &Status, viewer: Option<i64>) -> Result<bool, ApiError> {
    if !crate::instance_policy::account_id_visible(pool, item.account_id).await? {
        return Ok(false);
    }
    let proper_account_id = match item.reblog_of_id {
        Some(target_id) => status::find_by_id(pool, target_id)
            .await?
            .map(|target| target.account_id)
            .ok_or(ApiError::NotFound)?,
        None => item.account_id,
    };
    if proper_account_id != item.account_id
        && !crate::instance_policy::account_id_visible(pool, proper_account_id).await?
    {
        return Ok(false);
    }
    let Some(viewer_id) = viewer else {
        return Ok(!matches!(
            item.visibility.as_str(),
            "private" | "direct" | "local"
        ));
    };
    if viewer_id == item.account_id {
        return Ok(true);
    }
    if block::exists(pool, item.account_id, viewer_id).await? {
        return Ok(false);
    }
    if proper_account_id != item.account_id
        && block::exists(pool, proper_account_id, viewer_id).await?
    {
        return Ok(false);
    }
    if plamenu_db::account_domain_block::blocks_account_domain(pool, item.account_id, viewer_id)
        .await?
    {
        return Ok(false);
    }
    if proper_account_id != item.account_id
        && plamenu_db::account_domain_block::blocks_account_domain(
            pool,
            proper_account_id,
            viewer_id,
        )
        .await?
    {
        return Ok(false);
    }
    if item.visibility == "local" {
        return Ok(account::has_local_account(pool, item.account_id).await?);
    }
    if !matches!(item.visibility.as_str(), "private" | "direct") {
        return Ok(true);
    }
    if item.visibility == "private"
        && follow::pending_state(pool, viewer_id, item.account_id)
            .await?
            .is_some_and(|pending| !pending)
    {
        return Ok(true);
    }
    // Mentioned accounts may see posts that name them.
    mention::exists(pool, item.id, viewer_id)
        .await
        .map_err(Into::into)
}

/// Batched [`can_view`]: returns the subset of `items` the viewer may see, with
/// a query count independent of the page size. The per-item `can_view` runs ~3
/// queries per public status (instance policy + block + domain block) plus a
/// follow/mention lookup for private/direct ones; on a long thread or search
/// page that is a heavy N+1. This prefetches every visibility input once and
/// decides each item in memory, preserving `can_view`'s exact predicate.
///
/// `viewer` is always the authenticated *local* account, so `can_view`'s
/// account-domain-block branch (which joins on the viewer's own domain, always
/// NULL for a local account) can never fire and is elided here — matching the
/// behavior for every real caller.
pub async fn filter_viewable(
    pool: &PgPool,
    items: &[Status],
    viewer: Option<i64>,
) -> Result<Vec<Status>, ApiError> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let reblog_ids: Vec<i64> = items.iter().filter_map(|item| item.reblog_of_id).collect();
    let reblog_targets = status::find_by_ids(pool, &reblog_ids).await?;
    let target_authors: HashMap<i64, i64> = reblog_targets
        .into_iter()
        .map(|target| (target.id, target.account_id))
        .collect();
    let proper_author = |item: &Status| {
        item.reblog_of_id
            .and_then(|target_id| target_authors.get(&target_id).copied())
            .unwrap_or(item.account_id)
    };
    let mut author_ids: Vec<i64> = items
        .iter()
        .flat_map(|item| [item.account_id, proper_author(item)])
        .collect();
    author_ids.sort_unstable();
    author_ids.dedup();

    // Instance policy + local-author flag for every distinct author, one query.
    let vis = account::visibility_batch(pool, &author_ids).await?;
    let instance_ok: HashSet<i64> = vis.iter().filter(|v| v.allowed).map(|v| v.id).collect();
    let local_author: HashSet<i64> = vis.iter().filter(|v| v.is_local).map(|v| v.id).collect();

    let Some(viewer_id) = viewer else {
        // Anonymous: instance-visible public/unlisted posts only.
        return Ok(items
            .iter()
            .filter(|s| {
                instance_ok.contains(&s.account_id)
                    && instance_ok.contains(&proper_author(s))
                    && !matches!(s.visibility.as_str(), "private" | "direct" | "local")
            })
            .cloned()
            .collect());
    };

    // Authors who block the viewer (`can_view`'s `block::exists(author, viewer)`).
    let blockers = block::blocked_by_batch(pool, viewer_id, &author_ids).await?;
    // Authors the viewer accepted-follows (unlocks their `private` posts).
    let accepted = follow::accepted_out_batch(pool, viewer_id, &author_ids).await?;
    // Status ids that mention the viewer (unlocks `private`/`direct`), one query.
    let gated_ids: Vec<i64> = items
        .iter()
        .filter(|s| matches!(s.visibility.as_str(), "private" | "direct"))
        .map(|s| s.id)
        .collect();
    let mentions = mention::for_statuses(pool, &gated_ids, false).await?;
    let mentioned: HashSet<i64> = mentions
        .iter()
        .filter(|(_, accounts)| accounts.iter().any(|a| a.id == viewer_id))
        .map(|(status_id, _)| *status_id)
        .collect();

    Ok(items
        .iter()
        .filter(|item| {
            if !instance_ok.contains(&item.account_id) {
                return false;
            }
            if !instance_ok.contains(&proper_author(item)) {
                return false;
            }
            if item.account_id == viewer_id {
                return true;
            }
            if blockers.contains(&item.account_id) {
                return false;
            }
            if blockers.contains(&proper_author(item)) {
                return false;
            }
            match item.visibility.as_str() {
                "local" => local_author.contains(&item.account_id),
                "private" => accepted.contains(&item.account_id) || mentioned.contains(&item.id),
                "direct" => mentioned.contains(&item.id),
                _ => true,
            }
        })
        .cloned()
        .collect())
}

/// The Mastodon `ScheduledStatus` entity. We store the post options as typed
/// columns and rebuild the `params` object here (Mastodon keeps them as a raw
/// jsonb blob); the keys mirror what was submitted to `POST /statuses`.
pub async fn render_scheduled_status(
    pool: &PgPool,
    domain: &str,
    item: &ScheduledStatus,
) -> Result<Value, ApiError> {
    // Bound media, restored to the submitted order (find_by_ids returns id order).
    let mut rows = media::find_by_ids(pool, &item.media_ids).await?;
    rows.sort_by_key(|m| item.media_ids.iter().position(|id| *id == m.id));
    // Scheduled posts carry the owner's own local uploads (always cached).
    let media_attachments: Vec<Value> = rows.iter().map(|m| media_json(domain, m, false)).collect();

    let mut params = serde_json::Map::new();
    if item.object_type == "Article" {
        params.insert("post_kind".into(), json!("article"));
        params.insert("title".into(), json!(item.title));
    }
    params.insert("text".into(), json!(item.text));
    params.insert("visibility".into(), json!(item.visibility));
    params.insert("sensitive".into(), json!(item.sensitive));
    params.insert("spoiler_text".into(), json!(item.spoiler_text));
    params.insert("idempotency".into(), Value::Null);
    params.insert("with_rate_limit".into(), json!(false));
    params.insert(
        "in_reply_to_id".into(),
        item.in_reply_to_id
            .map_or(Value::Null, |id| json!(id.to_string())),
    );
    params.insert(
        "quoted_status_id".into(),
        item.quoted_status_id
            .map_or(Value::Null, |id| json!(id.to_string())),
    );
    params.insert("language".into(), json!(item.language));
    // Pleroma extension (P4): the captured rich-text format.
    params.insert("content_type".into(), json!(item.content_type));
    params.insert("application_id".into(), json!(item.application_id));
    // Mastodon's serializer maps the stored bitmap back onto the client
    // string (`'nobody'` when no known flag is set); a pre-column row (null)
    // has no recorded policy.
    params.insert(
        "quote_approval_policy".into(),
        item.quote_approval_policy.map_or(Value::Null, |bitmap| {
            let automatic = plamenu_ap::quote_policy::QuotePolicy::from_bitmap(bitmap).automatic();
            if automatic.public() {
                json!("public")
            } else if automatic.followers() {
                json!("followers")
            } else {
                json!("nobody")
            }
        }),
    );
    if !item.media_ids.is_empty() {
        let ids: Vec<String> = item.media_ids.iter().map(i64::to_string).collect();
        params.insert("media_ids".into(), json!(ids));
    }
    if let Some(options) = &item.poll_options {
        params.insert(
            "poll".into(),
            json!({
                "options": options,
                "expires_in": item.poll_expires_in,
                "multiple": item.poll_multiple,
                "hide_totals": item.poll_hide_totals,
            }),
        );
    }

    Ok(json!({
        "id": item.id.to_string(),
        "scheduled_at": rfc3339(item.scheduled_at)?,
        "params": Value::Object(params),
        "media_attachments": media_attachments,
    }))
}

// ---------------------------------------------------------------------------
// Admin dashboard metrics (`Admin::Metrics::*` serializers)
// ---------------------------------------------------------------------------

/// Renders a measure's `data` point date. The SQL day-axis measures emit a bare
/// `YYYY-MM-DD` (Mastodon's `row['period']` Date), while `active_users` emits a
/// full midnight-UTC RFC3339 timestamp (`date.to_time(:utc).iso8601`).
fn measure_point_date(period: time::Date, with_time: bool) -> Result<String, ApiError> {
    if with_time {
        rfc3339(period.with_time(time::Time::MIDNIGHT).assume_utc())
    } else {
        period
            .format(time::macros::format_description!("[year]-[month]-[day]"))
            .map_err(|e| ApiError::Internal(Box::new(e)))
    }
}

/// The `REST::Admin::Measure` entity. `unit` is always present (often `null`);
/// `human_value` appears only for byte-unit measures; `previous_total` only for
/// measures Mastodon flags `total_in_time_range?` (i.e. when one was computed).
pub fn measure_json(
    key: &str,
    unit: Option<&str>,
    measurement: &plamenu_db::metrics::Measurement,
    with_time_dates: bool,
) -> Result<Value, ApiError> {
    let mut data = Vec::with_capacity(measurement.data.len());
    for point in &measurement.data {
        data.push(json!({
            "date": measure_point_date(point.period, with_time_dates)?,
            "value": point.value.to_string(),
        }));
    }
    let mut out = serde_json::Map::new();
    out.insert("key".into(), Value::String(key.to_owned()));
    out.insert(
        "unit".into(),
        unit.map_or(Value::Null, |u| Value::String(u.to_owned())),
    );
    out.insert("total".into(), Value::String(measurement.total.to_string()));
    if unit == Some("bytes") {
        out.insert(
            "human_value".into(),
            Value::String(human_size(measurement.total)),
        );
    }
    if let Some(previous) = measurement.previous_total {
        out.insert("previous_total".into(), Value::String(previous.to_string()));
    }
    out.insert("data".into(), Value::Array(data));
    Ok(Value::Object(out))
}

/// The `REST::Admin::Dimension` entity: `{ key, data: [...] }`.
#[must_use]
pub fn dimension_json(key: &str, data: Vec<Value>) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("key".into(), Value::String(key.to_owned()));
    out.insert("data".into(), Value::Array(data));
    Value::Object(out)
}

/// The `REST::Admin::Cohort` list from flat retention cells. Cells arrive
/// ordered by cohort then retention period; each distinct `cohort_period`
/// becomes one cohort with a `data` array of `{date, rate, value}`. `period`
/// and `date` are bare `YYYY-MM-DD` (Mastodon serializes the underlying `Date`).
pub fn cohorts_json(
    cells: &[plamenu_db::metrics::RetentionCell],
    frequency: &str,
) -> Result<Vec<Value>, ApiError> {
    let date_only = |d: time::Date| {
        d.format(time::macros::format_description!("[year]-[month]-[day]"))
            .map_err(|e| ApiError::Internal(Box::new(e)))
    };
    let mut cohorts: Vec<Value> = Vec::new();
    let mut current_period: Option<time::Date> = None;
    let mut data: Vec<Value> = Vec::new();
    for cell in cells {
        if current_period != Some(cell.cohort_period) {
            if let Some(period) = current_period.take() {
                cohorts.push(json!({
                    "period": date_only(period)?,
                    "frequency": frequency,
                    "data": std::mem::take(&mut data),
                }));
            }
            current_period = Some(cell.cohort_period);
        }
        data.push(json!({
            "date": date_only(cell.retention_period)?,
            "rate": cell.rate,
            "value": cell.value.to_string(),
        }));
    }
    if let Some(period) = current_period {
        cohorts.push(json!({
            "period": date_only(period)?,
            "frequency": frequency,
            "data": data,
        }));
    }
    Ok(cohorts)
}

/// One dimension `data` row (`{ key, human_key, value }`).
#[must_use]
pub fn dimension_item(key: &str, human_key: &str, value: i64) -> Value {
    json!({
        "key": key,
        "human_key": human_key,
        "value": value.to_string(),
    })
}

/// `ActiveSupport#number_to_human_size` — 1024-based, 3 significant digits, the
/// `human_value` shown for byte measures/dimensions (e.g. `1234` → `"1.21 KB"`,
/// `512` → `"512 Bytes"`).
#[must_use]
pub fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1024 {
        return format!("{bytes} Bytes");
    }
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for next in UNITS {
        value /= 1024.0;
        unit = next;
        if value < 1024.0 {
            break;
        }
    }
    let decimals = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let mut formatted = format!("{value:.decimals$}");
    if formatted.contains('.') {
        formatted = formatted
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned();
    }
    format!("{formatted} {unit}")
}

/// A human-readable language name for a BCP-47/ISO-639 code, Mastodon's
/// `LanguagesHelper#standard_locale_name`. Covers the common cases and falls
/// back to the code itself for anything unmapped (`und` → "Unknown").
#[must_use]
pub fn locale_name(code: &str) -> String {
    let name = match code {
        "und" => "Unknown",
        "en" => "English",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "it" => "Italian",
        "pt" => "Portuguese",
        "nl" => "Dutch",
        "pl" => "Polish",
        "ru" => "Russian",
        "uk" => "Ukrainian",
        "ja" => "Japanese",
        "zh" => "Chinese",
        "ko" => "Korean",
        "ar" => "Arabic",
        "tr" => "Turkish",
        "sv" => "Swedish",
        "fi" => "Finnish",
        "no" | "nb" => "Norwegian",
        "da" => "Danish",
        "cs" => "Czech",
        "hu" => "Hungarian",
        "ca" => "Catalan",
        "eu" => "Basque",
        "gl" => "Galician",
        "el" => "Greek",
        "he" => "Hebrew",
        "hi" => "Hindi",
        "id" => "Indonesian",
        "vi" => "Vietnamese",
        "th" => "Thai",
        "ro" => "Romanian",
        "bg" => "Bulgarian",
        "sr" => "Serbian",
        "hr" => "Croatian",
        "sk" => "Slovak",
        "sl" => "Slovenian",
        "lt" => "Lithuanian",
        "lv" => "Latvian",
        "et" => "Estonian",
        "fa" => "Persian",
        _ => return code.to_owned(),
    };
    name.to_owned()
}
