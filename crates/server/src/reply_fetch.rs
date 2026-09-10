//! On-demand thread completion — parent resolution plus Mastodon's
//! `ActivityPub::FetchAllRepliesWorker`.
//!
//! When a local user opens a remote thread (`GET /statuses/:id/context` or the
//! web thread page), a focused orphan first hands its unresolved `inReplyTo`
//! URI to [`crate::parent_fetch`], then we enqueue the focused status for a
//! crawl of the origin's `ActivityPub` `replies` collection. Together these fill
//! both directions of the thread out of band: the missing ancestor and replies
//! we never received over federation appear on a later request.
//!
//! Safeguards match Mastodon's: an anti-amplification filter (a reply is only
//! fetched when it lives on the same host as the status it replies to), total
//! caps on replies and collection pages per crawl, and a per-status cooldown
//! (`status_reply_fetches`) so repeated opens don't hammer the origin.
//!
//! Deliberate differences from Mastodon: we do not emit the 4.x
//! `Mastodon-Async-Refresh` response header / polling endpoint — the crawl runs
//! in the background and the client (or a web page reload) sees the fetched
//! replies on its next request. We also skip Mastodon's separate shallow
//! inline-replies pass on ingest; the context-open crawl re-fetches the root
//! and picks up its inlined self-replies too.

use std::collections::HashSet;

use plamenu_ap::activity::id_of;
use plamenu_db::reply_fetch;
use plamenu_db::status::{self, Status};
use serde_json::Value;
use time::Duration;
use tokio::task::JoinHandle;
use url::Url;

use crate::AppState;
use crate::error::ApiError;

/// Don't re-crawl a status' replies more often than this (Mastodon's
/// `FETCH_REPLIES_COOLDOWN_MINUTES`).
const COOLDOWN: Duration = Duration::minutes(15);
/// Total replies ingested per crawl, across the whole tree (Mastodon caps at
/// 1000; we stay conservative).
const MAX_REPLIES: usize = 500;
/// Total `replies` collection pages fetched per crawl.
const MAX_PAGES: usize = 100;
/// A slow origin must not monopolize the single reply-crawl lane even when it
/// stays just inside every individual HTTP timeout.
const MAX_CRAWL_WALL_TIME: std::time::Duration = std::time::Duration::from_mins(2);
/// How many collection pages the worker claims per idle wake-up.
const BATCH_SIZE: i64 = 20;
const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether a status is eligible to have its replies crawled: it must be remote
/// (has an AP `uri`) and distributable (public or unlisted), like Mastodon's
/// `should_fetch_replies?`.
fn is_crawlable(status: &Status) -> bool {
    status.uri.is_some() && matches!(status.visibility.as_str(), "public" | "unlisted")
}

/// Starts bounded thread completion when an authenticated viewer opens a
/// remote, distributable status. A missing parent is resolved through the
/// in-process deduplicated fetch lane; descendants use the durable crawl queue
/// and its per-status cooldown. Both operations are best-effort and return
/// before federation completes.
pub async fn on_thread_open(state: &AppState, status: &Status) -> Result<(), ApiError> {
    if !is_crawlable(status) {
        return Ok(());
    }
    if let Some((_, parent_uri)) = status::unresolved_reply_parents(&state.pool, &[status.id])
        .await?
        .into_iter()
        .next()
    {
        crate::parent_fetch::spawn_resolves(state, vec![parent_uri]);
    }
    if reply_fetch::is_due(&state.pool, status.id, COOLDOWN).await? {
        reply_fetch::enqueue(&state.pool, status.id).await?;
    }
    Ok(())
}

/// Two URIs share a host (case-insensitively) — the anti-amplification guard:
/// we only fetch a reply hosted by the same origin as the status it replies to.
fn same_host(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (Url::parse(left), Url::parse(right)) else {
        return false;
    };
    match (left.host_str(), right.host_str()) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => false,
    }
}

/// The item IRIs of a collection page (`items` or `orderedItems`), each of
/// which may be a bare IRI string or an inlined object with an `id`.
fn collection_items(page: &Value) -> Vec<String> {
    let value = page.get("items").or_else(|| page.get("orderedItems"));
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(id_of).map(str::to_owned).collect(),
        Some(other) => id_of(other)
            .map(|id| vec![id.to_owned()])
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Resolves a Note's `replies` value to the first page object to walk: the
/// value may be an inlined collection/page, or an IRI to fetch. When it is a
/// `Collection` wrapping a `first` page, follows into that page.
async fn resolve_first_page(state: &AppState, replies: &Value) -> Option<Value> {
    let mut collection = if replies.is_object() {
        replies.clone()
    } else {
        let uri = id_of(replies)?;
        state.federation.fetch_object(uri).await.ok()?
    };
    if let Some(first) = collection.get("first") {
        if first.is_object() {
            collection = first.clone();
        } else if let Some(first_uri) = id_of(first) {
            collection = state.federation.fetch_object(first_uri).await.ok()?;
        }
    }
    Some(collection)
}

/// Walks a Note's `replies` collection, returning the reply IRIs found and how
/// many pages were fetched (bounded by `max_pages`).
async fn collect_reply_uris(
    state: &AppState,
    replies: &Value,
    max_pages: usize,
) -> (Vec<String>, usize) {
    let mut uris = Vec::new();
    let mut pages = 0;
    let Some(mut page) = resolve_first_page(state, replies).await else {
        return (uris, pages);
    };
    loop {
        if pages >= max_pages {
            break;
        }
        pages += 1;
        uris.extend(collection_items(&page));
        let Some(next_uri) = page.get("next").and_then(id_of) else {
            break;
        };
        let Ok(next_page) = state.federation.fetch_object(next_uri).await else {
            break;
        };
        page = next_page;
    }
    (uris, pages)
}

/// Total posts pulled from a context collection per backfill.
const MAX_CONTEXT_POSTS: usize = 500;
/// Pages walked across a context collection.
const MAX_CONTEXT_PAGES: usize = 50;
/// Bare-IRI activity items we will fetch to discover their post — an activities
/// collection (`contextHistory`) that paginates by IRI rather than inlining.
const MAX_ACTIVITY_FETCHES: usize = 100;

/// The raw items of a collection page (`items`/`orderedItems`), each a bare IRI
/// string or an inline object — kept whole so an activity can be dug into.
fn collection_item_values(page: &Value) -> Vec<Value> {
    match page.get("items").or_else(|| page.get("orderedItems")) {
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    }
}

/// Digs the post IRI a conversation item ultimately points at: unwrap
/// `Add`/`Announce`, then `Create`/`Update`, down to the post object (a bare
/// post object is its own id). Non-post activities (`Like`, `Delete`, …) yield
/// `None` — backfill only pulls posts, never replays foreign side effects.
fn dig_post_uri(value: &Value) -> Option<String> {
    match value.get("type").and_then(Value::as_str) {
        Some("Add" | "Announce" | "Create" | "Update") => {
            let object = value.get("object")?;
            if object.is_object() {
                dig_post_uri(object)
            } else {
                id_of(object).map(str::to_owned)
            }
        }
        Some("Note" | "Article" | "Page" | "Question") => id_of(value).map(str::to_owned),
        _ => None,
    }
}

/// Walks a conversation collection (`context` posts or `contextHistory`
/// activities) and returns the post IRIs it lists, bounded by page/item/fetch
/// caps. Posts-collection items are post IRIs directly; activities-collection
/// items are dug for their post (inline), or fetched once then dug (IRI-paged).
async fn context_post_uris(
    state: &AppState,
    collection_uri: &str,
    is_activities: bool,
) -> Vec<String> {
    let mut uris = Vec::new();
    let mut activity_fetches = 0usize;
    let start = Value::String(collection_uri.to_owned());
    let Some(mut page) = resolve_first_page(state, &start).await else {
        return uris;
    };
    let mut pages = 0usize;
    loop {
        if pages >= MAX_CONTEXT_PAGES || uris.len() >= MAX_CONTEXT_POSTS {
            break;
        }
        pages += 1;
        for item in collection_item_values(&page) {
            if uris.len() >= MAX_CONTEXT_POSTS {
                break;
            }
            if !is_activities {
                if let Some(id) = id_of(&item) {
                    uris.push(id.to_owned());
                }
            } else if item.is_object() {
                if let Some(uri) = dig_post_uri(&item) {
                    uris.push(uri);
                }
            } else if let Some(activity_iri) = id_of(&item) {
                if activity_fetches >= MAX_ACTIVITY_FETCHES {
                    continue;
                }
                activity_fetches += 1;
                if let Ok(activity) = state.federation.fetch_object(activity_iri).await
                    && let Some(uri) = dig_post_uri(&activity)
                {
                    uris.push(uri);
                }
            }
        }
        let Some(next_uri) = page.get("next").and_then(id_of) else {
            break;
        };
        let Ok(next_page) = state.federation.fetch_object(next_uri).await else {
            break;
        };
        page = next_page;
    }
    uris
}

/// FEP-f228 backfill: pull the conversation's collection (activities collection
/// preferred, else posts collection) and ingest each post from its own origin.
/// Complements the `replies` crawl — it keeps working when an interior reply's
/// origin has vanished, since the whole thread is enumerated by the owner in
/// one place. Best-effort; each post is still fetched and authenticated by its
/// own attribution, never trusted from the collection.
async fn backfill_context(state: &AppState, root: &Status) -> Result<(), ApiError> {
    let Some(ctx) = plamenu_db::conversation::context_of_status(&state.pool, root.id).await? else {
        return Ok(());
    };
    let (collection_uri, is_activities) = match (ctx.history_uri, ctx.uri) {
        (Some(history), _) => (history, true),
        (None, Some(posts)) => (posts, false),
        (None, None) => return Ok(()), // no context collection — the replies crawl covers it
    };
    let post_uris = context_post_uris(state, &collection_uri, is_activities).await;
    let mut seen: HashSet<String> = HashSet::new();
    let mut ingested = 0usize;
    for uri in post_uris {
        if !seen.insert(uri.clone()) {
            continue;
        }
        match crate::ingest::resolve_or_fetch_status(state, &uri).await {
            Ok(_) => ingested += 1,
            Err(error) => {
                tracing::debug!(error = %error.chain(), uri = %uri, "context backfill fetch failed");
            }
        }
    }
    tracing::debug!(
        root = root.id,
        ingested,
        is_activities,
        "backfilled conversation from context collection"
    );
    Ok(())
}

/// Crawls the replies collection of a remote thread rooted at `root_status_id`,
/// ingesting replies we don't have and recursing into their own replies.
pub async fn crawl_replies(state: &AppState, root_status_id: i64) -> Result<(), ApiError> {
    let Some(root) = status::find_by_id(&state.pool, root_status_id).await? else {
        return Ok(()); // deleted since being queued
    };
    let Some(root_uri) = root.uri.clone().filter(|_| is_crawlable(&root)) else {
        return Ok(());
    };
    // Start the cooldown now (Mastodon touches `fetched_replies_at` up front),
    // so concurrent thread opens stop enqueuing while this crawl runs.
    reply_fetch::mark_fetched(&state.pool, root.id).await?;

    // Pull the FEP-f228 context collection first — it enumerates the
    // whole thread in one place, so it survives an interior reply's origin
    // vanishing — then fall through to the node-by-node replies crawl.
    if let Err(error) = Box::pin(backfill_context(state, &root)).await {
        tracing::debug!(error = %error.chain(), root = root.id, "context backfill failed");
    }

    // Work-list of status URIs whose replies we still need to walk; `seen`
    // guards against cycles and re-fetching statuses we've already handled.
    let mut worklist = vec![root_uri.clone()];
    let mut seen: HashSet<String> = HashSet::from([root_uri]);
    let mut ingested = 0usize;
    let mut pages = 0usize;

    while let Some(status_uri) = worklist.pop() {
        if ingested >= MAX_REPLIES || pages >= MAX_PAGES {
            break;
        }
        let Ok(note) = state.federation.fetch_object_following(&status_uri).await else {
            continue;
        };
        let Some(replies) = note.get("replies") else {
            continue;
        };
        let (reply_uris, used) = collect_reply_uris(state, replies, MAX_PAGES - pages).await;
        pages += used;
        for reply_uri in reply_uris {
            if ingested >= MAX_REPLIES {
                break;
            }
            // Anti-amplification: only follow replies hosted alongside the
            // status they reply to, and never re-process a URI.
            if !same_host(&status_uri, &reply_uri) || !seen.insert(reply_uri.clone()) {
                continue;
            }
            ingested += 1;
            match crate::ingest::resolve_or_fetch_status(state, &reply_uri).await {
                Ok(Some(_)) => worklist.push(reply_uri),
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(error = %error.chain(), uri = %reply_uri, "reply fetch failed");
                }
            }
        }
    }
    tracing::debug!(
        root = root_status_id,
        ingested,
        pages,
        "crawled thread replies"
    );
    Ok(())
}

/// Claims and crawls one batch of due jobs; returns how many were claimed
/// (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match reply_fetch::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim reply fetch jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        // The claim leased the job rather than deleting it, so a crash before
        // the crawl finishes lets the lease expire and it runs again. A crawl
        // is single-attempt (a failed fetch is not retried), so we
        // complete the job whatever the outcome — except a job that has been
        // reclaimed past the cap, which we drop after logging.
        if job.exhausted() {
            tracing::warn!(
                status = job.status_id,
                attempts = job.attempts,
                "dropping reply crawl after too many crash reclaims"
            );
        } else {
            match tokio::time::timeout(
                MAX_CRAWL_WALL_TIME,
                Box::pin(crawl_replies(state, job.status_id)),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(error = %error.chain(), status = job.status_id, "reply crawl failed");
                }
                Err(_) => {
                    tracing::warn!(
                        status = job.status_id,
                        "reply crawl exhausted wall-clock budget"
                    );
                }
            }
        }
        if let Err(error) = reply_fetch::complete(&state.pool, job.id).await {
            tracing::error!(%error, status = job.status_id, "failed to complete reply crawl job");
        }
    }
    claimed
}

/// Runs the reply-crawl loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("reply fetch worker started");
        loop {
            if Box::pin(run_due(&state)).await == 0
                && !crate::workers::pause(&state, IDLE_POLL).await
            {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn same_host_matches_origin_only() {
        assert!(same_host(
            "https://a.test/users/x/statuses/1",
            "https://a.test/users/y/statuses/2"
        ));
        assert!(same_host(
            "https://A.test/users/x/statuses/1",
            "https://a.test/users/y/statuses/2"
        ));
        assert!(!same_host(
            "https://a.test/users/x/statuses/1",
            "https://b.test/users/y/statuses/2"
        ));
        assert!(!same_host("not a url", "https://a.test/s/1"));
    }

    #[test]
    fn collection_items_reads_iris_and_inline_objects() {
        let page = json!({
            "items": [
                "https://a.test/s/1",
                {"id": "https://a.test/s/2", "type": "Note"},
                {"type": "Note"},
                42,
            ]
        });
        assert_eq!(
            collection_items(&page),
            vec![
                "https://a.test/s/1".to_owned(),
                "https://a.test/s/2".to_owned(),
            ]
        );
        // orderedItems is accepted as a fallback; a single non-array value too.
        assert_eq!(
            collection_items(&json!({"orderedItems": "https://a.test/s/3"})),
            vec!["https://a.test/s/3".to_owned()]
        );
        assert!(collection_items(&json!({})).is_empty());
    }

    #[test]
    fn dig_post_uri_unwraps_to_the_post() {
        // Add(Create(Note)) — the FEP-171b container item shape.
        let add = json!({
            "type": "Add",
            "object": {
                "type": "Create",
                "object": {"type": "Note", "id": "https://a.test/posts/1"},
            },
        });
        assert_eq!(
            dig_post_uri(&add).as_deref(),
            Some("https://a.test/posts/1")
        );

        // Create(Note) with a bare-IRI object.
        let create = json!({"type": "Create", "object": "https://a.test/posts/2"});
        assert_eq!(
            dig_post_uri(&create).as_deref(),
            Some("https://a.test/posts/2")
        );

        // A bare post object is its own id.
        let note = json!({"type": "Note", "id": "https://a.test/posts/3"});
        assert_eq!(
            dig_post_uri(&note).as_deref(),
            Some("https://a.test/posts/3")
        );

        // Non-post activities never yield a post — no replaying side effects.
        for other in [
            json!({"type": "Like", "object": "https://a.test/posts/4"}),
            json!({"type": "Delete", "object": "https://a.test/posts/5"}),
            json!({"type": "EmojiReact", "object": "https://a.test/posts/6"}),
        ] {
            assert_eq!(dig_post_uri(&other), None, "{other}");
        }
    }
}
