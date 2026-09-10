//! Content-filter matching — Mastodon's `CustomFilter.apply_cached_filters`.
//!
//! A viewer's unexpired filters are loaded once per render batch and compiled
//! into a keyword regex union plus a set of pinned status ids. Each status is
//! matched against them, producing the `filtered` array (a list of
//! `FilterResult`s) clients use to decide how to present it. Matching is
//! context-agnostic, like Mastodon: every matching filter is returned and the
//! client applies each one's `context`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use plamenu_db::PgPool;
use plamenu_db::custom_filter::{self, ActiveFilter, CustomFilter, CustomFilterKeyword};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::entities::rfc3339;
use crate::error::ApiError;

/// A viewer's active filter compiled for matching.
pub struct CompiledFilter {
    pub filter: CustomFilter,
    /// The union of the filter's keyword regexes (`None` when it has none).
    keyword_union: Option<Regex>,
    /// The statuses pinned to this filter (`custom_filter_statuses`).
    status_ids: Vec<i64>,
}

/// How long a viewer's compiled filters are reused before recompiling from the
/// database. Signed-in status rendering matches every page
/// against these, and previously reloaded every filter row and rebuilt every
/// keyword regex on *every* render; a modest cache turns that back-to-back
/// timeline/notification/thread cost into one load-and-compile. Short enough
/// that a change a caller forgot to [`invalidate`] still takes effect within a
/// page or two.
const FILTER_CACHE_TTL: Duration = Duration::from_secs(30);
/// The most accounts whose compiled filters are cached at once; bounds resident
/// memory. A full cache drops its stalest entries before inserting a new one.
const FILTER_CACHE_MAX: usize = 4_096;

struct CacheEntry {
    stored: Instant,
    filters: Arc<Vec<CompiledFilter>>,
}

/// Process-global compiled-filter cache. Account ids are process-unique
/// snowflakes and the server is single-writer, so keying by account id alone is
/// safe across the one live instance; the entries are pure functions of the
/// account's stored filters, refreshed on mutation or TTL lapse.
static FILTER_CACHE: LazyLock<Mutex<HashMap<i64, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Drops a viewer's cached compiled filters so the next render recompiles from
/// the database. Every filter / keyword / pinned-status mutation calls this for
/// the owning account; the TTL is only a backstop.
pub fn invalidate(account_id: i64) {
    if let Ok(mut cache) = FILTER_CACHE.lock() {
        cache.remove(&account_id);
    }
}

fn cached(account_id: i64) -> Option<Arc<Vec<CompiledFilter>>> {
    let cache = FILTER_CACHE.lock().ok()?;
    let entry = cache.get(&account_id)?;
    (entry.stored.elapsed() < FILTER_CACHE_TTL).then(|| Arc::clone(&entry.filters))
}

fn store(account_id: i64, filters: &Arc<Vec<CompiledFilter>>) {
    let Ok(mut cache) = FILTER_CACHE.lock() else {
        return;
    };
    if cache.len() >= FILTER_CACHE_MAX && !cache.contains_key(&account_id) {
        // Drop everything already past its TTL; if all are fresh, evict the
        // single oldest so an insert always has room within the bound.
        let before = cache.len();
        cache.retain(|_, entry| entry.stored.elapsed() < FILTER_CACHE_TTL);
        if cache.len() == before
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.stored)
                .map(|(&id, _)| id)
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        account_id,
        CacheEntry {
            stored: Instant::now(),
            filters: Arc::clone(filters),
        },
    );
}

/// Loads and compiles `account_id`'s unexpired filters, reusing a recently
/// compiled set when one is cached. An invalid keyword regex
/// (should not happen — keywords are escaped) drops that filter's keyword
/// matching rather than erroring the whole render.
pub async fn compiled_filters(
    pool: &PgPool,
    account_id: i64,
) -> Result<Arc<Vec<CompiledFilter>>, ApiError> {
    let mut sets = compiled_filters_for_set(pool, &[account_id]).await?;
    // The set form answers every requested viewer, so this cannot miss.
    sets.remove(&account_id).ok_or(ApiError::NotFound)
}

/// [`compiled_filters`] across a set of viewers: cached sets are reused, and
/// every cache miss loads in one batched pass (N+1 decisions) — the
/// streaming fan-out's filter loader. The singular form is a one-element call
/// into this, so there is no second implementation to drift.
pub async fn compiled_filters_for_set(
    pool: &PgPool,
    viewer_ids: &[i64],
) -> Result<HashMap<i64, Arc<Vec<CompiledFilter>>>, ApiError> {
    let mut out: HashMap<i64, Arc<Vec<CompiledFilter>>> = HashMap::with_capacity(viewer_ids.len());
    let mut misses: Vec<i64> = Vec::new();
    for &account_id in viewer_ids {
        if out.contains_key(&account_id) {
            continue;
        }
        if let Some(hit) = cached(account_id) {
            out.insert(account_id, hit);
        } else {
            misses.push(account_id);
        }
    }
    if misses.is_empty() {
        return Ok(out);
    }
    misses.sort_unstable();
    misses.dedup();
    let mut loaded = custom_filter::active_for_many(pool, &misses).await?;
    for account_id in misses {
        let compiled = Arc::new(
            loaded
                .remove(&account_id)
                .unwrap_or_default()
                .into_iter()
                .map(compile)
                .collect::<Vec<_>>(),
        );
        store(account_id, &compiled);
        out.insert(account_id, compiled);
    }
    Ok(out)
}

fn compile(active: ActiveFilter) -> CompiledFilter {
    let keyword_union = keyword_union_pattern(&active.keywords).and_then(|pattern| {
        RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .build()
            .ok()
    });
    CompiledFilter {
        filter: active.filter,
        keyword_union,
        status_ids: active.status_ids,
    }
}

/// Ruby's `[[:word:]]`: ASCII/Unicode alphanumerics plus underscore.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// One keyword's sub-pattern, mirroring `CustomFilterKeyword#to_regex`:
/// `\b` boundaries (only against word characters) for whole-word keywords.
fn keyword_subpattern(keyword: &CustomFilterKeyword) -> Option<String> {
    if keyword.keyword.is_empty() {
        return None;
    }
    let escaped = regex::escape(&keyword.keyword);
    if keyword.whole_word {
        let start = keyword.keyword.chars().next().is_some_and(is_word_char);
        let end = keyword.keyword.chars().last().is_some_and(is_word_char);
        let sb = if start { r"\b" } else { "" };
        let eb = if end { r"\b" } else { "" };
        Some(format!("(?:{sb}{escaped}{eb})"))
    } else {
        Some(format!("(?:{escaped})"))
    }
}

/// `Regexp.union` of a filter's keywords — `None` when none are usable.
fn keyword_union_pattern(keywords: &[CustomFilterKeyword]) -> Option<String> {
    let parts: Vec<String> = keywords.iter().filter_map(keyword_subpattern).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("|"))
    }
}

/// Rails `strip_tags` followed by entity decoding, but with each tag turned
/// into a space so words on either side of an element boundary stay distinct
/// for whole-word matching. Also serves the web report form's post excerpts.
pub(crate) fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
}

/// Decodes the HTML entities sanitized status content can carry.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find(';').filter(|&e| e <= 32) else {
            out.push('&');
            rest = &rest[start + 1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    n.strip_prefix(['x', 'X']).map_or_else(
                        || n.parse::<u32>().ok(),
                        |h| u32::from_str_radix(h, 16).ok(),
                    )
                })
                .and_then(char::from_u32),
        };
        if let Some(c) = decoded {
            out.push(c);
            rest = &rest[start + end + 1..];
        } else {
            out.push('&');
            rest = &rest[start + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// Mastodon's `Status#searchable_text`: spoiler, plain-text body, poll options
/// and media descriptions, joined by blank lines (the units keywords match
/// against).
#[must_use]
pub fn searchable_text(
    spoiler: &str,
    content_html: &str,
    poll_options: &[String],
    media_descriptions: &[&str],
) -> String {
    let mut parts = vec![spoiler.to_owned(), plain_text(content_html)];
    if !poll_options.is_empty() {
        parts.push(poll_options.join("\n\n"));
    }
    if !media_descriptions.is_empty() {
        parts.push(media_descriptions.join("\n\n"));
    }
    parts.join("\n\n")
}

/// The `filtered` attribute for one status: every matching filter as a
/// `FilterResult`. `match_ids` are the ids checked against pinned statuses —
/// `[status.id]`, or `[boost.id, target.id]` for a boost — in that order.
pub fn filtered_value(
    filters: &[CompiledFilter],
    searchable: &str,
    match_ids: &[i64],
) -> Result<Value, ApiError> {
    let mut results = Vec::new();
    for compiled in filters {
        let keyword_match = compiled
            .keyword_union
            .as_ref()
            .and_then(|re| re.find(searchable))
            .map(|m| m.as_str().to_owned());
        let status_matches: Vec<i64> = if compiled.status_ids.is_empty() {
            Vec::new()
        } else {
            match_ids
                .iter()
                .copied()
                .filter(|id| compiled.status_ids.contains(id))
                .collect()
        };
        if keyword_match.is_none() && status_matches.is_empty() {
            continue;
        }
        results.push(json!({
            "filter": filter_summary_json(&compiled.filter)?,
            "keyword_matches": keyword_match.into_iter().collect::<Vec<_>>(),
            "status_matches": status_matches
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        }));
    }
    Ok(Value::Array(results))
}

/// The `Filter` entity as it appears inside a `FilterResult` — Mastodon's
/// `FilterSerializer` without the `keywords`/`statuses` rules.
pub fn filter_summary_json(filter: &CustomFilter) -> Result<Value, ApiError> {
    let expires_at = match filter.expires_at {
        Some(at) => Some(rfc3339(at)?),
        None => None,
    };
    Ok(json!({
        "id": filter.id.to_string(),
        "title": filter.title,
        "context": filter.context,
        "expires_at": expires_at,
        "filter_action": filter.action,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyword(text: &str, whole_word: bool) -> CustomFilterKeyword {
        CustomFilterKeyword {
            id: 0,
            custom_filter_id: 0,
            keyword: text.to_owned(),
            whole_word,
        }
    }

    fn compiled(keywords: &[CustomFilterKeyword], status_ids: Vec<i64>) -> CompiledFilter {
        compile(ActiveFilter {
            filter: CustomFilter {
                id: 1,
                account_id: 1,
                title: "f".to_owned(),
                action: "warn".to_owned(),
                context: vec!["home".to_owned()],
                expires_at: None,
            },
            keywords: keywords.to_vec(),
            status_ids,
        })
    }

    #[test]
    fn whole_word_respects_boundaries() {
        let filters = [compiled(&[keyword("cat", true)], vec![])];
        let body = filtered_value(
            &filters,
            &searchable_text("", "<p>the cat sat</p>", &[], &[]),
            &[10],
        )
        .unwrap();
        assert_eq!(body.as_array().unwrap().len(), 1, "matches whole word");
        assert_eq!(body[0]["keyword_matches"][0], "cat");

        // No match inside a longer word.
        let none = filtered_value(
            &filters,
            &searchable_text("", "<p>concatenate</p>", &[], &[]),
            &[10],
        )
        .unwrap();
        assert!(none.as_array().unwrap().is_empty());
    }

    #[test]
    fn non_whole_word_matches_substring() {
        let filters = [compiled(&[keyword("cat", false)], vec![])];
        let hit = filtered_value(
            &filters,
            &searchable_text("", "<p>concatenate</p>", &[], &[]),
            &[10],
        )
        .unwrap();
        assert_eq!(hit.as_array().unwrap().len(), 1);
        assert_eq!(hit[0]["keyword_matches"][0], "cat");
    }

    #[test]
    fn matching_is_case_insensitive_and_searches_spoiler_and_media() {
        let filters = [compiled(&[keyword("Spoiler", true)], vec![])];
        let from_cw = filtered_value(
            &filters,
            &searchable_text("SPOILER ahead", "<p>hi</p>", &[], &[]),
            &[10],
        )
        .unwrap();
        assert_eq!(from_cw.as_array().unwrap().len(), 1);
        let from_alt = filtered_value(
            &filters,
            &searchable_text("", "<p>hi</p>", &[], &["a big spoiler"]),
            &[10],
        )
        .unwrap();
        assert_eq!(from_alt.as_array().unwrap().len(), 1);
    }

    #[test]
    fn status_pin_matches_boost_and_target_ids() {
        let filters = [compiled(&[], vec![99])];
        // A boost wrapper checks [boost_id, target_id]; the pinned target hits.
        let hit = filtered_value(&filters, "nothing", &[42, 99]).unwrap();
        assert_eq!(hit.as_array().unwrap().len(), 1);
        assert_eq!(hit[0]["status_matches"], json!(["99"]));
        assert!(hit[0]["keyword_matches"].as_array().unwrap().is_empty());
        // An unrelated status with no keyword and no pin match: nothing.
        let none = filtered_value(&filters, "nothing", &[7]).unwrap();
        assert!(none.as_array().unwrap().is_empty());
    }

    #[test]
    fn empty_keyword_does_not_match_everything() {
        let filters = [compiled(&[keyword("", true)], vec![])];
        let none = filtered_value(
            &filters,
            &searchable_text("", "<p>anything</p>", &[], &[]),
            &[10],
        )
        .unwrap();
        assert!(none.as_array().unwrap().is_empty());
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn compiled_filters_are_cached_and_invalidated(pool: PgPool) {
        use plamenu_db::account::{self, NewLocalAccount};
        use plamenu_db::custom_filter::NewKeyword;

        let account = account::create_local(
            &pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let home = vec!["home".to_owned()];
        let mk = |word: &str| NewKeyword {
            keyword: word.to_owned(),
            whole_word: false,
        };
        custom_filter::create(&pool, account.id, "f", "warn", &home, None, &[mk("cat")])
            .await
            .unwrap();

        // First render compiles and caches; the second reuses the very same Arc
        // rather than reloading and recompiling.
        let first = compiled_filters(&pool, account.id).await.unwrap();
        assert_eq!(first.len(), 1);
        let second = compiled_filters(&pool, account.id).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second), "the compiled set is cached");

        // A filter added out of band is not seen until the cache is dropped —
        // proving the cache is real — and `invalidate` (called by every filter
        // mutation route) then forces a fresh compile.
        custom_filter::create(&pool, account.id, "g", "warn", &home, None, &[mk("dog")])
            .await
            .unwrap();
        assert_eq!(
            compiled_filters(&pool, account.id).await.unwrap().len(),
            1,
            "the cache still holds the pre-mutation set"
        );
        invalidate(account.id);
        let fresh = compiled_filters(&pool, account.id).await.unwrap();
        assert_eq!(fresh.len(), 2, "invalidation forces a recompile");
        assert!(!Arc::ptr_eq(&first, &fresh));
    }
}
