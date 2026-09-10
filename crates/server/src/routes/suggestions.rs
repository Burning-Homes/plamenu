//! Follow suggestions: `GET /api/v1/suggestions` (accounts only, deprecated),
//! `GET /api/v2/suggestions` (source-annotated), and
//! `DELETE /api/v1/suggestions/{id}` (dismiss). Candidates come from
//! [`plamenu_db::suggestion`]'s three SQL sources, merged into one deduped,
//! source-labelled list.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use plamenu_db::account::{self, Account};
use plamenu_db::suggestion;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::CurrentUser;
use crate::entities::render_accounts;
use crate::error::ApiError;
use crate::state::AppState;

/// Mastodon's `DEFAULT_ACCOUNTS_LIMIT`.
const DEFAULT_LIMIT: i64 = 40;
/// How many candidates each source contributes before merging.
const SOURCE_BATCH: i64 = 40;

#[derive(Deserialize)]
pub struct SuggestionsQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

/// Maps a raw source to Mastodon's legacy single `source` value
/// (`LEGACY_SOURCE_TYPE_MAP`).
fn legacy_source(source: &str) -> &'static str {
    match source {
        "friends_of_friends" | "similar_to_recently_followed" => "past_interactions",
        "featured" => "staff",
        // most_followed / most_interactions
        _ => "global",
    }
}

/// The ordered, deduped page of `(account, sources)` for a viewer. Sources are
/// tried in priority order (friends-of-friends, then the two global rankings);
/// an account keeps every source it matched, and first appearance sets its
/// position. Shared with the web Explore page.
pub(crate) async fn page(
    state: &AppState,
    viewer: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<(Account, Vec<&'static str>)>, ApiError> {
    let pool = &state.pool;
    let batches = [
        (
            suggestion::friends_of_friends(pool, viewer, SOURCE_BATCH).await?,
            "friends_of_friends",
        ),
        (
            suggestion::most_followed(pool, viewer, SOURCE_BATCH).await?,
            "most_followed",
        ),
        (
            suggestion::most_interactions(pool, viewer, SOURCE_BATCH).await?,
            "most_interactions",
        ),
    ];

    let mut order: Vec<i64> = Vec::new();
    let mut sources: HashMap<i64, Vec<&'static str>> = HashMap::new();
    for (ids, label) in batches {
        for id in ids {
            let entry = sources.entry(id).or_default();
            if !entry.contains(&label) {
                entry.push(label);
            }
            if !order.contains(&id) {
                order.push(id);
            }
        }
    }

    let offset = usize::try_from(offset.max(0)).unwrap_or(0);
    let limit = usize::try_from(limit.clamp(1, 80)).unwrap_or(usize::MAX);
    let page_ids: Vec<i64> = order.into_iter().skip(offset).take(limit).collect();
    let fetched: HashMap<i64, Account> = account::find_by_ids(pool, &page_ids)
        .await?
        .into_iter()
        .map(|account| (account.id, account))
        .collect();
    // Preserve suggestion ranking; drop any id whose account vanished.
    let out = page_ids
        .into_iter()
        .filter_map(|id| {
            fetched
                .get(&id)
                .cloned()
                .map(|account| (account, sources.remove(&id).unwrap_or_default()))
        })
        .collect();
    Ok(out)
}

/// `GET /api/v2/suggestions` — source-annotated suggestions.
pub async fn index_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<SuggestionsQuery>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:accounts")?;
    let page = page(
        &state,
        current.account.id,
        query.limit.unwrap_or(DEFAULT_LIMIT),
        query.offset.unwrap_or(0),
    )
    .await?;
    let accounts: Vec<_> = page.iter().map(|(account, _)| account.clone()).collect();
    let entities = render_accounts(
        &state.pool,
        &state.config.domain,
        &accounts,
        Some(current.account.id),
    )
    .await?;
    let out: Vec<Value> = page
        .iter()
        .zip(entities)
        .map(|((_, srcs), account_entity)| {
            json!({
                "source": legacy_source(srcs.first().copied().unwrap_or("global")),
                "sources": srcs,
                "account": account_entity,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/suggestions` — the deprecated flat account list.
pub async fn index_v1(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<SuggestionsQuery>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:accounts")?;
    let page = page(
        &state,
        current.account.id,
        query.limit.unwrap_or(DEFAULT_LIMIT),
        query.offset.unwrap_or(0),
    )
    .await?;
    let accounts: Vec<_> = page.iter().map(|(account, _)| account.clone()).collect();
    let out = render_accounts(
        &state.pool,
        &state.config.domain,
        &accounts,
        Some(current.account.id),
    )
    .await?;
    Ok(Json(Value::Array(out)))
}

/// `DELETE /api/v1/suggestions/{id}` — dismiss a suggestion so it never
/// resurfaces (Mastodon answers `{}`).
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    suggestion::suppress(&state.pool, current.account.id, id).await?;
    Ok(Json(json!({})))
}
