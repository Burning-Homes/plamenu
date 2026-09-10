//! Renders a local status as its full `ActivityPub` `Note` — shared by the
//! object endpoint and Create/Update distribution, so every representation
//! of a post carries the same attachments, tags, mentions and quote fields.

use std::collections::HashMap;

use plamenu_ap::activity::{
    NoteParams, NotePoll, NoteQuote, NoteSource, note_object, quote_authorization_uri_for_actor,
};
use plamenu_ap::urls::LocalUserUrls;
use plamenu_db::account::{self, Account};
use plamenu_db::status::Status;
use plamenu_db::{media, mention, poll, quote, tag};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use crate::AppState;
use crate::entities::{account_uri, ap_attachments, status_uri_for_account};
use crate::error::ApiError;

/// The resolved quote fields of a local status, if any.
pub struct ResolvedQuote {
    pub quoted_uri: String,
    pub authorization_uri: Option<String>,
}

/// The `context`/`contextHistory` IRIs a local status advertises.
///
/// For a conversation we own we mint `/contexts/{id}` — but only once the root
/// is distributable, so we never advertise a members-only URL an outsider
/// can't dereference. For a remote-owned conversation we pass the owner's IRIs
/// through, so the remote owner threads our reply into its collection.
/// `contextHistory` (the FEP-171b container) is emitted for a locally-owned
/// private conversation only when the container is enabled.
pub async fn context_links_for<'e, E: plamenu_db::PgExecutor<'e>>(
    state: &AppState,
    executor: E,
    item: &Status,
) -> Result<(Option<String>, Option<String>), ApiError> {
    // `executor` reads the status' conversation mapping (just written on the
    // transaction connection at compose time, or committed data when serving a
    // Note); `context_links_from` below needs no database.
    let Some(ctx) = plamenu_db::conversation::context_of_status(executor, item.id).await? else {
        return Ok((None, None));
    };
    Ok(context_links_from(
        state,
        &ctx,
        state.conversation_containers().await,
    ))
}

/// [`context_links_for`] from an already-loaded mapping, with the container
/// flag resolved by the caller — so a page of Notes reads both once.
fn context_links_from(
    state: &AppState,
    ctx: &plamenu_db::conversation::StatusContext,
    containers_enabled: bool,
) -> (Option<String>, Option<String>) {
    if let Some(remote) = &ctx.uri {
        // Remote-owned: advertise exactly what the owner published.
        return (Some(remote.clone()), ctx.history_uri.clone());
    }
    // Locally owned. Advertise the posts collection only when the root is
    // distributable (the collection lists public/unlisted posts).
    let context = ctx
        .root_visibility
        .as_deref()
        .is_some_and(|v| matches!(v, "public" | "unlisted"))
        .then(|| plamenu_ap::urls::context_uri(&state.config.domain, ctx.conversation_id));
    let history = crate::containers::local_history_uri_when(state, ctx, containers_enabled);
    (context, history)
}

/// A status' poll prepared for Note serialization: the row, its RFC 3339
/// end time, and the tallies as the wire should carry them.
struct PreparedPoll {
    row: poll::Poll,
    end_time: Option<String>,
    tallies: Vec<i64>,
}

/// Prepares a status' poll for serialization. While a `hide_totals` poll runs,
/// the wire carries zero tallies — the real counts would leak through the
/// AP representation otherwise.
fn prepare_poll(row: poll::Poll) -> Result<PreparedPoll, ApiError> {
    let end_time = row
        .expires_at
        .map(|at| at.format(&Rfc3339))
        .transpose()
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let tallies = if row.hide_totals && !row.expired() {
        vec![0; row.cached_tallies.len()]
    } else {
        row.cached_tallies.clone()
    };
    Ok(PreparedPoll {
        row,
        end_time,
        tallies,
    })
}

/// A status' event sidecar in the shape the wire builder wants (E4).
///
/// Owned strings, because [`plamenu_ap::activity::NoteEvent`] borrows and the row
/// itself is dropped before the object is built. Returns `None` for a status with
/// no sidecar (the overwhelming majority) and for one whose sidecar has no start
/// time: an `Event` without a start is not an event any consumer can place on a
/// calendar, and emitting one would be worse than emitting a plain Note.
pub struct PreparedEvent {
    row: plamenu_db::status_event::StatusEvent,
    start_time: String,
    end_time: Option<String>,
    /// Accepted attendees, counted from our own rows — we are the origin of any
    /// event we serialize, so this is the authoritative number.
    participant_count: i64,
}

impl PreparedEvent {
    /// The borrowed view the object builder takes.
    #[must_use]
    pub fn as_note_event(&self) -> plamenu_ap::activity::NoteEvent<'_> {
        plamenu_ap::activity::NoteEvent {
            start_time: &self.start_time,
            end_time: self.end_time.as_deref(),
            timezone: self.row.timezone.as_deref(),
            // A local event always has a join mode; `free` is the composer
            // default and the only sane fallback for a row written before one.
            join_mode: self.row.join_mode.as_deref().unwrap_or("free"),
            external_participation_url: self.row.external_participation_url.as_deref(),
            max_attendees: self.row.max_attendees,
            participant_count: self.participant_count,
            status: self.row.event_status.as_deref().unwrap_or("CONFIRMED"),
            is_online: self.row.is_online.unwrap_or(false),
            location_name: self.row.location_name.as_deref(),
            location_street: self.row.location_street.as_deref(),
            location_locality: self.row.location_locality.as_deref(),
            location_region: self.row.location_region.as_deref(),
            location_country: self.row.location_country.as_deref(),
            location_postal_code: self.row.location_postal_code.as_deref(),
        }
    }
}

/// Loads a status' event sidecar inside the authoring transaction, where the
/// sidecar row was only just written and is not yet visible to a separate pool
/// connection. Page rendering goes through [`NoteBatch`] instead.
pub async fn event_for_note_conn(
    conn: &mut plamenu_db::PgConnection,
    status_id: i64,
) -> Result<Option<PreparedEvent>, ApiError> {
    let Some(row) = plamenu_db::status_event::find(&mut *conn, status_id).await? else {
        return Ok(None);
    };
    let count = plamenu_db::status_participation::accepted_count(&mut *conn, status_id).await?;
    prepare_event(Some(row), count)
}

/// Formats a sidecar row for the wire. `None` for a status with no sidecar, and
/// for one with no start time: an `Event` without a start is not placeable on any
/// calendar, and emitting one would be worse than emitting a plain Note.
fn prepare_event(
    row: Option<plamenu_db::status_event::StatusEvent>,
    participant_count: i64,
) -> Result<Option<PreparedEvent>, ApiError> {
    let Some(row) = row else { return Ok(None) };
    let Some(start) = row.start_time else {
        return Ok(None);
    };
    // `entities::rfc3339` is the shared formatter (and the shared error mapping).
    let start_time = crate::entities::rfc3339(start)?;
    let end_time = row.end_time.map(crate::entities::rfc3339).transpose()?;
    Ok(Some(PreparedEvent {
        row,
        start_time,
        end_time,
        participant_count,
    }))
}

/// How many of a status' own oldest replies the Note advertises as the first
/// page of its `replies` collection — Mastodon's `SELF_REPLIES_LIMIT`.
const SELF_REPLY_PREVIEW: i64 = 5;

/// Everything a page of `Note` documents needs, loaded once for the page.
///
/// Every `ActivityPub` collection that inlines Notes — the outbox, the replies
/// collection, the featured pins, the FEP-171b container — used to call
/// [`note_for_status`] once per row, and each call re-issued the single-id form
/// of a batch helper that already existed. That cost 16 round trips per row
/// before any optional artifact: a 60-reply collection page measured 1,143.
/// `NoteBatch` runs each helper **once over the whole page** and
/// [`note_in_batch`] then renders every row out of memory, so the page's round
/// trips no longer scale with its length.
///
/// Loading is by status id, so a caller may hand it rows from several
/// collections; a status absent from the batch renders as if it carried none of
/// these artifacts, which is why the render function is only ever called with a
/// batch built from the same slice.
#[derive(Default)]
pub struct NoteBatch {
    parent_uris: HashMap<i64, String>,
    media: HashMap<i64, Vec<plamenu_db::media::Media>>,
    mentions: HashMap<i64, Vec<Account>>,
    /// `(author, mentioned account)` edges that exist (accepted or pending).
    /// Used to restrict a silenced local author's AP audience to followers.
    silenced_mention_audience: std::collections::HashSet<(i64, i64)>,
    tag_names: HashMap<i64, Vec<String>>,
    group_uris: HashMap<i64, String>,
    quotes: HashMap<i64, ResolvedQuote>,
    polls: HashMap<i64, PreparedPoll>,
    events: HashMap<i64, PreparedEvent>,
    engagement: HashMap<i64, plamenu_db::status::Engagement>,
    self_reply_ids: HashMap<i64, Vec<i64>>,
    sources: HashMap<i64, plamenu_db::status::StatusSource>,
    /// Per-author effective emoji namespaces, loaded for the whole page in
    /// one query so personal shortcode collisions never cross accounts.
    emojis: HashMap<i64, Vec<plamenu_db::custom_emoji::CustomEmoji>>,
    collection_tags: HashMap<i64, Vec<Value>>,
    contexts: HashMap<i64, (Option<String>, Option<String>)>,
    webxdc_invitations: HashMap<i64, (i64, String, String)>,
    /// Soft-deleted stub ids among the page (same batched lookup the entity
    /// renderer's `RenderMaps` does) — `note_in_batch` serves a placeholder
    /// for these instead of trusting every caller's STUBFILTER.
    deleted: std::collections::HashSet<i64>,
}

impl NoteBatch {
    /// Loads the sidecars of every status in `items`.
    #[allow(clippy::too_many_lines)]
    pub async fn load(state: &AppState, items: &[&Status]) -> Result<Self, ApiError> {
        let mut conn = state
            .pool
            .acquire()
            .await
            .map_err(plamenu_db::DbError::from)?;
        Self::load_conn(state, &mut conn, items).await
    }

    /// Reads the current mutation's sidecars before its outbox is committed.
    #[allow(clippy::too_many_lines)]
    pub async fn load_conn(
        state: &AppState,
        conn: &mut plamenu_db::PgConnection,
        items: &[&Status],
    ) -> Result<Self, ApiError> {
        let mut batch = Self::default();
        if items.is_empty() {
            return Ok(batch);
        }
        let domain = &state.config.domain;
        let ids: Vec<i64> = items.iter().map(|item| item.id).collect();

        // Parents, for `inReplyTo`. On a replies collection all 60 rows share
        // one parent and one parent author, which used to be 120 lookups of the
        // same two rows.
        let parent_ids = distinct(items.iter().filter_map(|item| item.in_reply_to_id));
        if !parent_ids.is_empty() {
            let parents = plamenu_db::status::find_by_ids(&mut *conn, &parent_ids).await?;
            let author_ids = distinct(parents.iter().map(|parent| parent.account_id));
            let authors = account::find_by_ids(&mut *conn, &author_ids).await?;
            for item in items {
                let Some(parent) = item
                    .in_reply_to_id
                    .and_then(|id| parents.iter().find(|parent| parent.id == id))
                else {
                    continue;
                };
                let author = authors
                    .iter()
                    .find(|a| a.id == parent.account_id)
                    .ok_or(ApiError::NotFound)?;
                batch
                    .parent_uris
                    .insert(item.id, status_uri_for_account(domain, parent, author));
            }
        }

        batch.media = media::for_statuses_conn(&mut *conn, &ids).await?;
        batch.tag_names = tag::names_for_statuses(&mut *conn, &ids).await?;
        batch.deleted = plamenu_db::status::deleted_ids(&mut *conn, &ids).await?;

        // A closed (private/direct) status must carry its silent recipients
        // too — the inherited reply audience is only addressed via
        // mention rows, and receivers like Mastodon and GoToSocial grant DM
        // visibility solely from `tag` Mentions; without them the reply is
        // invisible to its recipients. Open statuses keep active mentions only
        // (silent rows there are group attribution, never audience). The two
        // halves of the page take one query each.
        let (mut closed, mut open): (Vec<i64>, Vec<i64>) = (Vec::new(), Vec::new());
        for item in items {
            if matches!(item.visibility.as_str(), "private" | "direct") {
                closed.push(item.id);
            } else {
                open.push(item.id);
            }
        }
        if !open.is_empty() {
            batch
                .mentions
                .extend(mention::for_statuses(&mut *conn, &open, true).await?);
        }
        if !closed.is_empty() {
            batch
                .mentions
                .extend(mention::for_statuses(&mut *conn, &closed, false).await?);
        }
        let author_ids = distinct(items.iter().map(|item| item.account_id));
        let mention_ids = distinct(
            batch
                .mentions
                .values()
                .flatten()
                .map(|mentioned| mentioned.id),
        );
        batch.silenced_mention_audience =
            plamenu_db::follow::existing_in_edges(&mut *conn, &author_ids, &mention_ids).await?;

        // A group submission carries its community claim on every
        // representation: consumers — Lemmy re-fetching the object
        // from the group's Announce included — associate it with the group via
        // `audience`. Only the first community is claimed, but a missing
        // account row falls through to the next, like the per-status helper.
        let attributions = plamenu_db::group::community_attributions_of(&mut *conn, &ids).await?;
        if !attributions.is_empty() {
            let group_ids = distinct(attributions.iter().map(|(_, group_id)| *group_id));
            let groups = account::find_by_ids(&mut *conn, &group_ids).await?;
            for (status_id, group_id) in attributions {
                if batch.group_uris.contains_key(&status_id) {
                    continue;
                }
                if let Some(group) = groups.iter().find(|a| a.id == group_id) {
                    batch
                        .group_uris
                        .insert(status_id, crate::groups::community_uri(state, group));
                }
            }
        }

        let quote_rows = quote::for_statuses(&mut *conn, &ids).await?;
        if !quote_rows.is_empty() {
            let quoted_ids = distinct(quote_rows.values().filter_map(|row| row.quoted_status_id));
            let quoted = plamenu_db::status::find_by_ids(&mut *conn, &quoted_ids).await?;
            let author_ids = distinct(quoted.iter().map(|status| status.account_id));
            let authors = account::find_by_ids(&mut *conn, &author_ids).await?;
            for (status_id, row) in quote_rows {
                let Some(target) = row
                    .quoted_status_id
                    .and_then(|id| quoted.iter().find(|status| status.id == id))
                else {
                    continue;
                };
                let author = authors
                    .iter()
                    .find(|a| a.id == target.account_id)
                    .ok_or(ApiError::NotFound)?;
                let authorization_uri = if row.state == "accepted" {
                    match row.approval_uri.clone() {
                        Some(stored) => Some(stored),
                        None if author.is_local() => Some(quote_authorization_uri_for_actor(
                            &account_uri(domain, author),
                            row.id,
                        )),
                        None => None,
                    }
                } else {
                    None
                };
                batch.quotes.insert(
                    status_id,
                    ResolvedQuote {
                        quoted_uri: status_uri_for_account(domain, target, author),
                        authorization_uri,
                    },
                );
            }
        }

        for (status_id, row) in poll::for_statuses(&mut *conn, &ids).await? {
            batch.polls.insert(status_id, prepare_poll(row)?);
        }

        // `statuses.object_type` is already loaded, so a page with no Event on
        // it costs nothing here at all.
        let event_ids: Vec<i64> = items
            .iter()
            .filter(|item| item.object_type.as_deref() == Some("Event"))
            .map(|item| item.id)
            .collect();
        if !event_ids.is_empty() {
            let rows = plamenu_db::status_event::for_statuses(&mut *conn, &event_ids).await?;
            let with_sidecar: Vec<i64> = rows.keys().copied().collect();
            let counts =
                plamenu_db::status_participation::accepted_counts(&mut *conn, &with_sidecar)
                    .await?;
            for (status_id, row) in rows {
                let count = counts.get(&status_id).copied().unwrap_or(0);
                if let Some(prepared) = prepare_event(Some(row), count)? {
                    batch.events.insert(status_id, prepared);
                }
            }
        }

        batch.engagement = plamenu_db::status::engagement_for_conn(&mut *conn, &ids).await?;
        batch.self_reply_ids =
            plamenu_db::status::self_reply_ids_for(&mut *conn, &ids, SELF_REPLY_PREVIEW).await?;
        // The raw source rides the Note's `source` property (P4). Load it with
        // the sparse Webxdc invitation extension in one query: an extension
        // that almost every status lacks must not add one round trip to every
        // ActivityPub Note page.
        for (status_id, sidecars) in plamenu_db::status::note_sidecars(&mut *conn, &ids).await? {
            batch.sources.insert(status_id, sidecars.source);
            if let Some(invitation) = sidecars.webxdc_invitation {
                batch.webxdc_invitations.insert(status_id, invitation);
            }
        }

        // Custom emoji referenced in the text ride the `tag` array as `Emoji`
        // entries, like Mastodon's NoteSerializer. One lookup covers every
        // shortcode on the page, poll options included.
        let mut shortcodes: Vec<String> = Vec::new();
        let mut domains: Vec<Option<String>> = Vec::new();
        let mut owners: Vec<Option<i64>> = Vec::new();
        for item in items {
            let mut texts: Vec<&str> = vec![&item.spoiler_text, &item.content];
            if let Some(poll) = batch.polls.get(&item.id) {
                texts.extend(poll.row.options.iter().map(String::as_str));
            }
            for code in crate::emoji::shortcodes_of(&texts) {
                shortcodes.push(code);
                domains.push(None);
                owners.push(Some(item.account_id));
            }
        }
        for requested in plamenu_db::custom_emoji::lookup_many_for_authors(
            &mut *conn,
            &shortcodes,
            &domains,
            &owners,
        )
        .await?
        {
            if let Some(owner_id) = requested.request_owner_account_id {
                let emoji = requested.into_emoji();
                let bucket = batch.emojis.entry(owner_id).or_default();
                if !bucket.iter().any(|known| known.id == emoji.id) {
                    bucket.push(emoji);
                }
            }
        }

        // Referenced collections (FEP-7aa9) ride the `tag` array as
        // `FeaturedCollection` objects, like Mastodon's `NoteSerializer`.
        batch.collection_tags =
            crate::collections::note_tagged_collection_tags_for(state, &mut *conn, &ids).await?;

        let containers_enabled = state
            .settings_cache
            .get_on(&mut *conn)
            .await?
            .conversation_containers
            .unwrap_or(state.config.conversation_containers);
        for (status_id, ctx) in
            plamenu_db::conversation::contexts_of_statuses(&mut *conn, &ids).await?
        {
            batch.contexts.insert(
                status_id,
                context_links_from(state, &ctx, containers_enabled),
            );
        }
        Ok(batch)
    }
}

/// The distinct values of an iterator, in first-seen order — the id sets the
/// batch loaders bind.
fn distinct(values: impl Iterator<Item = i64>) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for value in values {
        if !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

/// Builds the complete Note object for a local status (no `@context`).
///
/// One status, one batch. Page builders load a [`NoteBatch`] over the whole
/// page and call [`note_in_batch`] per row instead.
pub async fn note_for_status(
    state: &AppState,
    item: &Status,
    author: &Account,
) -> Result<Value, ApiError> {
    let batch = NoteBatch::load(state, std::slice::from_ref(&item)).await?;
    note_in_batch(state, item, author, &batch)
}

/// Builds the complete Note object for one status of an already-loaded page.
#[allow(clippy::too_many_lines)]
pub fn note_in_batch(
    state: &AppState,
    item: &Status,
    author: &Account,
    batch: &NoteBatch,
) -> Result<Value, ApiError> {
    let domain = &state.config.domain;
    let published = item
        .created_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let updated = item
        .edited_at
        .map(|at| at.format(&Rfc3339))
        .transpose()
        .map_err(|e| ApiError::Internal(Box::new(e)))?;

    let parent_uri = batch.parent_uris.get(&item.id);

    // Defence in depth (thread-stub follow-ups): a soft-deleted stub must
    // never serve anything of the author's. The AP object GET answers `410`
    // (`routes/statuses.rs`) and every served collection is
    // STUBFILTER-excluded, so this branch is unreachable today — it exists so
    // a future collection that forgets the filter serves a placeholder Note,
    // not a post. The stored row is already stripped (`status::stub_local` /
    // `stub_resolved` null the content and drop the sidecars), so this pins
    // the wire shape instead of trusting every stripper to stay complete.
    // Threading (`inReplyTo`) is deliberately kept — preserving the tree is
    // the stub's whole purpose.
    if batch.deleted.contains(&item.id) {
        return Ok(note_object(&NoteParams {
            kind: plamenu_ap::activity::PostKind::Note,
            domain,
            username: &author.username,
            actor_id: author.uri.as_deref(),
            status_id: item.id,
            content_html: crate::entities::DELETED_STATUS_PLACEHOLDER,
            source: None,
            published: &published,
            updated: None,
            visibility: &item.visibility,
            summary: None,
            sensitive: false,
            language: None,
            in_reply_to_uri: parent_uri.map(String::as_str),
            attachments: &[],
            tag: &[],
            mentioned_uris: &[],
            quote: None,
            quote_approval_policy: item.quote_approval_policy,
            poll: None,
            self_reply_ids: &[],
            favourites_count: 0,
            reblogs_count: 0,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
        }));
    }

    let attachments = ap_attachments(
        domain,
        batch.media.get(&item.id).map_or(&[][..], Vec::as_slice),
    );

    // Rebuild tag objects and mention addressing from storage.
    let mut tag_json = Vec::new();
    let mut mentioned_uris = Vec::new();
    if let Some(mentioned) = batch.mentions.get(&item.id) {
        for account in mentioned {
            let url = account.uri.clone().unwrap_or_else(|| {
                LocalUserUrls::for_account(domain, &account.username, account.uri.as_deref()).id
            });
            let acct = if account.has_local_account_on(domain) {
                format!("{}@{}", account.username, state.config.account_domain)
            } else {
                format!(
                    "{}@{}",
                    account.username,
                    account.domain.as_deref().unwrap_or_default()
                )
            };
            tag_json.push(json!({"type": "Mention", "href": url, "name": format!("@{acct}")}));
            if !author.silenced()
                || batch
                    .silenced_mention_audience
                    .contains(&(author.id, account.id))
            {
                mentioned_uris.push(url);
            }
        }
    }
    if let Some(names) = batch.tag_names.get(&item.id) {
        for name in names {
            let lower = name.to_lowercase();
            tag_json.push(json!({
                "type": "Hashtag",
                "href": format!("https://{domain}/tags/{lower}"),
                "name": format!("#{lower}"),
            }));
        }
    }

    let group_uri = batch.group_uris.get(&item.id);
    let resolved = batch.quotes.get(&item.id);
    let prepared_poll = batch.polls.get(&item.id);
    let prepared_event = batch.events.get(&item.id);
    let engagement = batch.engagement.get(&item.id).copied().unwrap_or_default();
    let empty_replies: Vec<i64> = Vec::new();
    let self_reply_ids = batch.self_reply_ids.get(&item.id).unwrap_or(&empty_replies);
    let source = batch.sources.get(&item.id).cloned().unwrap_or_default();

    let mut emoji_texts: Vec<&str> = vec![&item.spoiler_text, &item.content];
    if let Some(poll) = prepared_poll {
        emoji_texts.extend(poll.row.options.iter().map(String::as_str));
    }
    tag_json.extend(crate::emoji::emoji_tags_from(
        domain,
        batch
            .emojis
            .get(&item.account_id)
            .map_or(&[][..], Vec::as_slice),
        &emoji_texts,
    )?);
    if let Some(tags) = batch.collection_tags.get(&item.id) {
        tag_json.extend(tags.iter().cloned());
    }

    let (context, context_history) = batch
        .contexts
        .get(&item.id)
        .cloned()
        .unwrap_or((None, None));

    let mut note = note_object(&NoteParams {
        // The stored column is authoritative for what the audience already
        // received; the `Create` in `actions::post_status` derives the same kind
        // from the same row, so activity and object cannot disagree.
        kind: plamenu_ap::activity::PostKind::of_stored(
            item.object_type.as_deref(),
            prepared_poll.is_some(),
            item.title.is_some(),
        ),
        domain,
        username: &author.username,
        actor_id: author.uri.as_deref(),
        status_id: item.id,
        content_html: &item.content,
        source: Some(NoteSource {
            content: &source.text,
            media_type: &source.content_type,
        }),
        published: &published,
        updated: updated.as_deref(),
        visibility: &item.visibility,
        summary: (!item.spoiler_text.is_empty()).then_some(item.spoiler_text.as_str()),
        sensitive: author.sensitized() || item.sensitive,
        language: item.language.as_deref(),
        in_reply_to_uri: parent_uri.map(String::as_str),
        attachments: &attachments,
        tag: &tag_json,
        mentioned_uris: &mentioned_uris,
        quote: resolved.map(|r| NoteQuote {
            quoted_uri: &r.quoted_uri,
            authorization_uri: r.authorization_uri.as_deref(),
        }),
        quote_approval_policy: item.quote_approval_policy,
        poll: prepared_poll.map(|p| NotePoll {
            options: &p.row.options,
            tallies: &p.tallies,
            multiple: p.row.multiple,
            end_time: p.end_time.as_deref(),
            expired: p.row.expired(),
            voters_count: p.row.voters_count,
        }),
        self_reply_ids,
        favourites_count: engagement.favourites,
        reblogs_count: engagement.reblogs,
        title: item.title.as_deref(),
        external_url: item.external_url.as_deref(),
        group_uri: group_uri.map(String::as_str),
        context: context.as_deref(),
        context_history: context_history.as_deref(),
        event: prepared_event.map(PreparedEvent::as_note_event),
    });
    if let Some((_, session_uri, session_name)) = batch.webxdc_invitations.get(&item.id) {
        crate::webxdc::enhance_invitation(&mut note, session_uri, session_name);
    }
    Ok(note)
}
