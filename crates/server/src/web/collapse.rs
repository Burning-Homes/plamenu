//! Boost collapse for the built-in web client (`FEEDS_DESIGN`).
//!
//! Three people boosting the same post used to cost three cards. This pass
//! merges them into one card that names its boosters — "Alice, Bob and 3
//! others boosted" — and it runs *after* the timeline query returns, on the
//! rendered entities, so `/api/v1/timelines/*` is untouched. A
//! third-party client keeps every boost row and does its own thing with them;
//! Phanpy, for one, already ships `dedupeBoosts`.
//!
//! Nothing is dropped inside a page: a merged card is still a card, so
//! page length stays near-stable and the reader never silently loses a post.
//! The one place a row does disappear is the operator's optional lookback
//! window, where the post it boosts was already shown further up the feed —
//! there is no card left on this page to merge into.
//!
//! Two rules the caller owns:
//!
//! * **The next-page cursor is computed before collapsing.** Collapsing hoists
//!   an older row up next to a newer one, so "the last rendered card" is no
//!   longer the feed-order minimum; paging on it would skip posts. Every
//!   caller pages on the raw timeline rows instead, which this pass never
//!   touches. Thread grouping inherits the same rule.
//! * **Annotation passes that zip rows to entities positionally run first**
//!   (`pages::annotate_tag_sources`), because this one breaks that alignment.

use std::collections::{HashMap, HashSet};

use plamenu_db::status::Status as StatusRow;
use plamenu_db::user::UserSettings;
use serde_json::Value;

use crate::state::AppState;

/// How many boosters a merged card names before "and N others".
const NAMED_BOOSTERS: usize = 2;

/// The web-only render hint carrying a merged card's boosters, in feed order
/// (newest boost first): an array of Account entities the card turns into the
/// booster line, the same convention as `_tag_source` and `_group_mod`.
pub const BOOSTERS: &str = "_boosters";

/// The web-only render hint marking a merged card whose *original* is in the
/// page: the card is the post itself, so its booster line reads "also
/// boosted by …" rather than presenting it as someone's boost.
pub const ALSO_BOOSTED: &str = "_also_boosted";

/// What the operator and the reader together asked for. The operator owns the
/// master switch and the window; the reader owns their own opt-out (Mastodon
/// splits `aggregate_reblogs` and `REBLOG_FALLOFF` the same way).
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Whether to merge at all.
    pub enabled: bool,
    /// How many posts above the page the window reaches; `0` = page-local.
    pub lookback: i64,
}

impl Policy {
    pub async fn resolve(state: &AppState, reader: &UserSettings) -> Self {
        Self {
            enabled: state.boost_collapse().await && reader.reading_collapse_boosts,
            lookback: i64::from(state.boost_collapse_lookback().await),
        }
    }

    /// Whether this render needs the extra timeline query that seeds the
    /// window. The first page has nothing above it, so it never pays.
    pub fn window_for(self, cursor: Option<i64>) -> Option<(i64, i64)> {
        let cursor = cursor?;
        (self.enabled && self.lookback > 0).then_some((self.lookback, cursor))
    }
}

/// The posts already shown above `cursor`, keyed the way [`collapse`] keys a
/// card: a boost counts as its target, everything else as itself.
///
/// `head` is the newest slice of the same feed, so the walk stops at the
/// cursor row (inclusive — it was the last card of the previous page) and
/// never reaches the page being rendered. When the cursor is deeper than the
/// window, every row of `head` counts: the window is then the feed's newest N
/// entries rather than the N immediately above the page. That is deliberate,
/// and it is also what Mastodon's `REBLOG_FALLOFF` measures — its `zrevrank`
/// asks whether the original sits among the newest 80 entries of the feed, not
/// whether it sits near whatever page you happen to be reading.
#[must_use]
pub fn seen_targets(head: &[StatusRow], cursor: i64) -> HashSet<String> {
    targets_above(head.iter().map(|row| (row.id, row.reblog_of_id)), cursor)
}

/// [`seen_targets`] over the two columns it actually reads.
fn targets_above(rows: impl Iterator<Item = (i64, Option<i64>)>, cursor: i64) -> HashSet<String> {
    let mut seen = HashSet::new();
    for (id, reblog_of_id) in rows {
        seen.insert(reblog_of_id.unwrap_or(id).to_string());
        if id == cursor {
            break;
        }
    }
    seen
}

/// The post a card is *about*: what it boosts, or itself.
fn target_of(entity: &Value) -> &str {
    reblog_of(entity)
        .and_then(|inner| inner.get("id"))
        .or_else(|| entity.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn reblog_of(entity: &Value) -> Option<&Value> {
    entity.get("reblog").filter(|inner| inner.is_object())
}

/// A community's Announce is not a boost to the reader — it is the post
/// reaching the community's followers, rendered as "posted in […]". Two
/// communities carrying the same post are two facts, not a repetition, so
/// group announces sit out of the whole pass. Phanpy hard-codes the same
/// exemption (`item.reblog && !item.account?.group`).
fn is_group_announce(entity: &Value) -> bool {
    reblog_of(entity).is_some()
        && entity
            .get("account")
            .and_then(|account| account.get("group"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// One merged card under construction.
struct Merge {
    /// Index of the post itself, when the page carries it.
    original: Option<usize>,
    /// Indices of the boosts of it, in feed order.
    boosts: Vec<usize>,
}

/// What occupies one output position.
enum Slot {
    /// Rendered as it came: an original nobody boosted here, or a group
    /// announce.
    Kept(usize),
    /// A merged card, by index into the merge list.
    Merged(usize),
}

/// Merges repeated boosts in `entities` (feed order, newest first), returning
/// the cards to render. `seen` is [`seen_targets`] for the window above the
/// page, empty when the window is off.
///
/// The merged card takes the position of the group's **newest** occurrence,
/// and shows the original when the page carries it — which can hoist an
/// older row up. Boosters accumulate in feed order, deduplicated by account.
#[must_use]
pub fn collapse(entities: Vec<Value>, seen: &HashSet<String>) -> Vec<Value> {
    let mut merges: Vec<Merge> = Vec::new();
    let mut by_target: HashMap<&str, usize> = HashMap::new();
    let mut plan: Vec<Slot> = Vec::new();

    for (i, entity) in entities.iter().enumerate() {
        if is_group_announce(entity) {
            plan.push(Slot::Kept(i));
            continue;
        }
        let boost = reblog_of(entity).is_some();
        let target = target_of(entity);
        // Already shown above this page, and the card it would merge into is
        // not here to merge with. Only a boost goes: the post itself is the
        // content, never a repetition of it.
        if boost && seen.contains(target) {
            continue;
        }
        let merge = *by_target.entry(target).or_insert_with(|| {
            merges.push(Merge {
                original: None,
                boosts: Vec::new(),
            });
            plan.push(Slot::Merged(merges.len() - 1));
            merges.len() - 1
        });
        if boost {
            merges[merge].boosts.push(i);
        } else {
            merges[merge].original = Some(i);
        }
    }

    let mut slots: Vec<Option<Value>> = entities.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(plan.len());
    for slot in plan {
        let merge = match slot {
            Slot::Kept(i) => {
                out.push(slots[i].take().unwrap_or(Value::Null));
                continue;
            }
            Slot::Merged(merge) => &merges[merge],
        };
        // Collected before the host card is moved out, because the host may be
        // one of these boosts and would take its own account with it.
        let boosters = booster_accounts(&slots, &merge.boosts);
        let Some(host) = merge.original.or_else(|| merge.boosts.first().copied()) else {
            continue;
        };
        let mut card = slots[host].take().unwrap_or(Value::Null);
        // A post nobody boosted here, and a lone boost with no original beside
        // it, are exactly today's cards — left untouched, so the common page
        // renders as it did before.
        let one_card = if merge.original.is_some() {
            !boosters.is_empty()
        } else {
            boosters.len() > 1
        };
        if one_card && let Some(object) = card.as_object_mut() {
            if merge.original.is_some() {
                object.insert(ALSO_BOOSTED.to_owned(), Value::Bool(true));
            }
            object.insert(BOOSTERS.to_owned(), Value::Array(boosters));
        }
        out.push(card);
    }
    out
}

/// The boosters' Account entities in feed order, one per account: a repeat
/// boost by the same person (unboost, reboost) is one name, not two.
fn booster_accounts(slots: &[Option<Value>], boosts: &[usize]) -> Vec<Value> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut accounts = Vec::new();
    for &i in boosts {
        let Some(account) = slots[i].as_ref().and_then(|entity| entity.get("account")) else {
            continue;
        };
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if seen.insert(id) {
            accounts.push(account.clone());
        }
    }
    accounts
}

/// How a merged card's booster list splits into named accounts and a count of
/// the rest — `(named, others)`, where `others` is 0 when everyone is named.
#[must_use]
pub fn split_boosters(boosters: &[Value]) -> (&[Value], usize) {
    if boosters.len() <= NAMED_BOOSTERS + 1 {
        return (boosters, 0);
    }
    (&boosters[..NAMED_BOOSTERS], boosters.len() - NAMED_BOOSTERS)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::collections::HashSet;

    use super::{ALSO_BOOSTED, BOOSTERS, collapse, split_boosters, targets_above};

    /// A plain post entity.
    fn post(id: &str, author: &str) -> serde_json::Value {
        json!({ "id": id, "account": { "id": author, "group": false } })
    }

    /// A boost of `target` by `booster`.
    fn boost(id: &str, booster: &str, target: &serde_json::Value) -> serde_json::Value {
        json!({
            "id": id,
            "account": { "id": booster, "group": false },
            "reblog": target,
        })
    }

    /// A community's Announce wrapper.
    fn announce(id: &str, community: &str, target: &serde_json::Value) -> serde_json::Value {
        json!({
            "id": id,
            "account": { "id": community, "group": true },
            "reblog": target,
        })
    }

    fn ids(entities: &[serde_json::Value]) -> Vec<String> {
        entities
            .iter()
            .map(|entity| entity["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn boosters(entity: &serde_json::Value) -> Vec<String> {
        entity[BOOSTERS]
            .as_array()
            .map(|accounts| {
                accounts
                    .iter()
                    .map(|account| account["id"].as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn three_boosts_of_one_post_become_one_card() {
        let target = post("100", "9");
        let page = vec![
            boost("30", "3", &target),
            boost("20", "2", &target),
            boost("10", "1", &target),
        ];
        let out = collapse(page, &HashSet::default());
        // The newest boost keeps the position and carries every booster.
        assert_eq!(ids(&out), ["30"]);
        assert_eq!(boosters(&out[0]), ["3", "2", "1"]);
        assert!(out[0].get(ALSO_BOOSTED).is_none());
    }

    #[test]
    fn the_original_wins_the_card_when_the_page_carries_it() {
        let target = post("100", "9");
        let page = vec![
            boost("30", "3", &target),
            post("25", "8"),
            target.clone(),
            boost("10", "1", &target),
        ];
        let out = collapse(page, &HashSet::default());
        // The post is hoisted to the newest occurrence's slot, and says so.
        assert_eq!(ids(&out), ["100", "25"]);
        assert_eq!(out[0][ALSO_BOOSTED], serde_json::Value::Bool(true));
        assert_eq!(boosters(&out[0]), ["3", "1"]);
    }

    #[test]
    fn one_booster_alone_is_left_exactly_as_it_came() {
        let target = post("100", "9");
        let page = vec![boost("30", "3", &target), post("25", "8")];
        let out = collapse(page.clone(), &HashSet::default());
        assert_eq!(out, page);
    }

    #[test]
    fn the_same_account_boosting_twice_is_named_once() {
        let target = post("100", "9");
        let page = vec![
            boost("30", "3", &target),
            boost("20", "3", &target),
            boost("10", "1", &target),
        ];
        let out = collapse(page, &HashSet::default());
        assert_eq!(boosters(&out[0]), ["3", "1"]);
    }

    #[test]
    fn group_announces_never_merge_with_each_other_or_with_boosts() {
        let target = post("100", "9");
        let page = vec![
            announce("40", "500", &target),
            announce("30", "501", &target),
            boost("20", "2", &target),
            boost("10", "1", &target),
        ];
        let out = collapse(page, &HashSet::default());
        // Both communities keep their card; the two plain boosts merge into one.
        assert_eq!(ids(&out), ["40", "30", "20"]);
        assert_eq!(boosters(&out[2]), ["2", "1"]);
    }

    #[test]
    fn a_boost_of_something_shown_above_the_page_is_dropped() {
        let target = post("100", "9");
        let page = vec![boost("30", "3", &target), post("25", "8")];
        let seen = ["100".to_owned()].into_iter().collect();
        let out = collapse(page, &seen);
        assert_eq!(ids(&out), ["25"]);
    }

    #[test]
    fn the_post_itself_survives_a_window_that_saw_it_boosted() {
        let target = post("100", "9");
        let page = vec![target.clone(), boost("30", "3", &target)];
        let seen = ["100".to_owned()].into_iter().collect();
        let out = collapse(page, &seen);
        // The boost goes, the post stays — and with no booster to name, it is
        // the plain card it always was.
        assert_eq!(ids(&out), ["100"]);
        assert!(out[0].get(BOOSTERS).is_none());
    }

    #[test]
    fn the_window_stops_at_the_cursor_row() {
        // Rows are (id, reblog_of_id) newest first; the cursor is the previous
        // page's last card, so everything after it is the page being rendered.
        let head = [(50, None), (40, Some(9)), (30, None), (20, Some(8))];
        let seen = targets_above(head.into_iter(), 30);
        assert_eq!(seen.len(), 3);
        for key in ["50", "9", "30"] {
            assert!(seen.contains(key), "{key} missing");
        }
        // Row 20 is below the cursor — the page itself, not the window.
        assert!(!seen.contains("8"));
    }

    #[test]
    fn a_cursor_deeper_than_the_window_counts_the_whole_head() {
        let head = [(50, None), (40, Some(9))];
        let seen = targets_above(head.into_iter(), 3);
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn booster_names_are_capped_before_the_others_count() {
        let accounts: Vec<_> = (0..5).map(|i| json!({ "id": i.to_string() })).collect();
        let (named, others) = split_boosters(&accounts);
        assert_eq!(named.len(), 2);
        assert_eq!(others, 3);
        // Three fit without an "and 1 other" that saves nothing.
        let (named, others) = split_boosters(&accounts[..3]);
        assert_eq!(named.len(), 3);
        assert_eq!(others, 0);
    }
}
