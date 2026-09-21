//! Making a reply readable in a feed, for the built-in web client
//! (`FEEDS_DESIGN`). Two passes, both presentation-only:
//!
//! **[`group`]** — a reply and the post it answers routinely arrive in the same
//! page and land far apart in it, because the feed is ordered by time and a
//! conversation is not. This links the ones already on the page and renders
//! them together, parent first, so the reader gets the exchange instead of two
//! disconnected halves. It runs on the **collapsed** card list
//! ([`super::collapse`]), after that pass has decided what a card is, and it
//! fetches nothing.
//!
//! **[`annotate_reply_peeks`]** — when the parent is *not* on the page, the
//! card says "Replying to @someone" and stops there, which tells the reader who
//! but never what. This attaches a one-line look at the parent, batched for the
//! whole page, and hands the never-fetched ones to
//! [`crate::parent_fetch`] so a later view can show them.
//!
//! Both are applied unconditionally, as Phanpy applies `groupContext`: unlike
//! the boost collapse, nothing is hidden from the reader here, so there is no
//! knob to weigh. (The reversal, if one is ever wanted, is a reader toggle
//! shaped exactly like `reading_collapse_boosts`.)
//!
//! Two rules inherited from the feed design, both already satisfied by the callers:
//!
//! * **The next-page cursor is computed before grouping**, because grouping
//!   hoists an older post up beside a newer one. Every caller pages on the raw
//!   timeline rows, which this pass never sees.
//! * **Positional row-to-entity passes run first** (`pages::annotate_tag_sources`).

use std::collections::{HashMap, HashSet};

use plamenu_db::{account, custom_emoji, mute, status};
use serde_json::{Value, json};

use crate::entities::{allow_direct_media, avatar_url, filter_viewable};
use crate::error::ApiError;
use crate::state::AppState;

/// Members past this many put the middle of the group behind a disclosure, so
/// a long exchange does not push the rest of the feed off the screen. Three is
/// Phanpy's threshold (`timeline.jsx`), and it is the smallest group where
/// hiding anything saves a card at all.
const SHOWN_WHOLE: usize = 3;

/// What a group is: one person talking to themselves, or several talking to
/// each other. The distinction is Phanpy's, and it is worth keeping because
/// the two read differently — a thread is one post continued, a conversation
/// is an exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Every member shares an author.
    Thread,
    /// Members by two or more authors.
    Conversation,
}

/// A run of cards belonging to one exchange, in reading order: the member
/// nothing else on the page replies *from* comes first, then its replies,
/// depth-first and oldest-first among siblings.
pub struct Group {
    pub kind: Kind,
    pub members: Vec<Value>,
}

impl Group {
    /// The members hidden behind the disclosure — everything between the first
    /// card and the last — and empty for a group short enough to render whole.
    #[must_use]
    pub fn folded(&self) -> &[Value] {
        if self.members.len() > SHOWN_WHOLE {
            &self.members[1..self.members.len() - 1]
        } else {
            &[]
        }
    }

    /// The members rendered plainly, in order, with [`Group::folded`] removed
    /// from the middle when the group is long enough to fold.
    #[must_use]
    pub fn shown(&self) -> (&Value, &[Value], &Value) {
        let last = self.members.len() - 1;
        let middle = if self.members.len() > SHOWN_WHOLE {
            &[][..]
        } else {
            &self.members[1..last]
        };
        (&self.members[0], middle, &self.members[last])
    }
}

/// One position in the rendered feed.
pub enum Card {
    /// A card with no relatives on this page — the overwhelming majority.
    Single(Value),
    /// Two or more cards of one exchange, rendered together.
    Group(Group),
}

/// Whether a card can join a group.
///
/// Boosts sit out, as they do in Phanpy (`timeline-utils.js:174`): a boost is
/// in the feed because someone repeated it, not because of what it answers,
/// and its own `in_reply_to_id` belongs to the boosted post rather than to the
/// wrapper. Group announces are boosts, so the group exemption comes along.
///
/// **A post hoisted up because the page also carried boosts of it does
/// join**, and that is the deliberate call this slice owed. It is a post, not a
/// boost; its booster line rides on the card and survives the move; and
/// excluding it fragments a thread exactly where a thread is most worth reading
/// — someone boosted one post out of an exchange the reader can see the rest
/// of. A four-post chain with one boosted member would otherwise render as a
/// lone card, a hoisted card and a two-card group. The group takes the hoisted
/// card's slot when that card is the newest member, so in the common case the
/// boost's placement is kept as well.
fn joins(entity: &Value) -> bool {
    !entity.get("reblog").is_some_and(Value::is_object)
}

fn id_of(entity: &Value) -> &str {
    entity.get("id").and_then(Value::as_str).unwrap_or_default()
}

fn parent_of(entity: &Value) -> Option<&str> {
    entity.get("in_reply_to_id").and_then(Value::as_str)
}

fn author_of(entity: &Value) -> &str {
    entity
        .get("account")
        .and_then(|account| account.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Snowflake ids sort as numbers, not as strings — `"9"` is older than `"10"`
/// but sorts after it. Anything unparseable sorts last and keeps its relative
/// order, which is what a stable sort gives us.
fn sort_key(entity: &Value) -> u64 {
    id_of(entity).parse().unwrap_or(u64::MAX)
}

/// Groups the replies and parents already present in `entities` (feed order,
/// newest first), returning the cards to render.
///
/// A group takes the position of its **newest** member — the first of them the
/// reader would have met — and renders parent-first from there, which can hoist
/// an older post up the page. Cards with no relative here are returned
/// untouched and in place, so the common feed renders exactly as it did.
#[must_use]
pub fn group(entities: Vec<Value>) -> Vec<Card> {
    let by_id: HashMap<&str, usize> = entities
        .iter()
        .enumerate()
        .filter(|(_, entity)| joins(entity))
        .map(|(i, entity)| (id_of(entity), i))
        .collect();

    // Each card has at most one parent here, so the links form a forest: every
    // component is a tree whose root is the member whose own parent is not on
    // this page. No cycle is possible unless two cards share an id, and `by_id`
    // has already collapsed those to one.
    let parent: Vec<Option<usize>> = entities
        .iter()
        .enumerate()
        .map(|(i, entity)| {
            if !joins(entity) {
                return None;
            }
            parent_of(entity)
                .and_then(|parent| by_id.get(parent).copied())
                .filter(|&p| p != i)
        })
        .collect();
    if parent.iter().all(Option::is_none) {
        return entities.into_iter().map(Card::Single).collect();
    }

    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, &p) in parent.iter().enumerate() {
        if let Some(p) = p {
            children.entry(p).or_default().push(i);
        }
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|&i| sort_key(&entities[i]));
    }

    // Reading order per component, from its root down. Walking from the root
    // rather than from the first member met means the group reads parent-first
    // however the feed happened to order it.
    let mut order: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut root_of: Vec<Option<usize>> = vec![None; entities.len()];
    for i in 0..entities.len() {
        if !joins(&entities[i]) || parent[i].is_some() {
            continue;
        }
        let mut members = Vec::new();
        let mut stack = vec![i];
        while let Some(node) = stack.pop() {
            members.push(node);
            root_of[node] = Some(i);
            if let Some(kids) = children.get(&node) {
                stack.extend(kids.iter().rev().copied());
            }
        }
        if members.len() > 1 {
            order.insert(i, members);
        }
    }

    let mut slots: Vec<Option<Value>> = entities.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(slots.len());
    for i in 0..slots.len() {
        if slots[i].is_none() {
            continue; // already emitted inside its group
        }
        let Some(members) = root_of[i].and_then(|root| order.remove(&root)) else {
            out.push(Card::Single(slots[i].take().unwrap_or(Value::Null)));
            continue;
        };
        let members: Vec<Value> = members
            .into_iter()
            .filter_map(|member| slots[member].take())
            .collect();
        let first = members.first().map(|entity| author_of(entity).to_owned());
        let kind = if members
            .iter()
            .all(|entity| Some(author_of(entity)) == first.as_deref())
        {
            Kind::Thread
        } else {
            Kind::Conversation
        };
        out.push(Card::Group(Group { kind, members }));
    }
    out
}

// ---- Reply hints --------------------------------------------------------

/// The web-only render hint carrying a one-line look at a reply's parent, for
/// a parent that is not itself on the page:
/// `{name, handle, avatar, excerpt, emojis}`, the same convention as
/// `_tag_source` and `_boosters`. The card already knows where to link — it
/// links to the parent either way.
pub const REPLY_PEEK: &str = "_reply_peek";

/// How much of the parent the peek carries. One line at the width of a status
/// card, no more: this is a hint, not a second post.
const EXCERPT_CHARS: usize = 140;

/// Attaches a [`REPLY_PEEK`] to every reply on the page whose parent is not on
/// the page, and hands the replies whose parent has never been fetched to
/// [`crate::parent_fetch`].
///
/// Runs on the entity list before [`group`], which needs no peeks — a parent it
/// can group is by definition on the page. Costs nothing on a page of
/// top-level posts, and a handful of keyed batch queries otherwise. Phanpy
/// pays an HTTP round-trip per parent for the same hint because it is a browser
/// talking to an API; we are the server and can read them in one go.
///
/// Every gate a card obeys, a peek obeys: [`filter_viewable`] for visibility
/// and blocks, the viewer's mutes, soft-deleted stubs, and — following Phanpy's
/// `status-compact` — a parent behind a content warning or marked sensitive
/// shows nothing at all rather than leaking its first line past the gate.
pub(super) async fn annotate_reply_peeks(
    state: &AppState,
    viewer_id: i64,
    entities: &mut [Value],
) -> Result<(), ApiError> {
    // What the reader can already see here, boosted posts included: a parent
    // rendered anywhere on this page needs no peek.
    let mut present: HashSet<String> = HashSet::with_capacity(entities.len() * 2);
    for entity in entities.iter() {
        present.insert(id_of(entity).to_owned());
        if let Some(inner) = entity.get("reblog").filter(|inner| inner.is_object()) {
            present.insert(id_of(inner).to_owned());
        }
    }

    let mut wanted: Vec<i64> = Vec::new();
    let mut unfetched: Vec<String> = Vec::new();
    for entity in entities.iter() {
        let shown = displayed(entity);
        match parent_of(shown) {
            Some(parent) if !present.contains(parent) => {
                if let Ok(parent) = parent.parse() {
                    wanted.push(parent);
                }
            }
            // No parent id at all but an `inReplyTo` we never resolved: the
            // card falls back to a link at the parent's host, and the fetch
            // below may turn it into a real hint on a later view.
            None => {
                if let Some(uri) = shown.get("in_reply_to_uri").and_then(Value::as_str) {
                    unfetched.push(uri.to_owned());
                }
            }
            Some(_) => {}
        }
    }
    crate::parent_fetch::spawn_resolves(state, unfetched, Some(viewer_id));
    if wanted.is_empty() {
        return Ok(());
    }
    wanted.sort_unstable();
    wanted.dedup();

    let peeks = build_peeks(state, viewer_id, &wanted).await?;
    if peeks.is_empty() {
        return Ok(());
    }
    for entity in entities.iter_mut() {
        let shown = displayed_mut(entity);
        let Some(peek) = parent_of(shown).and_then(|parent| peeks.get(parent)) else {
            continue;
        };
        let peek = peek.clone();
        if let Some(object) = shown.as_object_mut() {
            object.insert(REPLY_PEEK.to_owned(), peek);
        }
    }
    Ok(())
}

/// The peeks for `ids`, keyed by status id as a string (the form the entity
/// carries), with everything the viewer may not see left out.
async fn build_peeks(
    state: &AppState,
    viewer_id: i64,
    ids: &[i64],
) -> Result<HashMap<String, Value>, ApiError> {
    let pool = &state.pool;
    let rows = status::find_by_ids(pool, ids).await?;
    let rows = filter_viewable(pool, &rows, Some(viewer_id)).await?;
    // A parent behind a content warning, or marked sensitive, peeks as nothing:
    // the reader chose the gate and an excerpt would walk straight past it.
    let mut rows: Vec<_> = rows
        .into_iter()
        .filter(|row| row.spoiler_text.is_empty() && !row.sensitive)
        .collect();
    if rows.is_empty() {
        return Ok(HashMap::new());
    }

    let surviving: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let deleted = status::deleted_ids(pool, &surviving).await?;
    rows.retain(|row| !deleted.contains(&row.id));

    let mut author_ids: Vec<i64> = rows.iter().map(|row| row.account_id).collect();
    author_ids.sort_unstable();
    author_ids.dedup();
    let muted = mute::active_batch(pool, viewer_id, &author_ids).await?;
    rows.retain(|row| !muted.contains_key(&row.account_id));
    if rows.is_empty() {
        return Ok(HashMap::new());
    }

    let authors: HashMap<i64, account::Account> = account::find_by_ids(pool, &author_ids)
        .await?
        .into_iter()
        .map(|account| (account.id, account))
        .collect();
    let allow_direct = allow_direct_media(pool, Some(viewer_id)).await;

    // Build the text first, then resolve every shortcode referenced by either
    // the display name or excerpt. Lookups are grouped by author domain, just
    // like the normal status renderer, so a page of local replies adds one
    // query rather than one query per peek.
    let excerpts: HashMap<i64, String> = rows
        .iter()
        .map(|row| (row.id, excerpt(row.title.as_deref(), &row.content)))
        .filter(|(_, excerpt)| !excerpt.is_empty())
        .collect();
    let emoji_by_domain = peek_emoji_by_domain(pool, &rows, &authors, &excerpts).await?;

    let mut peeks = HashMap::with_capacity(rows.len());
    for row in rows {
        let Some(author) = authors.get(&row.account_id) else {
            continue;
        };
        let Some(excerpt) = excerpts.get(&row.id) else {
            continue; // nothing to show — a media-only post says more as a link
        };
        let acct = crate::entities::account_acct(&state.config.domain, author);
        let name = if author.display_name.is_empty() {
            author.username.clone()
        } else {
            author.display_name.clone()
        };
        let by_shortcode = emoji_by_domain.get(&(
            author.domain.clone(),
            author.domain.is_none().then_some(author.id),
        ));
        let mut seen = Vec::new();
        let emojis: Vec<Value> = plamenu_ap::emoji::scan_shortcodes(&name)
            .into_iter()
            .chain(plamenu_ap::emoji::scan_shortcodes(excerpt))
            .filter(|code| {
                if seen.contains(code) {
                    false
                } else {
                    seen.push(*code);
                    true
                }
            })
            .filter_map(|code| by_shortcode.and_then(|known| known.get(code)))
            .map(|emoji| crate::emoji::custom_emoji_json(&state.config.domain, emoji, allow_direct))
            .collect();
        peeks.insert(
            row.id.to_string(),
            json!({
                "name": name,
                "acct": acct,
                "avatar": avatar_url(&state.config.domain, author, allow_direct),
                "excerpt": excerpt,
                "emojis": emojis,
            }),
        );
    }
    Ok(peeks)
}

/// Resolves all custom emoji used by peek author names and excerpts, grouped
/// by the authors' domains so each domain costs at most one lookup.
async fn peek_emoji_by_domain(
    pool: &plamenu_db::PgPool,
    rows: &[status::Status],
    authors: &HashMap<i64, account::Account>,
    excerpts: &HashMap<i64, String>,
) -> Result<
    HashMap<(Option<String>, Option<i64>), HashMap<String, custom_emoji::CustomEmoji>>,
    ApiError,
> {
    let mut codes_by_domain: HashMap<(Option<String>, Option<i64>), Vec<String>> = HashMap::new();
    for row in rows {
        let (Some(author), Some(excerpt)) = (authors.get(&row.account_id), excerpts.get(&row.id))
        else {
            continue;
        };
        let name = if author.display_name.is_empty() {
            &author.username
        } else {
            &author.display_name
        };
        let codes = codes_by_domain
            .entry((
                author.domain.clone(),
                author.domain.is_none().then_some(author.id),
            ))
            .or_default();
        for code in plamenu_ap::emoji::scan_shortcodes(name)
            .into_iter()
            .chain(plamenu_ap::emoji::scan_shortcodes(excerpt))
        {
            if !codes.iter().any(|known| known == code) {
                codes.push(code.to_owned());
            }
        }
    }

    let mut shortcodes = Vec::new();
    let mut domains = Vec::new();
    let mut owners = Vec::new();
    for ((author_domain, owner), codes) in codes_by_domain {
        for code in codes {
            shortcodes.push(code);
            domains.push(author_domain.clone());
            owners.push(owner);
        }
    }
    let mut by_domain: HashMap<_, HashMap<String, custom_emoji::CustomEmoji>> = HashMap::new();
    for requested in
        custom_emoji::lookup_many_for_authors(pool, &shortcodes, &domains, &owners).await?
    {
        let key = (
            requested.request_domain.clone(),
            requested.request_owner_account_id,
        );
        let emoji = requested.into_emoji();
        by_domain
            .entry(key)
            .or_default()
            .insert(emoji.shortcode.clone(), emoji);
    }
    Ok(by_domain)
}

/// One line of the parent as plain text: an Article's title if it has one,
/// otherwise the start of its body with the markup taken out.
///
/// `filters::plain_text` turns every tag boundary into a space, deliberately —
/// it exists for whole-word filter matching, where two words either side of an
/// element must not run together. A mention is markup that splits *inside* a
/// word, though: Mastodon writes `@<span>rf</span>`, which came out of the
/// first staging render as "@ rf". [`rejoin_sigils`] puts those back.
fn excerpt(title: Option<&str>, content: &str) -> String {
    let (source, body) = match title {
        Some(title) if !title.trim().is_empty() => (title.to_owned(), false),
        _ => (crate::filters::plain_text(content), true),
    };
    let line = rejoin_sigils(&source);
    let line = if body {
        strip_leading_mentions(&line)
    } else {
        line.as_str()
    };
    let mut out: String = line.chars().take(EXCERPT_CHARS).collect();
    if out.chars().count() < line.chars().count() {
        out.push('…');
    }
    out
}

/// Drops the conventional addressee list from the start of a reply body. The
/// peek already names its author, and leading `@user` tokens routinely consume
/// the whole one-line budget without supplying any conversational context.
fn strip_leading_mentions(text: &str) -> &str {
    let mut rest = text.trim_start();
    while let Some(word) = rest.split_whitespace().next() {
        if !word.starts_with('@') || word.len() == 1 {
            break;
        }
        rest = rest[word.len()..].trim_start();
    }
    rest
}

/// Collapses runs of whitespace to single spaces and glues a handle or hashtag
/// back to the name that follows it — a token ending in `@` or `#` is a sigil
/// the markup separated from its word, never a word of its own.
fn rejoin_sigils(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() && !out.ends_with(['@', '#']) {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// The status a card is about: the boosted post for a boost, the entity itself
/// otherwise. A boosted reply gets its hint like any other reply.
fn displayed(entity: &Value) -> &Value {
    entity
        .get("reblog")
        .filter(|inner| inner.is_object())
        .unwrap_or(entity)
}

fn displayed_mut(entity: &mut Value) -> &mut Value {
    if entity.get("reblog").is_some_and(Value::is_object) {
        return entity.get_mut("reblog").expect("checked on the line above");
    }
    entity
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Card, Kind, excerpt, group};
    use crate::web::collapse::BOOSTERS;

    fn post(id: &str, author: &str, parent: Option<&str>) -> serde_json::Value {
        json!({
            "id": id,
            "in_reply_to_id": parent,
            "account": { "id": author, "group": false },
        })
    }

    fn boost(id: &str, booster: &str, target: &serde_json::Value) -> serde_json::Value {
        json!({
            "id": id,
            "account": { "id": booster, "group": false },
            "reblog": target,
        })
    }

    /// The shape of the rendered feed: a lone id for a single card, the group's
    /// members in reading order for a group.
    fn shape(cards: &[Card]) -> Vec<Vec<String>> {
        cards
            .iter()
            .map(|card| match card {
                Card::Single(entity) => vec![entity["id"].as_str().unwrap_or_default().to_owned()],
                Card::Group(group) => group
                    .members
                    .iter()
                    .map(|entity| entity["id"].as_str().unwrap_or_default().to_owned())
                    .collect(),
            })
            .collect()
    }

    fn kinds(cards: &[Card]) -> Vec<Kind> {
        cards
            .iter()
            .filter_map(|card| match card {
                Card::Group(group) => Some(group.kind),
                Card::Single(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_reply_and_its_parent_group_parent_first_at_the_newest_position() {
        let page = vec![
            post("30", "1", Some("10")),
            post("20", "2", None),
            post("10", "1", None),
        ];
        let cards = group(page);
        // The group takes slot 0 — where the reply, the newest of the two, was.
        assert_eq!(shape(&cards), [vec!["10", "30"], vec!["20"]]);
        assert_eq!(kinds(&cards), [Kind::Thread]);
    }

    #[test]
    fn two_authors_make_it_a_conversation() {
        let page = vec![post("30", "2", Some("10")), post("10", "1", None)];
        assert_eq!(kinds(&group(page)), [Kind::Conversation]);
    }

    #[test]
    fn siblings_read_oldest_first_under_their_parent() {
        // Snowflake ids sort numerically: "9" is older than "10".
        let page = vec![
            post("30", "1", Some("9")),
            post("10", "1", Some("9")),
            post("9", "1", None),
        ];
        assert_eq!(shape(&group(page)), [vec!["9", "10", "30"]]);
    }

    #[test]
    fn a_chain_reads_root_down_however_the_feed_ordered_it() {
        let page = vec![
            post("40", "1", Some("30")),
            post("20", "1", None),
            post("30", "2", Some("20")),
        ];
        assert_eq!(shape(&group(page)), [vec!["20", "30", "40"]]);
    }

    #[test]
    fn a_reply_whose_parent_is_absent_stays_a_single_card() {
        let page = vec![post("30", "1", Some("999")), post("20", "2", None)];
        assert_eq!(shape(&group(page)), [vec!["30"], vec!["20"]]);
    }

    #[test]
    fn boosts_never_join_a_group() {
        let parent = post("10", "1", None);
        let reply = post("30", "1", Some("10"));
        let page = vec![boost("40", "5", &reply), reply.clone(), parent.clone()];
        // The boost keeps its own card; the reply it repeats still groups.
        assert_eq!(shape(&group(page)), [vec!["40"], vec!["10", "30"]]);
    }

    #[test]
    fn a_card_hoisted_by_the_boost_collapse_still_joins_its_thread() {
        let mut parent = post("10", "1", None);
        parent[BOOSTERS] = json!([{ "id": "5" }]);
        // The parent is at the top of the page because people boosted it.
        // It is still a post, so the thread closes around it — and the group
        // inherits the slot the boost won, so nothing moves.
        let page = vec![parent, post("30", "1", Some("10"))];
        assert_eq!(shape(&group(page)), [vec!["10", "30"]]);
    }

    #[test]
    fn a_long_group_folds_everything_between_its_ends() {
        let page = vec![
            post("40", "1", Some("30")),
            post("30", "1", Some("20")),
            post("20", "1", Some("10")),
            post("10", "1", None),
        ];
        let cards = group(page);
        let Some(Card::Group(threaded)) = cards.first() else {
            panic!("expected one group, got {:?}", shape(&cards));
        };
        let (first, middle, last) = threaded.shown();
        assert_eq!(first["id"], "10");
        assert!(middle.is_empty());
        assert_eq!(last["id"], "40");
        let folded: Vec<_> = threaded.folded().iter().map(|e| e["id"].clone()).collect();
        assert_eq!(folded, ["20", "30"]);
    }

    #[test]
    fn a_group_of_three_renders_whole() {
        let page = vec![
            post("30", "1", Some("20")),
            post("20", "1", Some("10")),
            post("10", "1", None),
        ];
        let cards = group(page);
        let Some(Card::Group(threaded)) = cards.first() else {
            panic!("expected one group");
        };
        assert!(threaded.folded().is_empty());
        let (_, middle, _) = threaded.shown();
        assert_eq!(middle.len(), 1);
    }

    #[test]
    fn a_page_with_no_links_is_returned_untouched() {
        let page = vec![post("30", "1", None), post("20", "2", Some("999"))];
        assert_eq!(shape(&group(page.clone())), [vec!["30"], vec!["20"]]);
    }

    #[test]
    fn the_excerpt_is_one_line_of_plain_text() {
        let long = format!("<p>{}</p>", "word ".repeat(60));
        let short = excerpt(None, &long);
        assert!(short.ends_with('…'), "{short}");
        assert_eq!(short.chars().count(), 141);
        assert!(!short.contains('<'), "markup is stripped: {short}");
        // Line breaks in the parent must not break out of the one-line slot.
        assert_eq!(excerpt(None, "<p>first</p>\n<p>second</p>"), "first second");
    }

    #[test]
    fn a_mention_is_not_split_by_the_markup_around_it() {
        // Exactly what the first staging render showed: Mastodon writes the
        // handle as `@<span>rf</span>`, and a space per tag boundary made it
        // read "@ rf".
        let content = r#"<p>Вы моете ноги в раковине?</p><p><span class="h-card"><a href="https://mastodon.ml/@rf" class="u-url mention">@<span>rf</span></a></span></p>"#;
        assert_eq!(excerpt(None, content), "Вы моете ноги в раковине? @rf");
        // Hashtags are written the same way.
        assert_eq!(
            excerpt(
                None,
                "<p>see <a href=\"/tags/rust\">#<span>rust</span></a> today</p>"
            ),
            "see #rust today"
        );
    }

    #[test]
    fn leading_mentions_do_not_use_the_excerpt_budget() {
        let content = r#"<p><a class="mention" href="/@bob">@<span>bob</span></a> <a class="mention" href="/@carol">@carol</a> this is the useful context</p>"#;
        assert_eq!(excerpt(None, content), "this is the useful context");
        let crowded = format!("<p>{}context survives</p>", "@somebody ".repeat(30));
        assert_eq!(excerpt(None, &crowded), "context survives");
        // Article titles are authored summaries rather than reply-body
        // addressee lists, so a title that begins with an @ stays intact.
        assert_eq!(
            excerpt(Some("@home: a history"), "<p>@bob body</p>"),
            "@home: a history"
        );
    }

    #[test]
    fn an_article_peeks_as_its_title() {
        assert_eq!(
            excerpt(Some("On Ducks"), "<p>a very long essay follows</p>"),
            "On Ducks"
        );
        // A blank title is no title.
        assert_eq!(excerpt(Some("  "), "<p>body</p>"), "body");
    }
}
