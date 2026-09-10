//! High-level local-user actions, shared by the CLI and the client API.
//! Everything here only enqueues — the delivery worker does the sending.

use plamenu_ap::acct::Acct;
use plamenu_ap::activity::{self, NoteParams, NotePoll};
use plamenu_ap::urls::{InstanceActorUrls, LocalUserUrls, report_uri};
use plamenu_db::account::{self, Account};
use plamenu_db::media::Media;
use plamenu_db::report::Report;
use plamenu_db::status::Status;
use plamenu_db::{
    account_domain_block, block, bookmark, conversation, custom_emoji, dislike,
    domain_severance_job, favourite, featured_tag, follow, group, id, job, media, mention, mute,
    notification, pin, poll, quote, reaction, report, role, status, status_edit, statuses_cleanup,
    tag,
};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use crate::AppState;
use crate::compose::{Composed, PostFormat, compose};
use crate::entities::{
    account_uri, can_view, displayed_reaction_name, status_uri_for_account, status_web_url,
};
use crate::error::ApiError;
use crate::remote::store_remote_actor_from_resolution;

pub(crate) fn published_of(item: &Status) -> Result<String, ApiError> {
    item.created_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))
}

pub(crate) fn ensure_status_has_content(
    html: &str,
    media_ids: &[i64],
    has_typed_content: bool,
) -> Result<(), ApiError> {
    // A group submission's title or link stands in for the body — a Lemmy-style
    // titled/link thread is legitimately body-less, so it isn't "blank".
    if html.is_empty() && media_ids.is_empty() && !has_typed_content {
        return Err(ApiError::Unprocessable(
            "Validation failed: Text can't be blank".into(),
        ));
    }
    Ok(())
}

/// The instance character limit, weighed like Mastodon's
/// `StatusLengthValidator` (see [`crate::compose::countable_length`]) with
/// Mastodon's exact refusal wording.
pub(crate) fn ensure_within_character_limit(
    text: &str,
    spoiler_text: &str,
    max_characters: i32,
) -> Result<(), ApiError> {
    let max = usize::try_from(max_characters).unwrap_or(usize::MAX);
    if crate::compose::countable_length(spoiler_text, text) > max {
        return Err(ApiError::Unprocessable(format!(
            "Validation failed: Text character limit of {max_characters} exceeded"
        )));
    }
    Ok(())
}

/// How a delivery is enqueued — most activities go out as-is, but
/// retractable interactions and their retractions get the cancel-in-queue
/// treatment so a boost chased by an instant unboost never turns into two
/// activities racing through the remote's concurrent inbox processing.
#[derive(Clone, Copy)]
enum DeliveryMode {
    /// Deliver as soon as the worker gets to it.
    Plain,
    /// Plain, plus Mastodon's followers-synchronization header.
    Synchronized,
    /// A `Like`/`Announce`/`EmojiReact`: held back briefly so an immediate
    /// retraction can cancel it in-queue (see [`job::enqueue_cancellable`]).
    Cancellable,
    /// An `Undo` of a cancellable activity: cancels the queued original
    /// first, and is skipped entirely when that original was never attempted
    /// (the remote cannot have seen it, so retracting it would only recreate
    /// the ordering race).
    Retraction,
}

/// Connection-scoped delivery for an enclosing mutation transaction.
async fn enqueue_with_mode_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    signer: &Account,
    inbox: &str,
    activity: &Value,
    mode: DeliveryMode,
) -> Result<(), ApiError> {
    match mode {
        DeliveryMode::Plain => {
            job::enqueue(&mut *conn, signer.id, inbox, activity).await?;
        }
        DeliveryMode::Synchronized => {
            job::enqueue_synchronized(&mut *conn, signer.id, inbox, activity).await?;
        }
        DeliveryMode::Cancellable => {
            job::enqueue_cancellable(&mut *conn, signer.id, inbox, activity).await?;
        }
        DeliveryMode::Retraction => {
            let original_uri = activity["object"]["id"].as_str().unwrap_or_default();
            if job::cancel(&mut *conn, inbox, original_uri).await? == Some(0) {
                return Ok(());
            }
            job::enqueue(&mut *conn, signer.id, inbox, activity).await?;
        }
    }
    Ok(())
}

/// Enqueues `activity` (signed by `signer`) to every follower inbox, plus
/// `extras` (e.g. interaction targets or mentioned actors' inboxes).
pub(crate) async fn fan_out(
    state: &AppState,
    signer: &Account,
    activity: &Value,
    extras: &[String],
) -> Result<usize, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    fan_out_conn(state, &mut conn, signer, activity, extras).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub(crate) async fn fan_out_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    signer: &Account,
    activity: &Value,
    extras: &[String],
) -> Result<usize, ApiError> {
    fan_out_inner_conn(state, conn, signer, activity, extras, DeliveryMode::Plain).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
async fn fan_out_synchronized_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    signer: &Account,
    activity: &Value,
    extras: &[String],
) -> Result<usize, ApiError> {
    fan_out_inner_conn(
        state,
        conn,
        signer,
        activity,
        extras,
        DeliveryMode::Synchronized,
    )
    .await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
async fn fan_out_inner_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    signer: &Account,
    activity: &Value,
    extras: &[String],
    mode: DeliveryMode,
) -> Result<usize, ApiError> {
    let inboxes = fan_out_inboxes(&mut *conn, signer, extras).await?;
    // The whole fan-out is batched into one statement per mode — enqueueing
    // per inbox made posting O(#follower-inboxes) database round trips.
    match mode {
        DeliveryMode::Plain => {
            job::enqueue_many(&mut *conn, signer.id, &inboxes, activity, false).await?;
        }
        DeliveryMode::Synchronized => {
            job::enqueue_many(&mut *conn, signer.id, &inboxes, activity, true).await?;
        }
        DeliveryMode::Cancellable => {
            job::enqueue_many_cancellable(&mut *conn, signer.id, &inboxes, activity).await?;
        }
        DeliveryMode::Retraction => {
            let original_uri = activity["object"]["id"].as_str().unwrap_or_default();
            let cancelled = job::cancel_many(&mut *conn, &inboxes, original_uri).await?;
            // An inbox whose queued original was cancelled unattempted never
            // saw the activity, so the retraction toward it is dropped too.
            let remaining: Vec<String> = inboxes
                .iter()
                .filter(|inbox| cancelled.get(inbox.as_str()) != Some(&0))
                .cloned()
                .collect();
            job::enqueue_many(&mut *conn, signer.id, &remaining, activity, false).await?;
        }
    }
    Ok(inboxes.len())
}

/// Enqueues `activity` to exactly the given inboxes (deduplicated) — the
/// delivery for direct messages, whose audience is the mentioned actors
/// only, never the followers.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn deliver_to_inboxes_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    signer: &Account,
    activity: &Value,
    inboxes: &[String],
) -> Result<usize, ApiError> {
    let mut seen: Vec<String> = Vec::with_capacity(inboxes.len());
    for inbox in inboxes {
        if !inbox.is_empty() && !seen.contains(inbox) {
            seen.push(inbox.clone());
        }
    }
    job::enqueue_many(&mut *conn, signer.id, &seen, activity, false).await?;
    Ok(seen.len())
}

/// The author's inbox when the status belongs to a remote account.
async fn remote_author_inbox(
    state: &AppState,
    item: &Status,
) -> Result<(Account, Option<String>), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    remote_author_inbox_conn(state, &mut conn, item).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
async fn remote_author_inbox_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    item: &Status,
) -> Result<(Account, Option<String>), ApiError> {
    let author = account::find_by_id(&mut *conn, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let inbox = (!author.is_local()).then(|| author.inbox_url.clone());
    Ok((author, inbox))
}

#[derive(Debug, Default)]
pub struct PostParams<'a> {
    pub username: &'a str,
    pub text: &'a str,
    pub visibility: &'a str,
    pub in_reply_to_id: Option<i64>,
    /// Previously-uploaded media to attach.
    pub media_ids: &'a [i64],
    /// FEP-044f quote of another status.
    pub quoted_status_id: Option<i64>,
    /// Content warning (plain text); empty means none.
    pub spoiler_text: &'a str,
    /// NSFW flag; a content warning forces it on, like Mastodon.
    pub sensitive: bool,
    /// ISO 639 language code.
    pub language: Option<&'a str>,
    /// The format `text` is authored in (P4, Pleroma's `content_type`),
    /// already resolved from the client string / user preference.
    pub content_type: PostFormat,
    /// Poll to attach (`poll[...]` parameters).
    pub poll: Option<PollParams>,
    /// Quote-approval bitmap, already resolved from the client string / user
    /// preference; `None` falls back to the visibility default (and either way
    /// a non-distributable post stores `nobody`).
    pub quote_approval_policy: Option<i32>,
    /// Local group to submit this post to. Top-level only — replies
    /// inherit their parent's group automatically. Requires `public`.
    pub group_id: Option<i64>,
    /// Thread title: allowed only on a top-level group submission,
    /// which then federates as a `Page` (Lemmy's post shape).
    pub title: Option<&'a str>,
    /// A titled link post's target URL (`attachment: [{type: Link, href}]`).
    pub external_url: Option<&'a str>,
    /// Event to attach (E4): makes this an `Event` post rather than a `Note`.
    /// Never inferred — see [`EventParams`].
    pub event: Option<EventParams>,
    /// The post kind the author picked. Only the three author-selectable kinds
    /// are meaningful here — `Note` (the default), `Article` (long-form) and
    /// `Event`; `Question` and `Page` are implied by attaching a poll or
    /// submitting to a group and are resolved by [`stored_kind`].
    ///
    /// `Event` is stated by [`Self::event`] being set, which the composer gates
    /// on the same explicit choice; passing `PostKind::Event` here without event
    /// params is refused rather than silently downgraded.
    pub kind: activity::PostKind,
}

/// A progressive-enhancement link from an ordinary status to a Webxdc
/// session. The status remains readable by implementations that know nothing
/// about FEP-752d.
#[derive(Debug, Clone, Copy)]
pub struct WebxdcInvitation<'a> {
    pub session_id: i64,
    pub session_uri: &'a str,
    pub session_name: &'a str,
}

/// The AS2 type a new local post is stored and served as: what the author picked,
/// plus the two kinds nothing asks for explicitly. The single place this is
/// decided for a local post — `statuses.object_type` records the answer and every
/// representation (served object, `Create`, entity, rendered card) derives from
/// the column.
fn stored_kind(
    authored: activity::PostKind,
    has_poll: bool,
    has_title: bool,
) -> activity::PostKind {
    use activity::PostKind::{Article, Event, Note, Page, Question};
    match authored {
        Article => Article,
        Event => Event,
        // A `Question` and a group `Page` are implied, never chosen: a poll or a
        // group submission is what makes them.
        _ if has_poll => Question,
        _ if has_title => Page,
        _ => Note,
    }
}

/// The `statuses.object_type` value for a kind: `Note` and `Question` are the
/// untyped baseline (the column stays NULL), every
/// other kind names itself.
fn object_type_column(kind: activity::PostKind) -> Option<&'static str> {
    match kind {
        activity::PostKind::Note | activity::PostKind::Question => None,
        other => Some(other.as_str()),
    }
}

/// A new event's parameters, pre-validation (E4).
///
/// The composer sets this only when the author explicitly picked the event post
/// kind. It is deliberately *not* derived from "the author filled in a date":
/// Mastodon renders an `Event` as a truncated title-plus-link stub, so an implicit
/// upgrade would silently cost the author most of their readers.
#[derive(Debug, Default)]
pub struct EventParams {
    /// RFC 3339 start time. Required — an event without one is not placeable on
    /// any calendar.
    pub start_time: String,
    pub end_time: Option<String>,
    /// IANA zone name of the venue.
    pub timezone: Option<String>,
    /// `free` | `restricted` | `invite` | `external` (§9.4: the full matrix).
    pub join_mode: String,
    /// Required when `join_mode` is `external`, meaningless otherwise.
    pub external_participation_url: Option<String>,
    /// Enforced, not advertised: a `Join` past it is refused (§9.5).
    pub max_attendees: Option<i32>,
    /// `CONFIRMED` | `TENTATIVE` | `CANCELLED`.
    pub status: String,
    pub is_online: bool,
    pub location_name: Option<String>,
    pub location_street: Option<String>,
    pub location_locality: Option<String>,
    pub location_region: Option<String>,
    pub location_country: Option<String>,
    pub location_postal_code: Option<String>,
}

/// An amendment to an existing event (E4).
///
/// Every field is optional and `None` means **keep what is stored** — an edit
/// amends an event, it does not re-supply one. That matters concretely: a client
/// that PUTs only `status: CANCELLED` must not thereby erase the venue and the
/// end time, and an organizer moving the date must not have to re-type the
/// address to avoid losing it.
#[derive(Debug, Default)]
pub struct EventPatch {
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub timezone: Option<String>,
    pub join_mode: Option<String>,
    pub external_participation_url: Option<String>,
    pub max_attendees: Option<i32>,
    pub status: Option<String>,
    pub is_online: Option<bool>,
    pub location_name: Option<String>,
    pub location_street: Option<String>,
    pub location_locality: Option<String>,
    pub location_region: Option<String>,
    pub location_country: Option<String>,
    pub location_postal_code: Option<String>,
}

impl EventPatch {
    /// Applies the patch over a stored sidecar, yielding the complete parameters
    /// the validator and the writer take.
    pub(crate) fn apply(self, stored: &plamenu_db::status_event::StatusEvent) -> EventParams {
        let rfc3339 = |at: time::OffsetDateTime| {
            at.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default()
        };
        EventParams {
            start_time: self
                .start_time
                .or_else(|| stored.start_time.map(rfc3339))
                .unwrap_or_default(),
            end_time: self.end_time.or_else(|| stored.end_time.map(rfc3339)),
            timezone: self.timezone.or_else(|| stored.timezone.clone()),
            join_mode: self
                .join_mode
                .or_else(|| stored.join_mode.clone())
                .unwrap_or_else(|| "free".to_owned()),
            external_participation_url: self
                .external_participation_url
                .or_else(|| stored.external_participation_url.clone()),
            max_attendees: self.max_attendees.or(stored.max_attendees),
            status: self
                .status
                .or_else(|| stored.event_status.clone())
                .unwrap_or_else(|| "CONFIRMED".to_owned()),
            is_online: self.is_online.or(stored.is_online).unwrap_or(false),
            location_name: self.location_name.or_else(|| stored.location_name.clone()),
            location_street: self
                .location_street
                .or_else(|| stored.location_street.clone()),
            location_locality: self
                .location_locality
                .or_else(|| stored.location_locality.clone()),
            location_region: self
                .location_region
                .or_else(|| stored.location_region.clone()),
            location_country: self
                .location_country
                .or_else(|| stored.location_country.clone()),
            location_postal_code: self
                .location_postal_code
                .or_else(|| stored.location_postal_code.clone()),
        }
    }
}

/// A new poll's parameters, pre-validation.
#[derive(Debug)]
pub struct PollParams {
    pub options: Vec<String>,
    /// Seconds from now until the poll closes.
    pub expires_in: Option<i64>,
    pub multiple: bool,
    pub hide_totals: bool,
}

/// The sidecar row of a locally-authored event.
fn event_sidecar_of(
    status_id: i64,
    input: &EventParams,
    valid: &ValidatedEvent,
) -> plamenu_db::status_event::StatusEvent {
    let trimmed = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    plamenu_db::status_event::StatusEvent {
        status_id,
        start_time: Some(valid.start_time),
        end_time: valid.end_time,
        location_name: trimmed(&input.location_name),
        timezone: trimmed(&input.timezone),
        event_status: Some(input.status.clone()),
        join_mode: Some(input.join_mode.clone()),
        // Our own accepted rows are the count for an event we host, so the
        // column stays NULL rather than carrying a second, drifting copy.
        participant_count: None,
        max_attendees: input.max_attendees,
        remaining_attendees: None,
        external_participation_url: trimmed(&input.external_participation_url),
        // Anonymous participation is not offered on local events: an RSVP we
        // cannot attribute to an actor is one we can never Accept or Reject.
        anonymous_participation: Some(false),
        is_online: Some(input.is_online),
        comments_enabled: Some(true),
        category: None,
        location_url: None,
        location_street: trimmed(&input.location_street),
        location_locality: trimmed(&input.location_locality),
        location_region: trimmed(&input.location_region),
        location_country: trimmed(&input.location_country),
        location_postal_code: trimmed(&input.location_postal_code),
    }
}

/// The join modes a local event may use (§9.4: the full matrix).
pub(crate) const EVENT_JOIN_MODES: &[&str] = &["free", "restricted", "invite", "external"];
/// The ical statuses a local event may carry.
pub(crate) const EVENT_STATUSES: &[&str] = &["CONFIRMED", "TENTATIVE", "CANCELLED"];
/// Upper bound on a local event's advertised capacity. Not a real-world limit —
/// just a guard against a typo becoming a number no venue could mean.
const EVENT_MAX_CAPACITY: i32 = 1_000_000;

/// A validated local event, ready to store.
pub(crate) struct ValidatedEvent {
    pub start_time: time::OffsetDateTime,
    pub end_time: Option<time::OffsetDateTime>,
}

/// Validates a new local event.
///
/// Deliberately strict about the two things that make an event coherent — it has
/// a start, and it does not end before it starts — and about `external` needing
/// somewhere to send people. Everything else is optional, because a real event
/// often is: no end time, no address, no capacity.
pub(crate) fn validate_event(input: &EventParams) -> Result<ValidatedEvent, ApiError> {
    let fail = |message: &str| ApiError::Unprocessable(format!("Validation failed: {message}"));
    let parse = |value: &str| {
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
    };
    let Some(start_time) = parse(input.start_time.trim()) else {
        return Err(fail(
            "Event start time is required and must be a valid timestamp",
        ));
    };
    let end_time = match input
        .end_time
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(value) => match parse(value) {
            Some(parsed) => Some(parsed),
            None => return Err(fail("Event end time must be a valid timestamp")),
        },
        None => None,
    };
    if let Some(end) = end_time
        && end < start_time
    {
        return Err(fail("Event end time cannot be before its start time"));
    }
    if !EVENT_JOIN_MODES.contains(&input.join_mode.as_str()) {
        return Err(fail("Event join mode is invalid"));
    }
    if !EVENT_STATUSES.contains(&input.status.as_str()) {
        return Err(fail("Event status is invalid"));
    }
    // An `external` event with nowhere to send people offers no way to attend at
    // all — neither our RSVP button nor a link out.
    if input.join_mode == "external"
        && !input
            .external_participation_url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://"))
    {
        return Err(fail("An external event needs an https participation URL"));
    }
    if let Some(max) = input.max_attendees
        && !(1..=EVENT_MAX_CAPACITY).contains(&max)
    {
        return Err(fail("Event capacity must be a positive number"));
    }
    Ok(ValidatedEvent {
        start_time,
        end_time,
    })
}

// Mastodon's poll limits (PollOptionsValidator / PollExpirationValidator);
// the option count comes from the live instance settings instead.
const POLL_MAX_OPTION_CHARS: usize = 50;
const POLL_MIN_EXPIRATION_SECS: i64 = 5 * 60;
const POLL_MAX_EXPIRATION_SECS: i64 = 2_629_746; // 1 month

/// Validates a new poll against Mastodon's limits (option count from the
/// instance settings), returning the cleaned options and the absolute expiry.
pub(crate) fn validate_poll(
    input: &PollParams,
    max_options: i32,
) -> Result<(Vec<String>, time::OffsetDateTime), ApiError> {
    let options: Vec<String> = input
        .options
        .iter()
        .map(|option| option.trim().to_owned())
        .filter(|option| !option.is_empty())
        .collect();
    let fail = |message: &str| {
        Err(ApiError::Unprocessable(format!(
            "Validation failed: {message}"
        )))
    };
    if options.len() <= 1 {
        return fail("Options must have more than one item");
    }
    if options.len() > usize::try_from(max_options).unwrap_or(usize::MAX) {
        return fail(&format!(
            "Options can't contain more than {max_options} items"
        ));
    }
    if options
        .iter()
        .any(|option| option.chars().count() > POLL_MAX_OPTION_CHARS)
    {
        return fail("Options cannot be longer than 50 characters each");
    }
    let mut seen: Vec<&str> = Vec::with_capacity(options.len());
    for option in &options {
        if seen.contains(&option.as_str()) {
            return fail("Options contain duplicate items");
        }
        seen.push(option);
    }
    let Some(expires_in) = input.expires_in else {
        return fail("Expires at can't be blank");
    };
    if expires_in < POLL_MIN_EXPIRATION_SECS {
        return fail("Expires at is too soon");
    }
    if expires_in > POLL_MAX_EXPIRATION_SECS {
        return fail("Expires at is too far into the future");
    }
    let expires_at = time::OffsetDateTime::now_utc() + time::Duration::seconds(expires_in);
    Ok((options, expires_at))
}

/// A fresh status' stored poll, with the RFC 3339 expiry its outgoing
/// `Question` carries.
struct AttachedPoll {
    row: plamenu_db::poll::Poll,
    end_time: String,
}

impl AttachedPoll {
    fn as_note_poll(&self) -> NotePoll<'_> {
        NotePoll {
            options: &self.row.options,
            tallies: &self.row.cached_tallies,
            multiple: self.row.multiple,
            end_time: Some(&self.end_time),
            expired: false,
            voters_count: self.row.voters_count,
        }
    }
}

/// Stores a validated poll for a freshly-created status.
async fn attach_poll(
    conn: &mut plamenu_db::PgConnection,
    author: &Account,
    stored: &Status,
    input: &PollParams,
    options: &[String],
    expires_at: time::OffsetDateTime,
) -> Result<AttachedPoll, ApiError> {
    let row = poll::create(
        &mut *conn,
        poll::NewPoll {
            status_id: stored.id,
            account_id: author.id,
            options,
            cached_tallies: &vec![0; options.len()],
            multiple: input.multiple,
            hide_totals: input.hide_totals,
            voters_count: Some(0),
            expires_at: Some(expires_at),
        },
    )
    .await?;
    let end_time = expires_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok(AttachedPoll { row, end_time })
}

/// A plausible BCP 47 tag: 2–3 letter primary subtag plus optional
/// alphanumeric subtags (`en`, `pt-BR`). Mastodon validates against its
/// locale list; we only reject obvious garbage.
pub(crate) fn is_language_code(code: &str) -> bool {
    let mut subtags = code.split('-');
    let primary = subtags.next().unwrap_or("");
    matches!(primary.len(), 2..=3)
        && primary.bytes().all(|b| b.is_ascii_alphabetic())
        && subtags
            .all(|s| (1..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn validate_language(language: Option<&str>) -> Result<(), ApiError> {
    match language {
        Some(code) if !is_language_code(code) => Err(ApiError::Unprocessable(
            "Validation failed: Language is not included in the list".into(),
        )),
        _ => Ok(()),
    }
}

/// Mastodon's content-warning rules: a CW forces `sensitive` on (computed
/// first, so it survives the quirk), and a CW-only post becomes a plain post
/// of the CW text. Returns `(text, spoiler_text, sensitive)`.
pub(crate) fn apply_spoiler_rules<'a>(
    text: &'a str,
    spoiler_text: &'a str,
    sensitive: bool,
    has_quote: bool,
) -> (&'a str, &'a str, bool) {
    let sensitive = sensitive || !spoiler_text.trim().is_empty();
    if text.trim().is_empty() && !spoiler_text.trim().is_empty() && !has_quote {
        (spoiler_text, "", sensitive)
    } else {
        (text, spoiler_text, sensitive)
    }
}

/// The instance attachment-count limit, with Mastodon's refusal wording
/// (interpolating the live limit where Mastodon's locale hardcodes 4).
pub(crate) fn ensure_media_count(count: usize, max_media_attachments: i32) -> Result<(), ApiError> {
    if count > usize::try_from(max_media_attachments).unwrap_or(usize::MAX) {
        return Err(ApiError::Unprocessable(format!(
            "Cannot attach more than {max_media_attachments} files"
        )));
    }
    Ok(())
}

/// Mastodon's media validations shared by posting and editing: the ids
/// must all resolve (or the exact not-found wording names the strays),
/// video/audio is exclusive, and still-processing uploads cannot attach.
fn check_resolved_media(
    resolved: &[plamenu_db::media::Media],
    missing: &[i64],
) -> Result<(), ApiError> {
    if !missing.is_empty() {
        let ids: Vec<String> = missing.iter().map(ToString::to_string).collect();
        return Err(ApiError::Unprocessable(format!(
            "Media {} not found or already attached to another post",
            ids.join(", ")
        )));
    }
    // `audio_or_video?` excludes gifv — GIFs may mix with images.
    if resolved.len() > 1 && resolved.iter().any(Media::audio_or_video) {
        return Err(ApiError::Unprocessable(
            "Cannot attach a video to a post that already contains images".into(),
        ));
    }
    if resolved.iter().any(Media::not_processed) {
        return Err(ApiError::Unprocessable(
            "Cannot attach files that have not finished processing. Try again in a moment!".into(),
        ));
    }
    Ok(())
}

/// Media must exist, belong to the author, be done processing and still be
/// unattached — Mastodon's `PostStatusService#validate_media!`.
/// Validates the requested attachments and returns the resolved [`Media`] rows,
/// sorted by id (the on-the-wire attachment order). Returning them lets the
/// caller attach and build the outgoing `Document` objects without a read-back
/// query, so media attachment can run inside the status-creation transaction
/// without a pool round trip that would not see its own uncommitted write (QC
/// audit #18).
async fn validate_media_ids(
    state: &AppState,
    author: &Account,
    media_ids: &[i64],
    max_media_attachments: i32,
) -> Result<Vec<Media>, ApiError> {
    ensure_media_count(media_ids.len(), max_media_attachments)?;
    let owned = media::find_owned_many(&state.pool, media_ids, author.id).await?;
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for media_id in media_ids {
        match owned
            .iter()
            .find(|m| m.id == *media_id && m.status_id.is_none())
        {
            Some(item) => resolved.push(item.clone()),
            None => missing.push(*media_id),
        }
    }
    check_resolved_media(&resolved, &missing)?;
    resolved.sort_by_key(|m| m.id);
    Ok(resolved)
}

/// Quotes need a viewable, quotable (public/unlisted, non-boost) target whose
/// advertised interaction policy permits this author to quote automatically or
/// after manual review.
async fn resolve_quote_target(
    state: &AppState,
    author: &Account,
    quoted_status_id: Option<i64>,
) -> Result<Option<(Status, Account)>, ApiError> {
    let Some(quoted_id) = quoted_status_id else {
        return Ok(None);
    };
    let target = status::find_by_id(&state.pool, quoted_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if target.reblog_of_id.is_some() || !can_view(&state.pool, &target, Some(author.id)).await? {
        return Err(ApiError::NotFound);
    }
    if !matches!(target.visibility.as_str(), "public" | "unlisted") {
        return Err(ApiError::Unprocessable("This post cannot be quoted".into()));
    }
    let target_author = account::find_by_id(&state.pool, target.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Enforce the advertised policy before creating anything. A remote post
    // with no `interactionPolicy.canQuote` parses to no permitted audience, as
    // does an explicit self-only/disabled policy, so neither can leave a local
    // quote parked forever waiting for a server that will never approve it.
    // A genuine manual policy remains quotable and enters the QuoteRequest
    // handshake below. Local posts only advertise automatic policies.
    if !quote_allowed(state, &target, &target_author, author).await? {
        return Err(ApiError::Unprocessable(
            "Validation failed: Quoting is not allowed for this post".into(),
        ));
    }
    Ok(Some((target, target_author)))
}

/// Validates a quote target without creating a status. Scheduled-status paths
/// call this before persisting the draft so a forbidden remote quote cannot sit
/// in the scheduler only to fail every publication attempt later.
pub(crate) async fn validate_quote_target(
    state: &AppState,
    author: &Account,
    quoted_status_id: Option<i64>,
) -> Result<(), ApiError> {
    resolve_quote_target(state, author, quoted_status_id)
        .await
        .map(drop)
}

/// Whether `quoter` satisfies either the automatic or manual branch of the
/// target's quote policy. `public` allows anyone, `followers` requires the
/// quoter to follow the author, `following` requires the author to follow the
/// quoter, and an empty/disabled/unsupported policy denies the quote. The post
/// author may always quote themselves.
async fn quote_allowed(
    state: &AppState,
    target: &Status,
    target_author: &Account,
    quoter: &Account,
) -> Result<bool, ApiError> {
    if target.reblog_of_id.is_some() {
        return Ok(false);
    }
    // The author may always quote their own post, whatever the policy.
    if quoter.id == target_author.id {
        return Ok(true);
    }
    let policy = plamenu_ap::quote_policy::QuotePolicy::from_bitmap(target.quote_approval_policy);
    let automatic = policy.automatic();
    let manual = policy.manual();
    if automatic.public() || manual.public() {
        return Ok(true);
    }
    if (automatic.followers() || manual.followers())
        && follow::find(&state.pool, quoter.id, target_author.id)
            .await?
            .is_some_and(|edge| !edge.pending)
    {
        return Ok(true);
    }
    if (automatic.following() || manual.following())
        && follow::find(&state.pool, target_author.id, quoter.id)
            .await?
            .is_some_and(|edge| !edge.pending)
    {
        return Ok(true);
    }
    Ok(false)
}

/// Mastodon's `TextFormatter#add_quote_fallback`: the compatibility paragraph
/// is part of the post body itself, not presentation-only markup. That makes
/// the stored body, the served `ActivityPub` object and every Create/Update carry
/// the same `RE:` link. Quote-aware rendering removes it only once an accepted
/// native quote card is available.
fn add_quote_fallback(content: &str, quoted_url: &str) -> String {
    if quoted_url.is_empty() || content.contains(quoted_url) || content.contains("quote-inline") {
        return content.to_owned();
    }
    format!(
        r#"<p class="quote-inline">RE: {}</p>{content}"#,
        plamenu_ap::text::shortened_link_anchor(quoted_url),
    )
}

/// Resolves the human-facing URL Mastodon prefers for an edited quote's
/// fallback, retaining the structural URI when its target has since vanished.
async fn quote_fallback_url(
    state: &AppState,
    row: &quote::Quote,
) -> Result<Option<String>, ApiError> {
    if let Some(target_id) = row.quoted_status_id
        && let Some(target) = status::find_by_id(&state.pool, target_id).await?
        && let Some(author) = account::find_by_id(&state.pool, target.account_id).await?
    {
        return Ok(Some(status_web_url(
            &state.config.domain,
            &target,
            &author.username,
        )));
    }
    Ok(row.quoted_uri.clone())
}

/// Replies must point at a status the author can actually see.
async fn resolve_reply_parent(
    state: &AppState,
    author: &Account,
    in_reply_to_id: Option<i64>,
) -> Result<Option<Status>, ApiError> {
    let Some(parent_id) = in_reply_to_id else {
        return Ok(None);
    };
    let parent = status::find_by_id(&state.pool, parent_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if parent.reblog_of_id.is_some() || !can_view(&state.pool, &parent, Some(author.id)).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Some(parent))
}

/// The AP object URI of a reply's parent, if any.
async fn parent_note_uri(
    state: &AppState,
    parent: Option<&Status>,
) -> Result<Option<String>, ApiError> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    let parent_author = account::find_by_id(&state.pool, parent.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Some(status_uri_for_account(
        &state.config.domain,
        parent,
        &parent_author,
    )))
}

/// Quote bookkeeping carried from row creation to Note building.
struct PendingQuote {
    quoted_uri: String,
    authorization_uri: Option<String>,
    /// `(quote row id, quoted author's inbox)` when a `QuoteRequest` must go out.
    request: Option<(i64, String)>,
}

/// Validates and records a quote for a freshly created local status, on the
/// transaction connection `conn`. Returns the pending-quote bookkeeping and, for
/// a local target, the `(recipient, from)` of the "quote" notification to fire
/// after the transaction commits — the notification references the (still
/// uncommitted) status, so it cannot be created mid-transaction.
async fn begin_quote(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    author: &Account,
    stored: &Status,
    target: &Status,
    target_author: &Account,
) -> Result<(PendingQuote, Option<QuoteNotify>), ApiError> {
    let quoted_uri = status_uri_for_account(&state.config.domain, target, target_author);
    let our_uri = status_uri_for_account(&state.config.domain, stored, author);
    let quote_row_id = id::next();
    if target_author.is_local() {
        // The target's quote policy was already enforced in
        // `resolve_quote_target`, so a local target accepts immediately.
        let row = quote::create_in_tx(
            &mut *conn,
            quote::NewQuote {
                quote_id: quote_row_id,
                status_id: Some(stored.id),
                status_uri: &our_uri,
                account_id: author.id,
                quoted_status_id: Some(target.id),
                quoted_account_id: Some(target_author.id),
                state: "accepted",
                activity_uri: None,
                approval_uri: None,
                quoted_uri: Some(&quoted_uri),
                legacy: false,
            },
        )
        .await?;
        Ok((
            PendingQuote {
                quoted_uri,
                authorization_uri: Some(activity::quote_authorization_uri_for_actor(
                    &account_uri(&state.config.domain, target_author),
                    row.id,
                )),
                request: None,
            },
            Some(QuoteNotify {
                recipient: target_author.id,
                from: author.id,
            }),
        ))
    } else {
        let activity_uri = activity::quote_request_uri_for_actor(
            &account_uri(&state.config.domain, author),
            quote_row_id,
        );
        quote::create_in_tx(
            &mut *conn,
            quote::NewQuote {
                quote_id: quote_row_id,
                status_id: Some(stored.id),
                status_uri: &our_uri,
                account_id: author.id,
                quoted_status_id: Some(target.id),
                quoted_account_id: Some(target_author.id),
                state: "pending",
                activity_uri: Some(&activity_uri),
                approval_uri: None,
                quoted_uri: Some(&quoted_uri),
                legacy: false,
            },
        )
        .await?;
        Ok((
            PendingQuote {
                quoted_uri,
                authorization_uri: None,
                request: Some((quote_row_id, target_author.inbox_url.clone())),
            },
            None,
        ))
    }
}

/// The recipient/sender of a local "quote" notification, deferred until the
/// status-creation transaction commits.
struct QuoteNotify {
    recipient: i64,
    from: i64,
}

/// A quote inlines the post for the quoted author; in a DM they must be a
/// recipient, or the message would leak to them (Mastodon's safeguard).
fn ensure_direct_quote_is_addressed(
    author: &Account,
    composed: &crate::compose::Composed,
    quoted: Option<&(Status, Account)>,
) -> Result<(), ApiError> {
    if let Some((_, target_author)) = quoted
        && target_author.id != author.id
        && !composed.mentions.iter().any(|m| m.id == target_author.id)
    {
        return Err(ApiError::Unprocessable(
            "Validation failed: Cannot quote a non-mentioned user in a Private Mention post".into(),
        ));
    }
    Ok(())
}

fn validate_visibility(visibility: &str) -> Result<(), ApiError> {
    if matches!(
        visibility,
        "public" | "unlisted" | "private" | "direct" | "local"
    ) {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(format!(
            "visibility '{visibility}' is not supported"
        )))
    }
}

/// Attaches pre-uploaded media (the id-sorted rows [`validate_media_ids`]
/// resolved) to a fresh status and returns the AP `Document` objects for the
/// outgoing Note. The attachment `UPDATE` runs on `conn` — inside the
/// status-creation transaction — and the `Document`s are built from the already
/// in-hand rows rather than a read-back that a separate pool connection could
/// not see mid-transaction.
async fn attach_status_media(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    author: &Account,
    stored: &Status,
    media: &[Media],
) -> Result<Vec<Value>, ApiError> {
    if media.is_empty() {
        return Ok(Vec::new());
    }
    let media_ids: Vec<i64> = media.iter().map(|m| m.id).collect();
    media::attach(&mut *conn, &media_ids, stored.id, author.id).await?;
    Ok(crate::entities::ap_attachments(&state.config.domain, media))
}

/// The outgoing `tag` array of a fresh status: the composed
/// mentions/hashtags plus `Emoji` entries for custom emoji in the text —
/// matching what `note_for_status` renders for every later representation
/// of the post.
async fn outgoing_tags(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    composed: &crate::compose::Composed,
    stored: &Status,
    attached_poll: Option<&AttachedPoll>,
) -> Result<(Vec<Value>, Vec<i64>), ApiError> {
    let mut tag_json = composed.tag_json.clone();
    let mut emoji_texts: Vec<&str> = vec![&stored.spoiler_text, &stored.content];
    if let Some(poll) = attached_poll {
        emoji_texts.extend(poll.row.options.iter().map(String::as_str));
    }
    // Custom-emoji definitions are committed data — read on the pool.
    let emoji_rows = custom_emoji::lookup_local_for_account(
        &state.pool,
        stored.account_id,
        &crate::emoji::shortcodes_of(&emoji_texts),
    )
    .await?;
    tag_json.extend(crate::emoji::emoji_tags_from(
        &state.config.domain,
        &emoji_rows,
        &emoji_texts,
    )?);
    let emoji_ids: Vec<i64> = emoji_rows.iter().map(|emoji| emoji.id).collect();
    let origin_ids = custom_emoji::find_managed_by_ids(&state.pool, &emoji_ids)
        .await?
        .into_iter()
        .map(|emoji| emoji.origin_id)
        .collect();
    // Referenced collections (FEP-7aa9) ride the `tag` array as
    // `FeaturedCollection` objects, like Mastodon's `NoteSerializer`. The
    // status' `tagged_objects` were just written on `conn`, so the read must
    // run there to see them mid-transaction.
    tag_json.extend(
        crate::collections::note_tagged_collection_tags(state, &mut *conn, stored.id).await?,
    );
    Ok((tag_json, origin_ids))
}

/// Creates a local status and enqueues its `Create(Note)` for every follower
/// inbox (and the parent's author, for replies); a direct message is instead
/// delivered to its mentioned actors only. Returns the status and the number
/// of deliveries enqueued.
pub async fn post_status(
    state: &AppState,
    params: PostParams<'_>,
) -> Result<(Status, usize), ApiError> {
    post_status_inner(state, params, None, None, None).await
}

pub async fn post_webxdc_invitation(
    state: &AppState,
    params: PostParams<'_>,
    invitation: WebxdcInvitation<'_>,
) -> Result<(Status, usize), ApiError> {
    post_status_inner(state, params, None, Some(invitation), None).await
}

/// Creates a local status through an OAuth application. The app attribution is
/// stored before federation/streaming side effects so REST renders are stable.
pub async fn post_status_for_application(
    state: &AppState,
    params: PostParams<'_>,
    application_id: i64,
) -> Result<(Status, usize), ApiError> {
    post_status_inner(state, params, Some(application_id), None, None).await
}

/// Publishes a claimed scheduled entry. Its deletion, post, attachments, and
/// outgoing jobs commit together; a stale claim cannot create another post.
pub(crate) async fn post_scheduled_status(
    state: &AppState,
    params: PostParams<'_>,
    scheduled: &plamenu_db::scheduled_status::ScheduledStatus,
) -> Result<(Status, usize), ApiError> {
    post_status_inner(
        state,
        params,
        scheduled.application_id,
        None,
        Some((scheduled.id, scheduled.publish_generation)),
    )
    .await
}

struct LocalStatusRecord<'a> {
    author: &'a Account,
    content: &'a str,
    text: &'a str,
    content_type: &'a str,
    visibility: &'a str,
    in_reply_to_id: Option<i64>,
    spoiler_text: &'a str,
    sensitive: bool,
    language: Option<&'a str>,
    quote_approval_policy: Option<i32>,
    application_id: Option<i64>,
    title: Option<&'a str>,
    external_url: Option<&'a str>,
    /// The AS2 type to serve this post as, when the author picked one (E4).
    object_type: Option<&'a str>,
}

async fn create_local_status_record(
    conn: &mut plamenu_db::PgConnection,
    draft: LocalStatusRecord<'_>,
) -> Result<Status, ApiError> {
    let mut stored = status::create_local(
        &mut *conn,
        status::NewLocalStatus {
            account_id: draft.author.id,
            content: draft.content,
            text: draft.text,
            content_type: draft.content_type,
            visibility: draft.visibility,
            in_reply_to_id: draft.in_reply_to_id,
            spoiler_text: draft.spoiler_text,
            sensitive: draft.sensitive,
            language: draft.language,
            quote_approval_policy: draft.quote_approval_policy,
            title: draft.title,
            external_url: draft.external_url,
            object_type: draft.object_type,
        },
    )
    .await?;
    if let Some(application_id) = draft.application_id {
        stored = status::set_application(&mut *conn, stored.id, application_id)
            .await?
            .ok_or(ApiError::NotFound)?;
    }
    Ok(stored)
}

/// The conversation root's visibility for a reply's parent — authoritative for
/// clamping a reply's audience (FEP-171b). Falls back to the parent's own
/// visibility when the root is unknown (a remote conversation we know only by
/// URI, or an older thread).
async fn conversation_root_visibility(
    state: &AppState,
    parent: &Status,
) -> Result<String, ApiError> {
    if let Some(ctx) = conversation::context_of_status(&state.pool, parent.id).await?
        && let Some(visibility) = ctx.root_visibility
    {
        return Ok(visibility);
    }
    Ok(parent.visibility.clone())
}

/// Inherits a private/direct reply's audience from its parent (FEP-171b: the
/// audience of a reply is copied from the conversation, never widened). The
/// parent author and everyone the parent addressed become **silent** recipients
/// of the reply — added to its `to`/`cc`, delivery inboxes and, crucially, the
/// Note's `tag` array as Mention objects — so a reply into a closed thread
/// reaches exactly that audience even when the user did not re-mention anyone.
/// The Mention tags are not cosmetic: Mastodon demotes an audience-only direct
/// Note to unnotified `limited` and `GoToSocial` hides it outright; both grant
/// DM visibility solely from `tag`. A no-op for public/unlisted replies (their
/// audience is already open) and for non-replies. Silent attaches never
/// downgrade an existing text mention (`ON CONFLICT DO NOTHING`).
#[allow(clippy::too_many_arguments)]
async fn inherit_reply_audience(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    author: &Account,
    stored: &Status,
    parent: &Status,
    mentioned_uris: &mut Vec<String>,
    mention_inboxes: &mut Vec<String>,
    mention_tags: &mut Vec<Value>,
) -> Result<(), ApiError> {
    if !matches!(stored.visibility.as_str(), "private" | "direct") {
        return Ok(());
    }
    // The parent and its recipients are committed data, read on the pool; only
    // the new silent-mention rows are written on `conn`.
    let mut audience: Vec<Account> = Vec::new();
    if let Some(parent_author) = account::find_by_id(&state.pool, parent.account_id).await? {
        audience.push(parent_author);
    }
    // All of the parent's recipients (silent included) — the full audience.
    if let Some(mentioned) = mention::for_statuses(&state.pool, &[parent.id], false)
        .await?
        .remove(&parent.id)
    {
        audience.extend(mentioned);
    }
    let mut recipients: Vec<Account> = Vec::new();
    for account in audience {
        if account.id == author.id
            || account.id == stored.account_id
            || recipients.iter().any(|seen| seen.id == account.id)
        {
            continue;
        }
        recipients.push(account);
    }
    let silent_rows: Vec<(i64, bool)> = recipients.iter().map(|a| (a.id, true)).collect();
    mention::attach_many(&mut *conn, stored.id, &silent_rows).await?;
    let recipient_ids: Vec<i64> = recipients.iter().map(|account| account.id).collect();
    let following = if author.silenced() {
        follow::pending_state_in_batch(&state.pool, author.id, &recipient_ids).await?
    } else {
        std::collections::HashMap::new()
    };
    for account in recipients {
        let uri = if account.is_local() {
            Some(
                LocalUserUrls::for_account(
                    &state.config.domain,
                    &account.username,
                    account.uri.as_deref(),
                )
                .id,
            )
        } else {
            account.uri.clone()
        };
        let address = !author.silenced() || following.contains_key(&account.id);
        if let Some(uri) = uri {
            let acct = if account.has_local_account_on(&state.config.domain) {
                format!("{}@{}", account.username, state.config.account_domain)
            } else {
                format!(
                    "{}@{}",
                    account.username,
                    account.domain.as_deref().unwrap_or_default()
                )
            };
            mention_tags.push(json!({
                "type": "Mention", "href": uri,
                "name": format!("@{acct}"),
            }));
            if address && !mentioned_uris.contains(&uri) {
                mentioned_uris.push(uri);
            }
        }
        if address
            && !account.is_local()
            && !account.inbox_url.is_empty()
            && !mention_inboxes.contains(&account.inbox_url)
        {
            mention_inboxes.push(account.inbox_url.clone());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn post_status_inner(
    state: &AppState,
    params: PostParams<'_>,
    application_id: Option<i64>,
    webxdc_invitation: Option<WebxdcInvitation<'_>>,
    scheduled_claim: Option<(i64, i64)>,
) -> Result<(Status, usize), ApiError> {
    validate_visibility(params.visibility)?;
    let author = account::find_local_by_username(&state.pool, params.username)
        .await?
        .ok_or(ApiError::NotFound)?;
    validate_language(params.language)?;
    let limits = state.settings_cache.get(&state.pool).await?;
    // The authored kind, normalised once. Event params *are* the event kind (the
    // shape every caller has passed since E4, and the composer gates them on an
    // explicit choice of their own); `kind` is what selects long-form.
    let kind = match (params.kind, params.event.is_some()) {
        (activity::PostKind::Article, true) => {
            return Err(ApiError::Unprocessable(
                "Validation failed: A post is either long-form or an event, not both".into(),
            ));
        }
        (activity::PostKind::Event, false) => {
            return Err(ApiError::Unprocessable(
                "Validation failed: An event needs a start time".into(),
            ));
        }
        (_, true) => activity::PostKind::Event,
        (authored, false) => authored,
    };
    let long_form = kind == activity::PostKind::Article;
    let (text, spoiler_text, sensitive) = apply_spoiler_rules(
        params.text,
        params.spoiler_text,
        params.sensitive,
        params.quoted_status_id.is_some(),
    );
    // Length is checked before compose so an over-long draft never triggers
    // the mention webfinger fetches. A long-form post is measured against its own
    // (much larger) limit — a 500-character article is not one.
    ensure_within_character_limit(
        text,
        spoiler_text,
        if long_form {
            limits.max_characters_long_form
        } else {
            limits.max_characters
        },
    )?;
    // A session URL is the invitation's source of truth, including scheduled
    // posts and redrafts. Only recognize cached apps; never fetch on posting.
    let detected_invitation = if webxdc_invitation.is_none()
        && kind == activity::PostKind::Note
        && params.group_id.is_none()
    {
        crate::webxdc::invitation_in_text(&state.pool, text).await?
    } else {
        None
    };
    let webxdc_invitation = webxdc_invitation.or_else(|| {
        detected_invitation
            .as_ref()
            .map(|(id, uri, name)| WebxdcInvitation {
                session_id: *id,
                session_uri: uri,
                session_name: name,
            })
    });
    let composed = compose(state, text, params.content_type).await?;
    let mut html = composed.html.clone();
    // A group submission's title/link counts as content (a body-less titled
    // thread is valid); the title-only-on-group rule is enforced just below.
    let has_typed_content = params.title.is_some_and(|t| !t.trim().is_empty())
        || params.external_url.is_some_and(|u| !u.trim().is_empty());
    ensure_status_has_content(&html, params.media_ids, has_typed_content)?;
    let resolved_media = validate_media_ids(
        state,
        &author,
        params.media_ids,
        limits.max_media_attachments,
    )
    .await?;
    let validated_poll = params
        .poll
        .as_ref()
        .map(|poll| validate_poll(poll, limits.poll_max_options))
        .transpose()?;

    let parent = resolve_reply_parent(state, &author, params.in_reply_to_id).await?;
    let quoted = resolve_quote_target(state, &author, params.quoted_status_id).await?;
    if let Some((target, target_author)) = &quoted {
        let quoted_url = status_web_url(&state.config.domain, target, &target_author.username);
        html = add_quote_fallback(&html, &quoted_url);
    }

    // A reply may not widen the audience past its conversation root
    // (FEP-171b). Clamp the requested visibility to no broader than the root's;
    // the reply's audience is then inherited below for private/direct threads.
    // A direct parent additionally forces a direct reply (Akkoma parity, a
    // deliberate Mastodon divergence): a "whisper" reply under a broader root
    // must never be answered in the open, or the answer leaks thread context.
    let clamped_visibility: &str = match parent.as_ref() {
        Some(parent) if parent.visibility == "direct" => "direct",
        Some(parent) => {
            let root_visibility = conversation_root_visibility(state, parent).await?;
            plamenu_ap::activity::clamp_visibility(params.visibility, &root_visibility)
        }
        None => params.visibility,
    };
    // Local account limitation demotes a requested public post to unlisted.
    // Followers still receive it, but it is no longer addressed primarily to
    // the public collection or eligible for public/hashtag fan-out.
    let visibility = if author.silenced() && clamped_visibility == "public" {
        "unlisted"
    } else {
        clamped_visibility
    };

    // Event fields (E4): validated before the transaction opens, like the poll,
    // so a malformed date is a 422 rather than a rolled-back post.
    let validated_event = params.event.as_ref().map(validate_event).transpose()?;
    if params.event.is_some() && params.title.map(str::trim).unwrap_or_default().is_empty() {
        // Not a nicety: Mobilizon's Event model requires a title, and a calendar
        // entry with no name is unreadable in every list view that will carry it.
        // An untitled event is refused rather than silently given a generated one.
        return Err(ApiError::Unprocessable(
            "Validation failed: An event needs a title".into(),
        ));
    }
    if long_form {
        // What a long-form post may not also be. Each refusal is a shape another
        // kind already owns, or a shape the fediverse drops on the floor
        // (`LONGFORM_DESIGN.md` §3) — refusing is how the author finds out here
        // rather than from a headless post on someone else's timeline.
        if params.title.map(str::trim).unwrap_or_default().is_empty() {
            return Err(ApiError::Unprocessable(
                "Validation failed: A long-form post needs a title".into(),
            ));
        }
        if params.in_reply_to_id.is_some() {
            // `name` is never hoisted from a reply — our rule, Mitra's and
            // Lemmy's — so a titled long-form reply loses its title everywhere,
            // including here.
            return Err(ApiError::Unprocessable(
                "Validation failed: A long-form post can't be a reply".into(),
            ));
        }
        if params.group_id.is_some() {
            // A community submission is a `Page` (Lemmy's native post shape).
            return Err(ApiError::Unprocessable(
                "Validation failed: A long-form post can't be submitted to a group".into(),
            ));
        }
        if params.external_url.is_some_and(|u| !u.trim().is_empty()) {
            // The leading `Link` attachment is the Lemmy link-post shape, which
            // belongs to `Page`.
            return Err(ApiError::Unprocessable(
                "Validation failed: A long-form post can't be a link post".into(),
            ));
        }
    }
    // Group submission fields: validate and resolve the target groups
    // — an explicit group for a top-level post, the parent's for a reply.
    let title = params.title.map(str::trim).filter(|t| !t.is_empty());
    let external_url = params.external_url.map(str::trim).filter(|u| !u.is_empty());
    let group_targets = crate::groups::resolve_compose_targets(
        state,
        &author,
        &crate::groups::SubmissionParams {
            group_id: params.group_id,
            title,
            external_url,
            visibility,
            has_poll: params.poll.is_some(),
            kind,
        },
        parent.as_ref(),
    )
    .await?;
    // The community claim: a local group's own URL, or a remote community's
    // stored actor URI.
    let group_uri = group_targets
        .first()
        .map(|g| crate::groups::community_uri(state, g));

    if visibility == "direct" {
        ensure_direct_quote_is_addressed(&author, &composed, quoted.as_ref())?;
    }
    if visibility == "local"
        && quoted
            .as_ref()
            .is_some_and(|(_, target_author)| !target_author.is_local())
    {
        return Err(ApiError::Unprocessable(
            "Local-only posts cannot quote remote posts".into(),
        ));
    }

    // Everything durable — the status row, its child rows (quote, poll, media,
    // conversation, mentions, hashtags, tag uses, direct-message rows, linked
    // collections) and every delivery/outbox job — commits in ONE transaction,
    // so a failure anywhere rolls the whole post back rather than leaving a
    // half-assembled status, a post delivered to only part of its audience, or
    // — worst — a committed post with no queued deliveries that a client retry
    // would duplicate work (the transactional-outbox pattern). Every
    // remote fetch already happened above (compose/mention webfinger, reply and
    // quote-target resolution, media validation), so the transaction spans no
    // network I/O. Side effects that reference the still-uncommitted status
    // (notifications) or are regenerable (streaming, webhooks) run only after
    // the commit. Local-group Announce jobs belong to the transaction.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let conn = &mut *tx;
    if let Some((id, generation)) = scheduled_claim
        && !plamenu_db::scheduled_status::consume_claim(conn, id, author.id, generation).await?
    {
        return Err(ApiError::Conflict(
            "Scheduled post claim is no longer current".into(),
        ));
    }

    let stored = create_local_status_record(
        conn,
        LocalStatusRecord {
            author: &author,
            content: &html,
            text,
            content_type: params.content_type.media_type(),
            visibility,
            in_reply_to_id: parent.as_ref().map(|p| p.id),
            spoiler_text,
            sensitive,
            language: params.language,
            quote_approval_policy: params.quote_approval_policy,
            application_id,
            title,
            external_url,
            // The one place the wire type is decided for a local post: what the
            // author picked, plus the two implied kinds. The column says so, and
            // every representation (served object, Create, entity, render)
            // follows from it.
            object_type: object_type_column(stored_kind(
                kind,
                params.poll.is_some(),
                title.is_some(),
            )),
        },
    )
    .await?;
    if let Some(invitation) = webxdc_invitation {
        plamenu_db::webxdc::attach_invitation_tx(conn, stored.id, invitation.session_id).await?;
    }
    let (note_quote, quote_notify) = match &quoted {
        Some((target, target_author)) => {
            let (pending, notify) =
                begin_quote(state, conn, &author, &stored, target, target_author).await?;
            (Some(pending), notify)
        }
        None => (None, None),
    };
    let attached_poll = match (&params.poll, validated_poll) {
        (Some(input), Some((options, expires_at))) => {
            Some(attach_poll(conn, &author, &stored, input, &options, expires_at).await?)
        }
        _ => None,
    };
    // The event sidecar rides the same transaction as the status row: a status
    // typed `Event` with no sidecar would serialize as an event with no date.
    if let (Some(input), Some(valid)) = (&params.event, validated_event.as_ref()) {
        plamenu_db::status_event::upsert(&mut *conn, &event_sidecar_of(stored.id, input, valid))
            .await?;
    }
    let attachments = attach_status_media(state, conn, &author, &stored, &resolved_media).await?;

    // Conversation first, so a reply into a muted thread won't notify. A local
    // status carries no remote context IRIs — a fresh root becomes locally
    // owned, a reply inherits its parent's conversation.
    crate::conversations::ensure_conversation(
        conn,
        &stored,
        stored.in_reply_to_id.is_some(),
        plamenu_db::conversation::ContextRefs::default(),
    )
    .await?;
    let (mut mentioned_uris, mut mention_inboxes, mention_notify) =
        persist_compose_artifacts(state, conn, &stored, &composed, &[], author.silenced()).await?;
    // A local group's attribution rides a silent mention row — the same
    // rule inbound submissions get from `store_audience_mentions`, so edit and
    // delete paths find the group the same way. A remote community is
    // instead attributed by the boost row it creates when it announces the post
    // back; no mention row is stored for it.
    for group_account in &group_targets {
        if group_account.is_local() {
            mention::attach(&mut *conn, stored.id, group_account.id, true).await?;
        }
    }
    // Deliver the submission to each remote community's inbox so the origin
    // announces it. Followers reached via the ordinary fan-out below.
    for group_account in &group_targets {
        if !group_account.is_local() {
            mention_inboxes.push(group_account.inbox_url.clone());
        }
    }
    // A private/direct reply inherits the parent's audience (before the
    // DM fan-out and Note build, so both address the whole thread).
    let mut inherited_mention_tags: Vec<Value> = Vec::new();
    if let Some(parent) = parent.as_ref() {
        inherit_reply_audience(
            state,
            conn,
            &author,
            &stored,
            parent,
            &mut mentioned_uris,
            &mut mention_inboxes,
            &mut inherited_mention_tags,
        )
        .await?;
    }
    // Register tag usage for the ranking engine, like Mastodon's
    // `Trends.tags.register` on post. Edits don't re-register.
    tag::record_uses(
        &mut *conn,
        stored.id,
        time::OffsetDateTime::now_utc().date(),
    )
    .await?;
    crate::conversations::record_direct_status(state, conn, &author, &stored).await?;
    // Link scanning (Mastodon's `ProcessLinksService`): attach referenced
    // collections so they ride the outgoing `tag` array and the Status entity.
    crate::collections::scan_and_link_collections(state, conn, &stored).await?;

    let parent_uri = parent_note_uri(state, parent.as_ref()).await?;
    let (mut tag_json, emoji_origin_ids) =
        outgoing_tags(state, conn, &composed, &stored, attached_poll.as_ref()).await?;
    if matches!(stored.visibility.as_str(), "public" | "unlisted" | "local") {
        custom_emoji::record_post_usage_on(conn, stored.account_id, &emoji_origin_ids).await?;
    }
    // The inherited thread audience rides `tag` as real Mentions, exactly as
    // typed ones would — receivers that grant DM visibility from `tag` alone
    // (Mastodon, GoToSocial) must see them (`note_for_status` mirrors this).
    tag_json.extend(inherited_mention_tags);
    let (context, context_history) =
        crate::note::context_links_for(state, &mut *conn, &stored).await?;
    // Read the sidecar back rather than reusing the input: this is the same
    // preparation the served object and every later edit use, so the Create and
    // the object at the post's own URL cannot disagree.
    let prepared_event = match params.event {
        Some(_) => crate::note::event_for_note_conn(&mut *conn, stored.id).await?,
        None => None,
    };
    let mut create = activity::create_note(&NoteParams {
        // Derived from the row we just wrote, like the served object is — never
        // from the request, so the `Create` and the object at the post's own URL
        // state the same type.
        kind: activity::PostKind::of_stored(
            stored.object_type.as_deref(),
            attached_poll.is_some(),
            stored.title.is_some(),
        ),
        domain: &state.config.domain,
        username: &author.username,
        actor_id: author.uri.as_deref(),
        status_id: stored.id,
        content_html: &html,
        source: Some(plamenu_ap::activity::NoteSource {
            content: text,
            media_type: params.content_type.media_type(),
        }),
        published: &published_of(&stored)?,
        updated: None,
        visibility,
        summary: (!stored.spoiler_text.is_empty()).then_some(stored.spoiler_text.as_str()),
        sensitive: author.sensitized() || stored.sensitive,
        language: stored.language.as_deref(),
        in_reply_to_uri: parent_uri.as_deref(),
        attachments: &attachments,
        tag: &tag_json,
        mentioned_uris: &mentioned_uris,
        quote: note_quote
            .as_ref()
            .map(|q| plamenu_ap::activity::NoteQuote {
                quoted_uri: &q.quoted_uri,
                authorization_uri: q.authorization_uri.as_deref(),
            }),
        quote_approval_policy: stored.quote_approval_policy,
        poll: attached_poll.as_ref().map(AttachedPoll::as_note_poll),
        // A status that was just created has no replies or engagement yet.
        self_reply_ids: &[],
        favourites_count: 0,
        reblogs_count: 0,
        title,
        external_url,
        group_uri: group_uri.as_deref(),
        context: context.as_deref(),
        context_history: context_history.as_deref(),
        event: prepared_event
            .as_ref()
            .map(crate::note::PreparedEvent::as_note_event),
    });
    if let Some(invitation) = webxdc_invitation {
        crate::webxdc::enhance_invitation(
            &mut create,
            invitation.session_uri,
            invitation.session_name,
        );
    }

    let deliveries = deliver_create_tx(
        state,
        conn,
        &author,
        &stored,
        &create,
        parent.as_ref(),
        mention_inboxes.clone(),
    )
    .await?;
    // As owner of a private conversation, wrap this reply in an `Add` and
    // distribute it to the known participants (FEP-171b). Flagged, and a no-op
    // unless the conversation is a private/direct one we own.
    crate::containers::distribute_reply_add(
        state,
        conn,
        &stored,
        &create,
        &mentioned_uris,
        &mention_inboxes,
    )
    .await?;
    send_quote_request(state, conn, &author, note_quote.as_ref(), &create).await?;
    // Link preview crawl (Mastodon's LinkCrawlWorker). Queued only when the
    // body could carry a link at all: the worker still decides whether there
    // is an *eligible* one, but it was spending four or five round trips per
    // post to discover there was no link of any kind, which is most posts.
    if crate::link_preview::may_carry_link(&stored) {
        plamenu_db::preview_card::enqueue_crawl(&mut *conn, stored.id).await?;
    }

    let mut group_streams = Vec::new();
    if stored.visibility == "public" {
        for group_account in &group_targets {
            if group_account.is_local()
                && let Some(boost_id) = crate::groups::announce_submission_conn(
                    state,
                    conn,
                    group_account,
                    &stored,
                    &create,
                )
                .await?
            {
                group_streams.push(boost_id);
            }
        }
    }

    tx.commit().await.map_err(plamenu_db::DbError::from)?;

    // === Post-commit side effects: the post is durable, so these are best
    // effort — a failure is logged, never propagated as if the post failed. ===
    // Notifications reference the now-committed status.
    if let Some(QuoteNotify { recipient, from }) = quote_notify {
        notify_after_commit(state, recipient, from, "quote", Some(stored.id)).await;
    }
    if let Err(err) =
        notification::create_mentions_many(&state.pool, &mention_notify, author.id, stored.id).await
    {
        tracing::warn!(
            error = %err,
            status_id = stored.id,
            "post-commit mention notifications failed"
        );
    }
    for boost_id in group_streams {
        crate::streaming::status_created(state, boost_id).await;
    }
    if let Err(err) = notify_new_status_followers(state, &stored).await {
        tracing::warn!(error = %err, status_id = stored.id, "post-commit follower notification failed");
    }
    crate::streaming::status_created(state, stored.id).await;
    crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_CREATED, &stored).await;
    Ok((stored, deliveries))
}

/// Notifies local followers who opted into per-follow `notify` about a
/// fresh post by `stored`'s author — Mastodon's `status` notification
/// (`FeedInsertWorker#perform_notify`). Boosts, replies to anyone but the
/// author themself, and DMs never notify; the per-follow language filter
/// and mutes/blocks apply like the home timeline.
pub async fn notify_new_status_followers(
    state: &AppState,
    stored: &Status,
) -> Result<(), ApiError> {
    if stored.reblog_of_id.is_some() || stored.visibility == "direct" {
        return Ok(());
    }
    if let Some(parent_id) = stored.in_reply_to_id {
        let parent_author = status::find_by_id(&state.pool, parent_id)
            .await?
            .map(|parent| parent.account_id);
        if parent_author != Some(stored.account_id) {
            return Ok(());
        }
    }
    let followers =
        follow::notify_follower_ids(&state.pool, stored.account_id, stored.language.as_deref())
            .await?;
    notification::create_status_many(&state.pool, &followers, stored.account_id, stored.id).await?;
    Ok(())
}

/// Delivers a fresh status' `Create` to its audience within the status-creation
/// transaction: followers, mentioned remote actors and (for
/// replies) the parent's remote author — but for a direct message the mentioned
/// actors only, not followers, and not even the reply parent's author unless
/// they are mentioned.
///
/// The audience reads (parent author, relay inboxes, follower inboxes) hit
/// committed data on the pool, but every delivery job is enqueued on `conn`, so
/// the whole outbox commits atomically with the status and its child rows — a
/// crash can never leave a post that was delivered to only part of its audience
/// (or committed with no deliveries at all, so a client retry duplicates it).
async fn deliver_create_tx(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    author: &Account,
    stored: &Status,
    create: &Value,
    parent: Option<&Status>,
    mut mention_inboxes: Vec<String>,
) -> Result<usize, ApiError> {
    if stored.visibility == "local" {
        return Ok(0);
    }
    if stored.visibility == "direct" {
        return deliver_to_inboxes_conn(state, conn, author, create, &mention_inboxes).await;
    }
    if let Some(parent) = parent {
        let (parent_author, inbox) = remote_author_inbox(state, parent).await?;
        let address_parent = !author.silenced()
            || follow::find(&state.pool, parent_author.id, author.id)
                .await?
                .is_some();
        if address_parent && let Some(inbox) = inbox {
            mention_inboxes.push(inbox);
        }
    }
    mention_inboxes
        .extend(crate::relays::enabled_inboxes_for_public(state, &stored.visibility).await?);
    let inboxes = fan_out_inboxes(&mut *conn, author, &mention_inboxes).await?;
    if stored.visibility == "private" {
        job::enqueue_many_synchronized_tx(&mut *conn, author.id, &inboxes, create).await?;
    } else {
        job::enqueue_many_tx(&mut *conn, author.id, &inboxes, create).await?;
    }
    Ok(inboxes.len())
}

/// The deduplicated inbox set for a public/followers fan-out: the signer's
/// follower inboxes (committed data, read on the pool) plus any extras. Shared
/// by [`fan_out_inner_conn`] and the post-creation delivery path.
async fn fan_out_inboxes<'e, E: plamenu_db::PgExecutor<'e>>(
    executor: E,
    signer: &Account,
    extras: &[String],
) -> Result<Vec<String>, ApiError> {
    let mut inboxes = follow::follower_inboxes(executor, signer.id).await?;
    for inbox in extras {
        if !inbox.is_empty() && !inboxes.iter().any(|i| i == inbox) {
            inboxes.push(inbox.clone());
        }
    }
    Ok(inboxes)
}

/// Quoting a remote post: ask its author for consent, inlining our post.
async fn send_quote_request(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    author: &Account,
    note_quote: Option<&PendingQuote>,
    create: &Value,
) -> Result<(), ApiError> {
    if let Some(PendingQuote {
        quoted_uri,
        request: Some((quote_row_id, inbox)),
        ..
    }) = note_quote
    {
        let request = activity::quote_request(
            &state.config.domain,
            &author.username,
            *quote_row_id,
            quoted_uri,
            create["object"].clone(),
        );
        job::enqueue_tx(&mut *conn, author.id, inbox, &request).await?;
    }
    Ok(())
}

/// Stores a composed status' hashtag and mention rows on `conn` — inside the
/// status-creation (or edit) transaction — and returns the mentioned actor IRIs
/// (for `cc`), the remote inboxes to deliver to, and the local mention-recipient
/// ids whose "mention" notification the caller must fire *after* the transaction
/// commits (a notification references the still-uncommitted status, and edits do
/// not re-notify accounts in `already_mentioned`).
async fn persist_compose_artifacts(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    stored: &Status,
    composed: &crate::compose::Composed,
    already_mentioned: &[i64],
    limit_addressing_to_followers: bool,
) -> Result<(Vec<String>, Vec<String>, Vec<i64>), ApiError> {
    let hashtag_names: Vec<&str> = composed.hashtags.iter().map(String::as_str).collect();
    tag::ensure_and_attach_many(&mut *conn, stored.id, &hashtag_names).await?;
    let mention_rows: Vec<(i64, bool)> = composed.mentions.iter().map(|m| (m.id, false)).collect();
    mention::attach_many(&mut *conn, stored.id, &mention_rows).await?;
    let mut mentioned_uris = Vec::new();
    let mut mention_inboxes = Vec::new();
    let mut notify_local_ids = Vec::new();
    let mention_ids: Vec<i64> = composed.mentions.iter().map(|account| account.id).collect();
    let following = if limit_addressing_to_followers {
        follow::pending_state_in_batch(&mut *conn, stored.account_id, &mention_ids).await?
    } else {
        std::collections::HashMap::new()
    };
    for mentioned in &composed.mentions {
        let address = !limit_addressing_to_followers || following.contains_key(&mentioned.id);
        if mentioned.is_local() {
            if !already_mentioned.contains(&mentioned.id) {
                notify_local_ids.push(mentioned.id);
            }
            if address {
                mentioned_uris.push(
                    LocalUserUrls::for_account(
                        &state.config.domain,
                        &mentioned.username,
                        mentioned.uri.as_deref(),
                    )
                    .id,
                );
            }
        } else if address {
            if let Some(uri) = &mentioned.uri {
                mentioned_uris.push(uri.clone());
            }
            mention_inboxes.push(mentioned.inbox_url.clone());
        }
    }
    Ok((mentioned_uris, mention_inboxes, notify_local_ids))
}

/// The fields of a status edit — all optional; absent fields keep their
/// current value, like Mastodon's `PUT /api/v1/statuses/{id}`.
#[derive(Debug, Default)]
pub struct EditParams<'a> {
    pub text: Option<&'a str>,
    /// The format the (possibly kept) text is authored in; `None` keeps the
    /// stored `content_type`, like the other absent fields.
    pub content_type: Option<PostFormat>,
    pub spoiler_text: Option<&'a str>,
    pub sensitive: Option<bool>,
    pub language: Option<&'a str>,
    /// `None` keeps the current attachments; `Some` reconciles to the list.
    pub media_ids: Option<Vec<i64>>,
    /// Resolved quote-approval bitmap; `None` keeps the current policy
    /// (Mastodon applies the param only when present — no preference fallback
    /// on edit).
    pub quote_approval_policy: Option<i32>,
    /// Per-attachment attribute updates (Mastodon's `media_attributes`):
    /// alt text and focal point for attachments kept by this edit. Entries
    /// naming media outside the edited status' final attachment set are
    /// ignored, like Mastodon's `next_media_attachments` intersection.
    pub media_attributes: Vec<MediaEditAttributes>,
    /// A new headline for a post that already has one — a long-form `Article` or
    /// a group `Page`. `None` keeps the stored title; a status with no title
    /// refuses one, because gaining a `name` would change what kind of object the
    /// audience already received.
    pub title: Option<&'a str>,
    /// An amendment to the event fields (E4); `None` leaves them untouched.
    ///
    /// This is how an event is **moved** and how it is **cancelled** — the two
    /// changes attendees are notified about. Only meaningful on a status that is
    /// already an event: an edit never promotes a Note into one, because the wire
    /// type is part of what the audience already received and silently changing it
    /// would strand every consumer that stored the post as a Note.
    pub event: Option<EventPatch>,
}

/// One `media_attributes` entry of a status edit.
#[derive(Debug)]
pub struct MediaEditAttributes {
    pub id: i64,
    /// `None` keeps the current alt text; an empty string clears it.
    pub description: Option<String>,
    pub focus: Option<(f64, f64)>,
}

/// A fully validated status edit before any durable writes or federation.
///
/// The web edit preview consumes this directly, while [`edit_status`] carries
/// it on into persistence. Keeping the preparation shared makes Preview an
/// honest dry run of Save changes instead of a second, subtly different
/// renderer.
pub(crate) struct PreparedStatusEdit {
    pub stored: Status,
    pub current_source: status::StatusSource,
    pub quote_row: Option<quote::Quote>,
    pub text: String,
    pub spoiler_text: String,
    pub sensitive: bool,
    pub language: Option<String>,
    pub content_type: PostFormat,
    /// The submitted title change. `None` means keep the stored title.
    pub title: Option<String>,
    /// The title the preview must render after applying `title`.
    pub effective_title: Option<String>,
    pub kept_media: Vec<media::Media>,
    pub current_media_ids: Vec<i64>,
    pub media_attributes: Vec<MediaEditAttributes>,
    pub composed: Composed,
    /// The exact content that Save changes would store, including a quote
    /// compatibility fallback when this is a quote post.
    pub html: String,
    pub quote_approval_policy: i32,
    pub event: Option<EventPatch>,
}

/// Resolves an edit's attachment list (`None` keeps the current set) and
/// validates it: at most 4, each the author's, and unattached or already on
/// this status. Returns `(media rows, current_media_ids)`, both id-ordered —
/// attachments render in id order, so list order is irrelevant.
async fn validated_edit_media(
    state: &AppState,
    actor: &Account,
    stored: &Status,
    requested: Option<Vec<i64>>,
    max_media_attachments: i32,
) -> Result<(Vec<media::Media>, Vec<i64>), ApiError> {
    let current_media_ids: Vec<i64> = media::for_statuses(&state.pool, &[stored.id])
        .await?
        .remove(&stored.id)
        .map(|files| files.iter().map(|m| m.id).collect())
        .unwrap_or_default();
    let mut media_ids = requested.unwrap_or_else(|| current_media_ids.clone());
    media_ids.sort_unstable();
    media_ids.dedup();
    ensure_media_count(media_ids.len(), max_media_attachments)?;
    let owned = media::find_owned_many(&state.pool, &media_ids, actor.id).await?;
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for media_id in &media_ids {
        match owned
            .iter()
            .find(|m| m.id == *media_id && m.status_id.is_none_or(|sid| sid == stored.id))
        {
            Some(item) => resolved.push(item.clone()),
            None => missing.push(*media_id),
        }
    }
    check_resolved_media(&resolved, &missing)?;
    Ok((resolved, current_media_ids))
}

/// The `media_attributes` entries that would actually change a kept
/// attachment, normalized for the update: trimmed-empty descriptions read as
/// "clear", unchanged values and unknown ids drop out. An empty result means
/// the edit's attribute part is a no-op.
fn changed_media_attributes(
    kept: &[media::Media],
    requested: &[MediaEditAttributes],
) -> Vec<MediaEditAttributes> {
    requested
        .iter()
        .filter_map(|attrs| {
            let current = kept.iter().find(|m| m.id == attrs.id)?;
            let description = attrs
                .description
                .as_deref()
                .map(|d| d.trim().to_owned())
                .filter(|d| current.description.as_deref().unwrap_or("") != d);
            let focus = attrs
                .focus
                .filter(|(x, y)| (current.focus_x, current.focus_y) != (Some(*x), Some(*y)));
            if description.is_none() && focus.is_none() {
                return None;
            }
            Some(MediaEditAttributes {
                id: attrs.id,
                description,
                focus,
            })
        })
        .collect()
}

/// The original becomes the first history entry on first edit.
async fn snapshot_original_on_first_edit(
    conn: &mut plamenu_db::PgConnection,
    actor: &Account,
    stored: &Status,
    current_text: &str,
    current_media_ids: &[i64],
) -> Result<(), ApiError> {
    if !status_edit::any_for_status(&mut *conn, stored.id).await? {
        status_edit::snapshot(
            &mut *conn,
            status_edit::NewStatusEdit {
                status_id: stored.id,
                account_id: actor.id,
                content: &stored.content,
                text: current_text,
                spoiler_text: &stored.spoiler_text,
                sensitive: stored.sensitive,
                media_ids: current_media_ids,
                created_at: stored.created_at,
            },
        )
        .await?;
    }
    Ok(())
}

/// Notifies local audiences that a status was edited: `update` to accounts
/// that boosted it, `quoted_update` to accounts whose accepted quotes embed
/// it (targeting their own quote post) — Mastodon's `notify_about_update!`.
pub async fn notify_status_edited(state: &AppState, edited: &Status) -> Result<(), ApiError> {
    let rebloggers: Vec<(i64, i64)> = status::local_rebloggers_of(&state.pool, edited.id)
        .await?
        .into_iter()
        .map(|reblogger| (reblogger, edited.id))
        .collect();
    notification::create_ungroupable_many(&state.pool, &rebloggers, edited.account_id, "update")
        .await?;
    let quoters: Vec<(i64, i64)> = quote::accepted_local_quoters_of(&state.pool, edited.id)
        .await?
        .into_iter()
        .map(|quoter| (quoter.account_id, quoter.status_id))
        .collect();
    notification::create_ungroupable_many(
        &state.pool,
        &quoters,
        edited.account_id,
        "quoted_update",
    )
    .await?;
    Ok(())
}

/// Validates and renders an edit without changing any durable state.
///
/// Callers must already have resolved the owned, non-boost status. The result
/// is safe to render as a preview; [`edit_status`] is the only caller that
/// proceeds from it to snapshots, database writes, notifications and delivery.
pub(crate) async fn prepare_status_edit(
    state: &AppState,
    actor: &Account,
    stored: Status,
    params: EditParams<'_>,
) -> Result<PreparedStatusEdit, ApiError> {
    let current_source = status::source_of(&state.pool, stored.id)
        .await?
        .unwrap_or_default();
    let quote_row = quote::for_statuses(&state.pool, &[stored.id])
        .await?
        .remove(&stored.id);
    let has_quote = quote_row.is_some();

    validate_language(params.language)?;
    let language = params
        .language
        .map(str::to_owned)
        .or_else(|| stored.language.clone());
    // Absent keeps the stored format, like the other fields (P4).
    let content_type = params
        .content_type
        .unwrap_or_else(|| PostFormat::from_media_type(&current_source.content_type));
    let limits = state.settings_cache.get(&state.pool).await?;
    let (text, spoiler_text, sensitive) = apply_spoiler_rules(
        params.text.unwrap_or(&current_source.text),
        params.spoiler_text.unwrap_or(&stored.spoiler_text),
        params.sensitive.unwrap_or(stored.sensitive),
        has_quote,
    );
    let text = text.to_owned();
    let spoiler_text = spoiler_text.to_owned();
    ensure_within_character_limit(
        &text,
        &spoiler_text,
        if stored.object_type.as_deref() == Some("Article") {
            limits.max_characters_long_form
        } else {
            limits.max_characters
        },
    )?;

    // A headline is editable on a post that has one; a typo in a title is as
    // worth fixing as one in the body, and the `Update` carries the new `name`
    // wherever the post reached. The kind itself never changes.
    let title = match params.title.map(str::trim) {
        Some(_) if stored.title.is_none() => {
            return Err(ApiError::Unprocessable(
                "Validation failed: This post has no title".into(),
            ));
        }
        Some("") => {
            return Err(ApiError::Unprocessable(
                "Validation failed: The title can't be blank".into(),
            ));
        }
        Some(new_title) => Some(new_title.to_owned()),
        None => None,
    };
    let effective_title = title.clone().or_else(|| stored.title.clone());

    let (kept_media, current_media_ids) = validated_edit_media(
        state,
        actor,
        &stored,
        params.media_ids,
        limits.max_media_attachments,
    )
    .await?;
    let media_ids: Vec<i64> = kept_media.iter().map(|m| m.id).collect();
    let media_attributes = changed_media_attributes(&kept_media, &params.media_attributes);

    let composed = compose(state, &text, content_type).await?;
    let mut html = composed.html.clone();
    if let Some(row) = &quote_row
        && let Some(quoted_url) = quote_fallback_url(state, row).await?
    {
        html = add_quote_fallback(&html, &quoted_url);
    }
    if html.is_empty() && media_ids.is_empty() {
        return Err(ApiError::Unprocessable(
            "Validation failed: Text can't be blank".into(),
        ));
    }

    // A quote-policy param applies only when present; a non-distributable
    // post is forced to `nobody` either way (`downgrade_quote_policy`).
    let quote_approval_policy = status::effective_quote_policy(
        &stored.visibility,
        params
            .quote_approval_policy
            .or(Some(stored.quote_approval_policy)),
    );

    Ok(PreparedStatusEdit {
        stored,
        current_source,
        quote_row,
        text,
        spoiler_text,
        sensitive,
        language,
        content_type,
        title,
        effective_title,
        kept_media,
        current_media_ids,
        media_attributes,
        composed,
        html,
        quote_approval_policy,
        event: params.event,
    })
}

/// Edits one's own status: merges `params` over the current values, records
/// the version history (a baseline snapshot on first edit, then the new
/// state) and federates an `Update(Note)` to the `Create` audience. A
/// no-change edit is an idempotent no-op, like Mastodon.
#[allow(clippy::too_many_lines)]
pub async fn edit_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
    params: EditParams<'_>,
) -> Result<Status, ApiError> {
    let stored = status::find_local(&state.pool, actor.id, status_id)
        .await?
        .filter(|s| s.reblog_of_id.is_none())
        .ok_or(ApiError::NotFound)?;
    let PreparedStatusEdit {
        stored,
        current_source,
        quote_row: _,
        text,
        spoiler_text,
        sensitive,
        language,
        content_type,
        title,
        effective_title: _,
        kept_media,
        current_media_ids,
        media_attributes,
        composed,
        html,
        quote_approval_policy,
        event,
    } = prepare_status_edit(state, actor, stored, params).await?;
    let media_ids: Vec<i64> = kept_media.iter().map(|m| m.id).collect();

    // The event rewrite, resolved before the no-change guard below — an edit that
    // only moves or cancels an event touches no compared field, so without this it
    // would be swallowed as "nothing changed" and never federate.
    //
    // An edit never *promotes* a Note into an event: the wire type is part of what
    // the audience already received, and changing it would strand every consumer
    // that stored the post as a Note.
    let event_rewrite = match (event, stored.object_type.as_deref()) {
        (Some(patch), Some("Event")) => {
            // An event status always has a sidecar; refusing rather than inventing
            // one keeps that invariant honest.
            let before = plamenu_db::status_event::find(&state.pool, stored.id)
                .await?
                .ok_or(ApiError::NotFound)?;
            let input = patch.apply(&before);
            let valid = validate_event(&input)?;
            let next = event_sidecar_of(stored.id, &input, &valid);
            (before != next).then(|| {
                let disrupted = before.disrupts(&next);
                (next, disrupted)
            })
        }
        _ => None,
    };

    // Nothing changed: no snapshot, no federation (Mastodon swallows these).
    if html == stored.content
        && content_type.media_type() == current_source.content_type
        && spoiler_text == stored.spoiler_text
        && sensitive == stored.sensitive
        && language == stored.language
        && media_ids == current_media_ids
        && quote_approval_policy == stored.quote_approval_policy
        && media_attributes.is_empty()
        && event_rewrite.is_none()
        && title
            .as_deref()
            .is_none_or(|t| Some(t) == stored.title.as_deref())
    {
        return Ok(stored);
    }
    let refresh_invitation = text != current_source.text
        && matches!(
            stored.object_type.as_deref(),
            None | Some("Note" | "Question")
        );
    let edited_invitation = if refresh_invitation {
        crate::webxdc::invitation_in_text(&state.pool, &text).await?
    } else {
        None
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    // Written before the `Update` is built (it reads the sidecar back), so a moved
    // or cancelled event federates with this edit rather than one delivery later.
    let event_disruption = match &event_rewrite {
        Some((next, disrupted)) => {
            plamenu_db::status_event::upsert(&mut *tx, next).await?;
            *disrupted
        }
        None => false,
    };

    snapshot_original_on_first_edit(
        &mut tx,
        actor,
        &stored,
        &current_source.text,
        &current_media_ids,
    )
    .await?;

    let edited_at = time::OffsetDateTime::now_utc();
    let updated = status::apply_local_edit(
        &mut *tx,
        stored.id,
        actor.id,
        status::LocalStatusEdit {
            content: &html,
            text: &text,
            content_type: content_type.media_type(),
            spoiler_text: &spoiler_text,
            sensitive,
            language: language.as_deref(),
            edited_at,
            quote_approval_policy,
            title: title.as_deref(),
        },
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    if refresh_invitation {
        sqlx::query("DELETE FROM webxdc_invitations WHERE status_id=$1")
            .bind(stored.id)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        if let Some((session_id, _, _)) = &edited_invitation {
            plamenu_db::webxdc::attach_invitation_tx(&mut tx, stored.id, *session_id).await?;
        }
    }
    if !media::set_attachments_conn(&mut tx, stored.id, actor.id, &media_ids).await? {
        return Err(ApiError::Unprocessable(
            "Validation failed: Media is invalid".into(),
        ));
    }
    // Applied before the `Update(Note)` below so the new alt text federates.
    if !media_attributes.is_empty() {
        let attr_ids: Vec<i64> = media_attributes.iter().map(|attrs| attrs.id).collect();
        let set_description: Vec<bool> = media_attributes
            .iter()
            .map(|attrs| attrs.description.is_some())
            .collect();
        let descriptions: Vec<Option<String>> = media_attributes
            .iter()
            .map(|attrs| {
                attrs
                    .description
                    .clone()
                    .and_then(|d| Some(d).filter(|d| !d.is_empty()))
            })
            .collect();
        let focus_x: Vec<Option<f64>> = media_attributes
            .iter()
            .map(|attrs| attrs.focus.map(|(x, _)| x))
            .collect();
        let focus_y: Vec<Option<f64>> = media_attributes
            .iter()
            .map(|attrs| attrs.focus.map(|(_, y)| y))
            .collect();
        media::update_edit_attributes_many(
            &mut *tx,
            actor.id,
            &attr_ids,
            &set_description,
            &descriptions,
            &focus_x,
            &focus_y,
        )
        .await?;
    }

    tag::detach_all(&mut *tx, stored.id).await?;
    // Group attribution (a silent mention row) isn't a text mention:
    // capture it across the rebuild below or an edit would orphan the post
    // from its group.
    let group_accounts =
        crate::groups::group_accounts_of_status_conn(state, &mut tx, &stored).await?;
    // Likewise the inherited thread audience of a closed status (silent
    // mention rows): mention rows gate direct visibility, so dropping them on
    // edit would lock the recipients out of a DM they already received — a
    // closed status' audience may grow on edit, never silently shrink.
    let audience_accounts = if matches!(stored.visibility.as_str(), "private" | "direct") {
        mention::for_statuses(&mut *tx, &[stored.id], false)
            .await?
            .remove(&stored.id)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let already_mentioned = mention::detach_all(&mut *tx, stored.id).await?;
    let (_, mut mention_inboxes, mention_notify) = persist_compose_artifacts(
        state,
        &mut tx,
        &updated,
        &composed,
        &already_mentioned,
        actor.silenced(),
    )
    .await?;
    let mut silent_rows: Vec<(i64, bool)> = group_accounts
        .iter()
        .map(|group| (group.id, true))
        .collect();
    let audience_ids: Vec<i64> = audience_accounts.iter().map(|account| account.id).collect();
    let audience_followers = if actor.silenced() {
        follow::pending_state_in_batch(&mut *tx, actor.id, &audience_ids).await?
    } else {
        std::collections::HashMap::new()
    };
    for account in &audience_accounts {
        if account.id == actor.id
            || composed.mentions.iter().any(|m| m.id == account.id)
            || silent_rows.iter().any(|(id, _)| *id == account.id)
        {
            continue;
        }
        silent_rows.push((account.id, true));
        if (!actor.silenced() || audience_followers.contains_key(&account.id))
            && !account.is_local()
            && !account.inbox_url.is_empty()
            && !mention_inboxes.contains(&account.inbox_url)
        {
            mention_inboxes.push(account.inbox_url.clone());
        }
    }
    mention::attach_many(&mut *tx, stored.id, &silent_rows).await?;

    status_edit::snapshot(
        &mut *tx,
        status_edit::NewStatusEdit {
            status_id: updated.id,
            account_id: actor.id,
            content: &html,
            text: &text,
            spoiler_text: &spoiler_text,
            sensitive,
            media_ids: &media_ids,
            created_at: edited_at,
        },
    )
    .await?;

    // An edit with changed text gets a fresh link preview (Mastodon's
    // `reset_preview_card!`); media-only edits keep the current card.
    if html != stored.content {
        let main_link = updated.external_url.as_deref();
        crate::link_preview::reset_for_edit_conn(&mut tx, stored.id, &html, main_link).await?;
    }
    // Re-scan the edited text for collection links (Mastodon's
    // `ProcessLinksService` runs on update too), reconciling the recorded set.
    crate::collections::scan_and_link_collections(state, &mut tx, &updated).await?;

    federate_note_update_conn(state, &mut tx, actor, &updated, edited_at, mention_inboxes).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if let Err(error) =
        notification::create_mentions_many(&state.pool, &mention_notify, actor.id, updated.id).await
    {
        tracing::warn!(%error, "post-commit edit mention notification failed");
    }
    if let Err(error) = notify_status_edited(state, &updated).await {
        tracing::warn!(%error, "post-commit edit notification failed");
    }
    if event_disruption
        && let Err(error) = crate::events::notify_event_changed(state, &updated).await
    {
        tracing::warn!(%error, "post-commit event notification failed");
    }
    crate::streaming::status_edited(state, updated.id).await;
    crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_UPDATED, &updated).await;
    Ok(updated)
}

/// Federates an `Update(Note)` to the same audience as the `Create`:
/// followers, mentioned remote actors, and (for replies) the parent's
/// remote author — mentioned actors only for a direct message.
pub(crate) async fn federate_note_update(
    state: &AppState,
    actor: &Account,
    updated: &Status,
    edited_at: time::OffsetDateTime,
    mention_inboxes: Vec<String>,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    federate_note_update_conn(state, &mut conn, actor, updated, edited_at, mention_inboxes).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub(crate) async fn federate_note_update_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    actor: &Account,
    updated: &Status,
    edited_at: time::OffsetDateTime,
    mut mention_inboxes: Vec<String>,
) -> Result<(), ApiError> {
    if updated.visibility == "local" {
        return Ok(());
    }
    let batch = crate::note::NoteBatch::load_conn(state, conn, &[updated]).await?;
    let note = crate::note::note_in_batch(state, updated, actor, &batch)?;
    let updated_rfc3339 = edited_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let update = activity::update_note(
        &state.config.domain,
        &actor.username,
        note,
        &updated_rfc3339,
    );
    if updated.visibility == "direct" {
        deliver_to_inboxes_conn(state, conn, actor, &update, &mention_inboxes).await?;
    } else {
        if let Some(parent_id) = updated.in_reply_to_id
            && let Some(parent) = status::find_by_id(&mut *conn, parent_id).await?
        {
            let (parent_author, inbox) = remote_author_inbox_conn(state, conn, &parent).await?;
            let address_parent = !actor.silenced()
                || follow::find(&mut *conn, parent_author.id, actor.id)
                    .await?
                    .is_some();
            if address_parent && let Some(inbox) = inbox {
                mention_inboxes.push(inbox);
            }
        }
        mention_inboxes.extend(
            crate::relays::enabled_inboxes_for_public_conn(state, conn, &updated.visibility)
                .await?,
        );
        // A remote community post's edit goes to the community inbox so the
        // origin re-announces the change; the note already carries the
        // `audience` claim. Local group re-announce is handled below.
        for (_, inbox) in crate::groups::remote_community_inboxes_conn(state, conn, updated).await?
        {
            if !mention_inboxes.contains(&inbox) {
                mention_inboxes.push(inbox);
            }
        }
        // An event's **attendees** get its updates whether or not they follow us
        // (E4). Someone who RSVP'd has organized their day around this post; a
        // move or a cancellation that only reached our followers would leave every
        // remote attendee who found the event by URL believing the old time. This
        // is the outbound half of the notification promise the inbound path makes.
        for inbox in crate::events::remote_attendee_inboxes_conn(state, conn, updated).await? {
            if !mention_inboxes.contains(&inbox) {
                mention_inboxes.push(inbox);
            }
        }
        if updated.visibility == "private" {
            fan_out_synchronized_conn(state, conn, actor, &update, &mention_inboxes).await?;
        } else {
            fan_out_conn(state, conn, actor, &update, &mention_inboxes).await?;
        }
    }
    // A local group submission's edit travels onward wrapped in the group's
    // Announce — group followers don't necessarily follow the author.
    crate::groups::reannounce_to_groups_conn(state, conn, updated, &update).await?;
    Ok(())
}

/// The remote actors mentioned in a stored status, as inbox URLs — the extra
/// `Update(Note)` audience beyond followers when re-federating a status.
pub(crate) async fn remote_mention_inboxes(
    state: &AppState,
    status_id: i64,
) -> Result<Vec<String>, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    remote_mention_inboxes_conn(state, &mut conn, status_id).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub(crate) async fn remote_mention_inboxes_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    status_id: i64,
) -> Result<Vec<String>, ApiError> {
    let by_status = mention::for_statuses(&mut *conn, &[status_id], false).await?;
    Ok(by_status
        .get(&status_id)
        .into_iter()
        .flatten()
        .filter(|a| !a.is_local())
        .map(|a| a.inbox_url.clone())
        .collect())
}

/// Sets a local status' quote-approval policy (the `interaction_policy` REST
/// endpoint). Re-federates an `Update(Note)` so remotes learn the new policy;
/// unlike an edit it bumps neither `edited_at` nor the version history, matching
/// Mastodon's `StatusUpdateDistributionWorker` (`skip_notifications`).
pub async fn set_interaction_policy(
    state: &AppState,
    actor: &Account,
    status_id: i64,
    policy: i32,
) -> Result<Status, ApiError> {
    let stored = status::find_local(&state.pool, actor.id, status_id)
        .await?
        .filter(|s| s.reblog_of_id.is_none())
        .ok_or(ApiError::NotFound)?;
    // A non-distributable post can never be quoted; force the policy to nobody,
    // like Mastodon's `downgrade_quote_policy`.
    let effective = if matches!(stored.visibility.as_str(), "public" | "unlisted") {
        policy
    } else {
        0
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let updated = status::update_quote_policy(&mut *tx, status_id, actor.id, effective)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mention_inboxes = remote_mention_inboxes_conn(state, &mut tx, updated.id).await?;
    federate_note_update_conn(
        state,
        &mut tx,
        actor,
        &updated,
        time::OffsetDateTime::now_utc(),
        mention_inboxes,
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(updated)
}

/// The quoted author revokes a previously-accepted quote of their local
/// `status_id` made by `quoting_status_id` (Mastodon's `quote_id` path param,
/// which is the *quoting* status). Marks the quote `revoked`, tells the quoter
/// to drop the embed (`Delete(QuoteAuthorization)` to a remote quoter, an
/// `Update` of the quoting post for a local one) and returns the quoting status.
pub async fn revoke_quote(
    state: &AppState,
    actor: &Account,
    status_id: i64,
    quoting_status_id: i64,
) -> Result<Status, ApiError> {
    let quoted = status::find_local(&state.pool, actor.id, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let quote = quote::for_statuses(&state.pool, &[quoting_status_id])
        .await?
        .remove(&quoting_status_id)
        .filter(|q| q.quoted_status_id == Some(quoted.id) && q.quoted_account_id == Some(actor.id))
        .ok_or(ApiError::NotFound)?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let revoked = quote::revoke(&mut *tx, quote.id, actor.id)
        .await?
        .ok_or(ApiError::NotFound)?;

    let quote_author = account::find_by_id(&mut *tx, revoked.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if quote_author.is_local() {
        // Both sides are local: re-distribute the quoting post (signed by its
        // author) so its remote followers drop the embed.
        if let Some(qsid) = revoked.status_id
            && let Some(quoting) = status::find_by_id(&mut *tx, qsid).await?
        {
            let inboxes = remote_mention_inboxes_conn(state, &mut tx, quoting.id).await?;
            federate_note_update_conn(
                state,
                &mut tx,
                &quote_author,
                &quoting,
                time::OffsetDateTime::now_utc(),
                inboxes,
            )
            .await?;
        }
    } else {
        let quoted_uri = status_uri_for_account(&state.config.domain, &quoted, actor);
        let delete = activity::delete_quote_authorization_for_actor(
            &account_uri(&state.config.domain, actor),
            revoked.id,
            &revoked.status_uri,
            &quoted_uri,
        );
        job::enqueue(&mut *tx, actor.id, &quote_author.inbox_url, &delete).await?;
    }

    let quoting = status::find_by_id(&mut *tx, quoting_status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    crate::streaming::status_edited(state, quoting.id).await;
    Ok(quoting)
}

/// One may see a blocked author's posts (when visiting deliberately), but
/// never interact with them — Mastodon's `StatusPolicy#favourite?`.
async fn forbid_interacting_with_blocked(
    state: &AppState,
    actor: &Account,
    item: &Status,
) -> Result<(), ApiError> {
    if block::exists(&state.pool, actor.id, item.account_id).await? {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    let author = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if let Some(domain) = author.domain.as_deref()
        && account_domain_block::exists(&state.pool, actor.id, domain).await?
    {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    Ok(())
}

/// Favourites a status; federates a `Like` to remote authors.
pub async fn favourite_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.reblog_of_id.is_some() || !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    forbid_interacting_with_blocked(state, actor, &item).await?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let marker = favourite::create(&mut *tx, actor.id, item.id, None).await?;
    // Already favourited: idempotent no-op, like Mastodon's FavouriteService —
    // no second notification, no second Like delivery.
    if !marker.inserted {
        return Ok(item);
    }

    let (author, remote_inbox) = remote_author_inbox_conn(state, &mut tx, &item).await?;
    let like = activity::like(
        &state.config.domain,
        &actor.username,
        marker.id,
        &status_uri_for_account(&state.config.domain, &item, &author),
    );
    if !author.is_local()
        && item.visibility != "local"
        && let Some(inbox) = remote_inbox
    {
        enqueue_with_mode_conn(
            state,
            &mut tx,
            actor,
            &inbox,
            &like,
            DeliveryMode::Cancellable,
        )
        .await?;
    }
    // Group votes: on a group post the favourite is an upvote — it displaces
    // the actor's downvote (mutual exclusion, Lemmy's model) and travels to
    // the post's groups. An outcast's favourite stays a plain favourite; the
    // groups never relay it.
    if item.visibility == "public"
        && crate::groups::is_group_post_conn(state, &mut tx, item.id).await?
        && crate::groups::may_vote_conn(state, &mut tx, actor.id, item.id).await?
    {
        retract_dislike_conn(state, &mut tx, actor, &item).await?;
        federate_vote_to_groups_conn(
            state,
            &mut tx,
            actor,
            &item,
            &like,
            DeliveryMode::Cancellable,
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if author.is_local() {
        notify_after_commit(state, author.id, actor.id, "favourite", Some(item.id)).await;
    }
    Ok(item)
}

/// Removes a favourite; federates `Undo(Like)` to remote authors.
pub async fn unfavourite_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let result = unfavourite_status_conn(state, &mut tx, actor, status_id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(result)
}

pub async fn unfavourite_status_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&mut *conn, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let Some(marker) = favourite::delete(&mut *conn, actor.id, item.id).await? else {
        return Ok(item); // not favourited: idempotent no-op, like Mastodon
    };
    // Un-favouriting one's own status may lift its cleanup exemption.
    if item.account_id == actor.id {
        statuses_cleanup::rollback_last_inspected(
            &mut *conn,
            actor.id,
            item.id,
            statuses_cleanup::KeptReason::SelfFav,
        )
        .await?;
    }
    let (author, remote_inbox) = remote_author_inbox_conn(state, conn, &item).await?;
    // A retracted favourite takes its notification with it (Mastodon
    // destroys the notification with the Favourite row).
    if author.is_local() {
        notification::clear_kind_for_status(&mut *conn, author.id, actor.id, "favourite", item.id)
            .await?;
    }
    let like = activity::like(
        &state.config.domain,
        &actor.username,
        marker,
        &status_uri_for_account(&state.config.domain, &item, &author),
    );
    let undo = activity::undo(&state.config.domain, &actor.username, like);
    if item.visibility != "local"
        && let Some(inbox) = remote_inbox
    {
        enqueue_with_mode_conn(state, conn, actor, &inbox, &undo, DeliveryMode::Retraction).await?;
    }
    // A retracted upvote on a group post travels to the groups too.
    if item.visibility == "public"
        && crate::groups::is_group_post_conn(state, conn, item.id).await?
    {
        federate_vote_to_groups_conn(state, conn, actor, &item, &undo, DeliveryMode::Retraction)
            .await?;
    }
    Ok(item)
}

/// Downvotes a group post: stores the `Dislike`, displaces the
/// actor's upvote (mutually exclusive, Lemmy's model) and federates the
/// `Dislike` to the author and the post's groups. Only group posts take
/// downvotes; a ban (outcast) forbids voting outright.
pub async fn downvote_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.reblog_of_id.is_some() || !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    forbid_interacting_with_blocked(state, actor, &item).await?;
    if !crate::groups::is_group_post(state, item.id).await? {
        return Err(ApiError::Unprocessable(
            "Validation failed: Only group posts can be downvoted".into(),
        ));
    }
    if !crate::groups::may_vote(state, actor.id, item.id).await? {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let marker = dislike::create(&mut *tx, actor.id, item.id, None).await?;
    // Already downvoted: idempotent no-op, like the favourite verb.
    if !marker.inserted {
        return Ok(item);
    }
    // The downvote displaces the actor's upvote, retracting it on the wire
    // (Lemmy clients send the Undo(Like) alongside the Dislike).
    unfavourite_status_conn(state, &mut tx, actor, status_id).await?;
    let (author, remote_inbox) = remote_author_inbox_conn(state, &mut tx, &item).await?;
    let dislike_activity = activity::dislike(
        &state.config.domain,
        &actor.username,
        marker.id,
        &status_uri_for_account(&state.config.domain, &item, &author),
    );
    if item.visibility != "local"
        && let Some(inbox) = remote_inbox
    {
        enqueue_with_mode_conn(
            state,
            &mut tx,
            actor,
            &inbox,
            &dislike_activity,
            DeliveryMode::Cancellable,
        )
        .await?;
    }
    if item.visibility == "public" {
        federate_vote_to_groups_conn(
            state,
            &mut tx,
            actor,
            &item,
            &dislike_activity,
            DeliveryMode::Cancellable,
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(item)
}

/// Removes a downvote; federates `Undo(Dislike)` to the author and the
/// post's groups. Idempotent, like the other status verbs.
pub async fn undownvote_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    retract_dislike(state, actor, &item).await?;
    Ok(item)
}

/// Removes the actor's downvote on `item` if one exists, federating the
/// `Undo(Dislike)` to the author and the post's groups. The shared tail of
/// the undownvote verb and the upvote's mutual exclusion.
async fn retract_dislike(state: &AppState, actor: &Account, item: &Status) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    retract_dislike_conn(state, &mut tx, actor, item).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

async fn retract_dislike_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    actor: &Account,
    item: &Status,
) -> Result<(), ApiError> {
    let Some(marker) = dislike::delete(&mut *conn, actor.id, item.id).await? else {
        return Ok(());
    };
    let (author, remote_inbox) = remote_author_inbox_conn(state, conn, item).await?;
    let dislike_activity = activity::dislike(
        &state.config.domain,
        &actor.username,
        marker,
        &status_uri_for_account(&state.config.domain, item, &author),
    );
    let undo = activity::undo(&state.config.domain, &actor.username, dislike_activity);
    if item.visibility != "local"
        && let Some(inbox) = remote_inbox
    {
        enqueue_with_mode_conn(state, conn, actor, &inbox, &undo, DeliveryMode::Retraction).await?;
    }
    if item.visibility == "public" {
        federate_vote_to_groups_conn(state, conn, actor, item, &undo, DeliveryMode::Retraction)
            .await?;
    }
    Ok(())
}

/// Delivers a local voter's vote activity (`Like`/`Dislike`/`Undo`) to a
/// group post's communities: local groups announce the wrapped vote
/// to their followers; remote groups get their own copy — stamped with the
/// group as `audience`, the FEP-1b12 community claim — delivered to their
/// inbox, exactly as Lemmy clients address the community.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn federate_vote_to_groups_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    actor: &Account,
    item: &Status,
    vote: &Value,
    mode: DeliveryMode,
) -> Result<(), ApiError> {
    crate::groups::announce_vote_conn(state, conn, item, vote).await?;
    let group_ids = group::boosting_group_ids(&mut *conn, item.id).await?;
    for group_account in account::find_by_ids(&mut *conn, &group_ids).await? {
        if group_account.is_local() {
            continue; // announced above
        }
        let Some(group_uri) = group_account.uri.clone() else {
            continue;
        };
        let mut copy = vote.clone();
        copy["audience"] = Value::String(group_uri);
        enqueue_with_mode_conn(state, conn, actor, &group_account.inbox_url, &copy, mode).await?;
    }
    Ok(())
}

/// A normalized Pleroma emoji reaction. `name` is the Unicode emoji or local
/// shortcode without colons; `custom_emoji_url` is present only for local
/// custom emoji that should federate with an `Emoji` tag.
pub(crate) struct ReactionEmoji {
    pub name: String,
    pub custom_emoji_url: Option<String>,
    /// The local custom-emoji row id, when `name` is a local custom emoji
    /// (announcement reactions store this FK; status reactions ignore it).
    pub custom_emoji_id: Option<i64>,
    pub custom_emoji_origin_id: Option<i64>,
}

/// Maximum Unicode reaction length. This mirrors the inbound cap: long enough
/// for ZWJ emoji and flag pairs, short enough to reject arbitrary prose.
const MAX_REACTION_CHARS: usize = 16;

pub(crate) async fn normalize_local_reaction(
    state: &AppState,
    account_id: i64,
    raw: &str,
) -> Result<ReactionEmoji, ApiError> {
    let emoji = raw.trim();
    if emoji.is_empty() {
        return Err(ApiError::Unprocessable("Reaction can't be blank".into()));
    }

    let shortcode = emoji
        .strip_prefix(':')
        .and_then(|rest| rest.strip_suffix(':'))
        .unwrap_or(emoji);
    if plamenu_ap::emoji::is_valid_shortcode(shortcode, plamenu_ap::emoji::MAX_SHORTCODE_LEN) {
        let codes = vec![shortcode.to_owned()];
        if let Some(found) = custom_emoji::lookup_local_for_account(&state.pool, account_id, &codes)
            .await?
            .into_iter()
            .next()
        {
            let managed = custom_emoji::find_managed_by_id(&state.pool, found.id)
                .await?
                .ok_or(ApiError::NotFound)?;
            let rendered = crate::emoji::custom_emoji_json(&state.config.domain, &found, false);
            return Ok(ReactionEmoji {
                name: found.shortcode,
                custom_emoji_url: rendered
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                custom_emoji_id: Some(found.id),
                custom_emoji_origin_id: Some(managed.origin_id),
            });
        }
        if emoji.starts_with(':') {
            return Err(ApiError::Unprocessable("Unknown custom emoji".into()));
        }
    }

    if emoji.chars().count() <= MAX_REACTION_CHARS && !emoji.is_ascii() {
        Ok(ReactionEmoji {
            name: emoji.to_owned(),
            custom_emoji_url: None,
            custom_emoji_id: None,
            custom_emoji_origin_id: None,
        })
    } else {
        Err(ApiError::Unprocessable(
            "Reaction is not a valid emoji".into(),
        ))
    }
}

/// Resolves what a status reaction names: a Unicode emoji or local shortcode
/// through [`normalize_local_reaction`], or — Pleroma-style — a qualified
/// `shortcode@host` that joins (+1s) a remote custom-emoji reaction already
/// on the status, reusing its federated image. Like Pleroma, a remote emoji
/// can only join an existing reaction, never start one.
async fn resolve_status_reaction(
    state: &AppState,
    account_id: i64,
    status_id: i64,
    raw: &str,
) -> Result<ReactionEmoji, ApiError> {
    let Some(qualified) = qualified_reaction_name(raw) else {
        return normalize_local_reaction(state, account_id, raw).await;
    };
    let groups = reaction::for_statuses(&state.pool, &[status_id])
        .await?
        .remove(&status_id)
        .unwrap_or_default();
    groups
        .into_iter()
        .find(|group| displayed_reaction_name(&state.config.domain, group) == qualified)
        .map(|group| ReactionEmoji {
            name: group.name,
            custom_emoji_url: group.custom_emoji_url,
            custom_emoji_id: group.custom_emoji_id,
            custom_emoji_origin_id: group.custom_emoji_origin_id,
        })
        .ok_or_else(|| {
            ApiError::Unprocessable("Remote emoji is not among this post's reactions".into())
        })
}

/// The `shortcode@host` form of a reaction name (colons stripped), when the
/// name addresses a remote custom emoji.
fn qualified_reaction_name(raw: &str) -> Option<&str> {
    let emoji = raw.trim();
    let bare = emoji
        .strip_prefix(':')
        .and_then(|rest| rest.strip_suffix(':'))
        .unwrap_or(emoji);
    bare.contains('@').then_some(bare)
}

fn reaction_display(emoji: &ReactionEmoji) -> String {
    if emoji.custom_emoji_url.is_some() {
        format!(":{}:", emoji.name)
    } else {
        emoji.name.clone()
    }
}

fn reaction_activity(
    state: &AppState,
    actor: &Account,
    target: &Status,
    target_author: &Account,
    marker: i64,
    emoji: &ReactionEmoji,
) -> Value {
    activity::emoji_react(&activity::EmojiReactParams {
        domain: &state.config.domain,
        username: &actor.username,
        marker,
        object_uri: &status_uri_for_account(&state.config.domain, target, target_author),
        object_author_uri: &account_uri(&state.config.domain, target_author),
        public: matches!(target.visibility.as_str(), "public" | "unlisted"),
        name: &emoji.name,
        custom_emoji_url: emoji.custom_emoji_url.as_deref(),
    })
}

/// Connection-scoped delivery for an enclosing mutation transaction.
async fn deliver_reaction_activity_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    actor: &Account,
    target: &Status,
    target_author: &Account,
    activity: &Value,
    mode: DeliveryMode,
) -> Result<(), ApiError> {
    if target.visibility == "local" {
        return Ok(());
    }
    let remote_author_inbox = (!target_author.is_local()).then(|| target_author.inbox_url.clone());
    if matches!(target.visibility.as_str(), "public" | "unlisted") {
        fan_out_inner_conn(
            state,
            conn,
            actor,
            activity,
            remote_author_inbox.as_slice(),
            mode,
        )
        .await?;
    } else if let Some(inbox) = remote_author_inbox {
        enqueue_with_mode_conn(state, conn, actor, &inbox, activity, mode).await?;
    }
    Ok(())
}

/// Reacts to a status with a Pleroma/litepub emoji reaction; federates
/// `EmojiReact` to the remote author and, for distributable targets, to the
/// reacting account's followers.
pub async fn react_with_emoji(
    state: &AppState,
    actor: &Account,
    status_id: i64,
    raw_emoji: &str,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.reblog_of_id.is_some() || !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    forbid_interacting_with_blocked(state, actor, &item).await?;
    let emoji = resolve_status_reaction(state, actor.id, item.id, raw_emoji).await?;
    let new_reaction = reaction::NewReaction {
        account_id: actor.id,
        status_id: item.id,
        name: &emoji.name,
        custom_emoji_url: emoji.custom_emoji_url.as_deref(),
        uri: None,
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let marker = match (emoji.custom_emoji_id, emoji.custom_emoji_origin_id) {
        (Some(emoji_id), Some(origin_id)) => {
            reaction::create_custom(&mut *tx, new_reaction, emoji_id, origin_id).await?
        }
        _ => reaction::create(&mut *tx, new_reaction).await?,
    };
    // Already reacted with this emoji: idempotent no-op — no second
    // notification, delivery or re-push.
    if !marker.inserted {
        return Ok(item);
    }
    if matches!(item.visibility.as_str(), "public" | "unlisted" | "local")
        && let Some(origin_id) = emoji.custom_emoji_origin_id
    {
        custom_emoji::record_reaction_usage(&mut *tx, actor.id, origin_id).await?;
    }

    let target_author = account::find_by_id(&mut *tx, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let activity = reaction_activity(state, actor, &item, &target_author, marker.id, &emoji);
    deliver_reaction_activity_conn(
        state,
        &mut tx,
        actor,
        &item,
        &target_author,
        &activity,
        DeliveryMode::Cancellable,
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if target_author.is_local() {
        notification::create_reaction(
            &state.pool,
            target_author.id,
            actor.id,
            item.id,
            &reaction_display(&emoji),
        )
        .await
        .unwrap_or_else(|error| tracing::warn!(%error, "post-commit reaction notification failed"));
    }
    crate::streaming::status_edited(state, item.id).await;
    Ok(item)
}

/// Removes a Pleroma/litepub emoji reaction; federates `Undo(EmojiReact)` when
/// the local row existed. Missing reactions are an idempotent no-op.
pub async fn unreact_with_emoji(
    state: &AppState,
    actor: &Account,
    status_id: i64,
    raw_emoji: &str,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // A qualified `shortcode@host` identifies the stable remote-emoji origin,
    // not just its bare name. Fall back to the legacy name-only row so joins
    // written before origin-aware reactions were introduced remain removable.
    let (name, origin_id, legacy_fallback) =
        if let Some(qualified) = qualified_reaction_name(raw_emoji) {
            let origin_id = reaction::for_statuses(&state.pool, &[item.id])
                .await?
                .remove(&item.id)
                .unwrap_or_default()
                .into_iter()
                .find(|group| displayed_reaction_name(&state.config.domain, group) == qualified)
                .and_then(|group| group.custom_emoji_origin_id);
            (
                qualified.split('@').next().unwrap_or(qualified).to_owned(),
                origin_id,
                true,
            )
        } else {
            let normalized = normalize_local_reaction(state, actor.id, raw_emoji).await?;
            (normalized.name, normalized.custom_emoji_origin_id, false)
        };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let mut removed = if let Some(origin_id) = origin_id {
        reaction::delete_returning_by_origin(&mut *tx, actor.id, item.id, origin_id).await?
    } else {
        None
    };
    if removed.is_none() && (origin_id.is_none() || legacy_fallback) {
        removed = reaction::delete_returning(&mut *tx, actor.id, item.id, &name).await?;
    }
    let Some(removed) = removed else {
        return Ok(item);
    };
    let removed_emoji = ReactionEmoji {
        name: removed.name,
        custom_emoji_url: removed.custom_emoji_url,
        custom_emoji_id: None,
        custom_emoji_origin_id: None,
    };
    let target_author = account::find_by_id(&mut *tx, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let react = reaction_activity(
        state,
        actor,
        &item,
        &target_author,
        removed.row_id,
        &removed_emoji,
    );
    let undo = activity::undo(&state.config.domain, &actor.username, react);
    deliver_reaction_activity_conn(
        state,
        &mut tx,
        actor,
        &item,
        &target_author,
        &undo,
        DeliveryMode::Retraction,
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if target_author.is_local() {
        notification::clear_reaction(
            &state.pool,
            target_author.id,
            actor.id,
            item.id,
            &reaction_display(&removed_emoji),
        )
        .await
        .unwrap_or_else(
            |error| tracing::warn!(%error, "post-commit reaction notification cleanup failed"),
        );
    }
    crate::streaming::status_edited(state, item.id).await;
    Ok(item)
}

/// Bookmarks a status — purely local, never federated, no notification.
pub async fn bookmark_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.reblog_of_id.is_some() || !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    bookmark::create(&state.pool, actor.id, item.id).await?;
    Ok(item)
}

/// Removes a bookmark; a missing one is an idempotent no-op (but the status
/// must still be viewable, like Mastodon's fallback `authorize show?`).
pub async fn unbookmark_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let removed = bookmark::delete(&state.pool, actor.id, item.id).await?;
    if removed.is_none() && !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    // Un-bookmarking one's own status may lift its cleanup exemption.
    if removed.is_some() && item.account_id == actor.id {
        statuses_cleanup::rollback_last_inspected(
            &state.pool,
            actor.id,
            item.id,
            statuses_cleanup::KeptReason::SelfBookmark,
        )
        .await?;
    }
    Ok(item)
}

/// Mutes the status' conversation, so the whole thread stops notifying and
/// renders `muted`. Mastodon keys this on the status' conversation; we ensure
/// one exists (creating it, inheriting the reply parent's) so any visible
/// thread can be muted, not only DMs.
pub async fn mute_conversation(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    let conversation_id = conversation::ensure_for_status(
        &state.pool,
        &conversation::EnsureConversation {
            status_id: item.id,
            account_id: item.account_id,
            in_reply_to_id: item.in_reply_to_id,
            is_reply: item.in_reply_to_id.is_some(),
            refs: conversation::ContextRefs::default(),
        },
    )
    .await?;
    conversation::mute(&state.pool, actor.id, conversation_id).await?;
    Ok(item)
}

/// Unmutes the status' conversation; a status without one is an idempotent
/// no-op.
pub async fn unmute_conversation(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    if let Some(conversation_id) = conversation::of_status(&state.pool, item.id).await? {
        conversation::unmute(&state.pool, actor.id, conversation_id).await?;
    }
    Ok(item)
}

/// Mastodon's pin limit for local accounts (`StatusPinValidator`). Advertised
/// in `configuration.accounts.max_pinned_statuses`.
pub(crate) const PIN_LIMIT: i64 = 5;

/// Pins one's own status: validates like Mastodon's `StatusPinValidator`
/// (exact error wording, all failures collected) and federates an `Add`
/// targeting the featured collection to followers.
pub async fn pin_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    let mut errors: Vec<&str> = Vec::new();
    if item.reblog_of_id.is_some() {
        errors.push("A boost cannot be pinned");
    }
    if item.account_id != actor.id {
        errors.push("Someone else's post cannot be pinned");
    }
    if item.visibility == "direct" {
        errors.push("Posts that are only visible to mentioned users cannot be pinned");
    }
    if item.visibility == "local" {
        errors.push("Local-only posts cannot be pinned");
    }
    if pin::count_by_account(&state.pool, actor.id).await? >= PIN_LIMIT {
        errors.push("You have already pinned the maximum number of posts");
    }
    if !errors.is_empty() {
        return Err(ApiError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if pin::create(&mut *tx, actor.id, item.id).await?.is_none() {
        // Already pinned: Mastodon's RecordNotUnique rescue.
        return Err(ApiError::Unprocessable("Duplicate record".into()));
    }
    let add = activity::add_to_featured(
        &state.config.domain,
        &actor.username,
        &status_uri_for_account(&state.config.domain, &item, actor),
    );
    fan_out_conn(state, &mut tx, actor, &add, &[]).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(item)
}

/// Unpins a status; federates a `Remove` when a pin actually existed. A
/// missing pin is an idempotent no-op (but the status must be viewable).
pub async fn unpin_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if pin::delete(&mut *tx, actor.id, item.id).await?.is_some() {
        // Un-pinning lifts the status' cleanup exemption (pins are always
        // the actor's own statuses).
        statuses_cleanup::rollback_last_inspected(
            &mut *tx,
            actor.id,
            item.id,
            statuses_cleanup::KeptReason::Pin,
        )
        .await?;
        let remove = activity::remove_from_featured(
            &state.config.domain,
            &actor.username,
            &status_uri_for_account(&state.config.domain, &item, actor),
        );
        fan_out_conn(state, &mut tx, actor, &remove, &[]).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(item)
}

/// Features a hashtag on one's profile and federates `Add(Hashtag)` to
/// followers. Returns the featured-tag row id. Idempotent.
pub async fn feature_tag(
    state: &AppState,
    actor: &Account,
    tag_id: i64,
    name: &str,
) -> Result<i64, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let row_id = featured_tag::feature(&mut *tx, actor.id, tag_id).await?;
    let add = activity::add_hashtag_to_featured(&state.config.domain, &actor.username, name);
    fan_out_conn(state, &mut tx, actor, &add, &[]).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(row_id)
}

/// Unfeatures a hashtag and queues its Remove in the same transaction.
pub async fn unfeature_tag(
    state: &AppState,
    actor: &Account,
    tag_id: i64,
    name: &str,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if featured_tag::unfeature(&mut *tx, actor.id, tag_id).await? {
        let remove =
            activity::remove_hashtag_from_featured(&state.config.domain, &actor.username, name);
        fan_out_conn(state, &mut tx, actor, &remove, &[]).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Removes a featured-tag row by its REST id, returning whether it existed.
pub async fn unfeature_tag_by_id(
    state: &AppState,
    actor: &Account,
    row_id: i64,
) -> Result<bool, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let Some(tag_id) = featured_tag::delete_by_id(&mut *tx, actor.id, row_id).await? else {
        return Ok(false);
    };
    let name = tag::name_of(&mut *tx, tag_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let remove =
        activity::remove_hashtag_from_featured(&state.config.domain, &actor.username, &name);
    fan_out_conn(state, &mut tx, actor, &remove, &[]).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(true)
}

/// Boosts a status; federates an `Announce` to followers and the author.
/// Adds a boosted post's author to an `Announce`'s `cc` (Mastodon's addressing).
/// Idempotent; a no-op when the author URI can't be determined.
fn address_boosted_author(announce: &mut Value, domain: &str, author: &Account) {
    let author_uri = if author.is_local() {
        LocalUserUrls::for_account(domain, &author.username, author.uri.as_deref()).id
    } else {
        author.uri.clone().unwrap_or_default()
    };
    if author_uri.is_empty() {
        return;
    }
    if let Some(cc) = announce.get_mut("cc").and_then(Value::as_array_mut)
        && !cc.iter().any(|v| v.as_str() == Some(author_uri.as_str()))
    {
        cc.push(Value::String(author_uri));
    }
}

pub async fn reblog_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.reblog_of_id.is_some() || !can_view(&state.pool, &item, Some(actor.id)).await? {
        return Err(ApiError::NotFound);
    }
    // Followers-only posts and direct messages cannot be boosted
    // (Mastodon: 422).
    if matches!(item.visibility.as_str(), "private" | "direct" | "local") {
        return Err(ApiError::Unprocessable("This action is not allowed".into()));
    }
    forbid_interacting_with_blocked(state, actor, &item).await?;
    // Already boosted: return the existing boost without re-notifying,
    // re-announcing or re-streaming, like Mastodon's ReblogService.
    if let Some(existing) = status::find_reblog_by(&state.pool, actor.id, item.id).await? {
        return Ok(existing);
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let boost = status::create_local_reblog_conn(&mut tx, actor.id, item.id).await?;

    let (author, remote_inbox) = remote_author_inbox_conn(state, &mut tx, &item).await?;
    let mut announce = activity::announce(
        &state.config.domain,
        &actor.username,
        boost.id,
        &status_uri_for_account(&state.config.domain, &item, &author),
        &published_of(&boost)?,
    );
    // Address the boosted post's author, as Mastodon does. A strict receiver
    // (upstream Pleroma's `recipient_in_message`) rejects an `Announce` POSTed
    // to the author's own inbox with 400 unless the author is an addressee (or
    // follows the booster), silently dropping the boost so the author never
    // sees the reblog count/notification. `cc` is where Mastodon places them.
    address_boosted_author(&mut announce, &state.config.domain, &author);
    let mut extras: Vec<String> = remote_inbox.into_iter().collect();
    extras.extend(
        crate::relays::enabled_inboxes_for_public_conn(state, &mut tx, &boost.visibility).await?,
    );
    fan_out_inner_conn(
        state,
        &mut tx,
        actor,
        &announce,
        &extras,
        DeliveryMode::Cancellable,
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if author.is_local() {
        notify_after_commit(state, author.id, actor.id, "reblog", Some(item.id)).await;
    }
    crate::streaming::status_created(state, boost.id).await;
    // Mastodon's `status.created` fires for every local status, boosts too.
    crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_CREATED, &boost).await;
    Ok(boost)
}

/// Removes a boost; federates `Undo(Announce)`.
pub async fn unreblog_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Status, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let Some(boost) = status::find_reblog_by(&state.pool, actor.id, item.id).await? else {
        return Ok(item); // not boosted: idempotent no-op
    };
    let delete_event = crate::streaming::prepare_delete_event(state, &boost).await?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    status::delete_local_conn(&mut tx, boost.id, actor.id).await?;

    let (author, remote_inbox) = remote_author_inbox_conn(state, &mut tx, &item).await?;
    let announce = activity::announce(
        &state.config.domain,
        &actor.username,
        boost.id,
        &status_uri_for_account(&state.config.domain, &item, &author),
        &published_of(&boost)?,
    );
    let undo = activity::undo(&state.config.domain, &actor.username, announce);
    let mut extras: Vec<String> = remote_inbox.into_iter().collect();
    extras.extend(
        crate::relays::enabled_inboxes_for_public_conn(state, &mut tx, &boost.visibility).await?,
    );
    fan_out_inner_conn(
        state,
        &mut tx,
        actor,
        &undo,
        &extras,
        DeliveryMode::Retraction,
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    crate::streaming::publish(state, &delete_event).await;
    // A retracted boost takes its notification with it (in Mastodon the
    // notification's activity is the boost row, so it cascades).
    if author.is_local() {
        notification::clear_kind_for_status(&state.pool, author.id, actor.id, "reblog", item.id)
            .await
            .unwrap_or_else(
                |error| tracing::warn!(%error, "post-commit unboost notification cleanup failed"),
            );
    }
    Ok(item)
}

/// A direct message's audience, captured before deletion cascades the
/// mention rows away.
struct DirectAudience {
    conversation_id: Option<i64>,
    inboxes: Vec<String>,
}

/// Deletes one's own status; federates `Delete` (or `Undo(Announce)` for a
/// boost) to followers — for a direct message, to the mentioned actors
/// instead. Returns the deleted row.
/// How [`delete_status`] removes the row. A user/federated delete leaves a
/// `Stub` (GtS-style) so the reply tree survives a deleted middle post;
/// age-based retention `Wipe`s outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    Stub,
    Wipe,
}

#[allow(
    clippy::too_many_lines,
    reason = "status deletion keeps authorization, cleanup, federation, and notifications in one auditable flow"
)]
pub async fn delete_status(
    state: &AppState,
    actor: &Account,
    status_id: i64,
    mode: DeleteMode,
) -> Result<Status, ApiError> {
    let existing = status::find_local(&state.pool, actor.id, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // The streaming event's routing data (tags, mentioned locals) must be
    // captured before the delete cascades those rows away.
    let delete_event = crate::streaming::prepare_delete_event(state, &existing).await?;
    // Likewise a group submission's retraction data: the attributing
    // mention rows and the group's boost row cascade with the status.
    let group_accounts = crate::groups::group_accounts_of_status(state, &existing).await?;
    let group_boosts = crate::groups::capture_boosts(state, &existing, actor).await?;
    // Remote communities the post belongs to: captured before the delete
    // cascade removes their boost rows, so the `Delete` can still reach them.
    let remote_communities = crate::groups::remote_community_inboxes(state, &existing).await?;
    // A DM's audience (and its conversation) must be captured before the
    // delete cascades the mention rows away.
    let direct_audience = if existing.visibility == "direct" {
        let mentioned = mention::for_statuses(&state.pool, &[existing.id], false)
            .await?
            .remove(&existing.id)
            .unwrap_or_default();
        Some(DirectAudience {
            conversation_id: conversation::of_status(&state.pool, existing.id).await?,
            inboxes: mentioned
                .iter()
                .filter(|m| !m.is_local())
                .map(|m| m.inbox_url.clone())
                .collect(),
        })
    } else {
        None
    };
    // A boost is never stubbed: it roots no thread, and deleting one is an
    // unboost that must remove the row outright (else `reblogged_of` and the
    // reblog counters would see a ghost). Only originals leave a stub.
    let mode = if existing.reblog_of_id.is_some() {
        DeleteMode::Wipe
    } else {
        mode
    };
    let mut original_extras = remote_mention_inboxes(state, existing.id).await?;
    original_extras.extend(crate::events::remote_attendee_inboxes(state, &existing).await?);
    original_extras
        .extend(crate::relays::enabled_inboxes_for_public(state, &existing.visibility).await?);
    if let Some(parent_id) = existing.in_reply_to_id
        && let Some(parent) = status::find_by_id(&state.pool, parent_id).await?
    {
        let (parent_author, inbox) = remote_author_inbox(state, &parent).await?;
        if (!actor.silenced()
            || follow::find(&state.pool, parent_author.id, actor.id)
                .await?
                .is_some())
            && let Some(inbox) = inbox
        {
            original_extras.push(inbox);
        }
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let deleted = match mode {
        DeleteMode::Stub => status::stub_local_conn(&mut tx, status_id, actor.id).await?,
        DeleteMode::Wipe => status::delete_local_conn(&mut tx, status_id, actor.id).await?,
    }
    .ok_or(ApiError::NotFound)?;
    // Record a short-lived tombstone for federating originals so the
    // Note URI answers `410 Gone` while the `Delete` propagates. Boosts (an
    // Undo(Announce), not a Note) and local-only posts (never dereferenced by
    // remotes) don't need one.
    if deleted.reblog_of_id.is_none() && deleted.visibility != "local" {
        let uri = status_uri_for_account(&state.config.domain, &deleted, actor);
        plamenu_db::status_tombstone::record(&mut *tx, deleted.id, actor.id, &uri).await?;
    }
    if deleted.visibility == "local" {
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        crate::streaming::publish(state, &delete_event).await;
        return Ok(deleted);
    }
    if let Some(audience) = &direct_audience {
        if let Some(conversation_id) = audience.conversation_id {
            conversation::remove_status_conn(&mut tx, conversation_id, deleted.id).await?;
        }
        let uri = status_uri_for_account(&state.config.domain, &deleted, actor);
        let delete = activity::delete_note(&state.config.domain, &actor.username, &uri);
        deliver_to_inboxes_conn(state, &mut tx, actor, &delete, &audience.inboxes).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        crate::streaming::publish(state, &delete_event).await;
        return Ok(deleted);
    }
    if let Some(target_id) = deleted.reblog_of_id {
        // Deleting a boost == unboosting.
        if let Some(target) = status::find_by_id(&mut *tx, target_id).await? {
            let (author, remote_inbox) = remote_author_inbox_conn(state, &mut tx, &target).await?;
            if author.is_local() {
                notification::clear_kind_for_status(
                    &mut *tx, author.id, actor.id, "reblog", target.id,
                )
                .await?;
            }
            let announce = activity::announce(
                &state.config.domain,
                &actor.username,
                deleted.id,
                &status_uri_for_account(&state.config.domain, &target, &author),
                &published_of(&deleted)?,
            );
            let undo = activity::undo(&state.config.domain, &actor.username, announce);
            let mut extras: Vec<String> = remote_inbox.into_iter().collect();
            extras.extend(
                crate::relays::enabled_inboxes_for_public_conn(state, &mut tx, &deleted.visibility)
                    .await?,
            );
            fan_out_inner_conn(
                state,
                &mut tx,
                actor,
                &undo,
                &extras,
                DeliveryMode::Retraction,
            )
            .await?;
        }
    } else {
        let uri = status_uri_for_account(&state.config.domain, &deleted, actor);
        let delete = activity::delete_note(&state.config.domain, &actor.username, &uri);
        fan_out_conn(state, &mut tx, actor, &delete, &original_extras).await?;
        // A local group submission's Delete travels onward wrapped in the
        // group's Announce, and the compat bare Announce is undone.
        if deleted.visibility == "public" {
            for group_account in &group_accounts {
                crate::groups::announce_wrapped_conn(state, &mut tx, group_account, &delete)
                    .await?;
            }
            for captured in &group_boosts {
                crate::groups::retract_boost_conn(state, &mut tx, captured).await?;
            }
            // A remote community post's Delete goes to the community inbox,
            // stamped with the community claim, so the origin removes and
            // un-announces it.
            for (uri, inbox) in &remote_communities {
                let mut copy = delete.clone();
                copy["audience"] = Value::String(uri.clone());
                enqueue_with_mode_conn(
                    state,
                    &mut tx,
                    actor,
                    inbox,
                    &copy,
                    DeliveryMode::Retraction,
                )
                .await?;
            }
        }
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    crate::streaming::publish(state, &delete_event).await;
    Ok(deleted)
}

/// Follows `target` on behalf of `actor`: local targets are followed
/// instantly (with a `follow` notification), remote targets get a federated
/// `Follow` and stay pending until the `Accept` arrives. An existing
/// follow/request is an idempotent no-op; a block in either direction is a
/// 403, like Mastodon's `FollowService`.
/// Fires a local notification as a post-commit best effort: the durable state
/// it accompanies has already committed, so a transient failure here is logged
/// rather than propagated as an error that would wrongly imply the mutation
/// itself failed. Notification creation fans into the whole notification-policy
/// subtree (mute checks, filtering, grouping), so it stays out of the mutation
/// transaction; a lost local notification is a regenerable convenience, unlike
/// the federation delivery that commits atomically with its domain row (QC
/// audit #18, mirroring the post-commit signup notifications of #41).
async fn notify_after_commit(
    state: &AppState,
    account_id: i64,
    from_account_id: i64,
    kind: &str,
    status_id: Option<i64>,
) {
    if let Err(err) =
        notification::create(&state.pool, account_id, from_account_id, kind, status_id).await
    {
        tracing::warn!(
            error = %err,
            kind,
            account_id,
            from_account_id,
            "post-commit notification creation failed"
        );
    }
}

pub async fn follow_account(
    state: &AppState,
    actor: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    if !crate::instance_policy::public_account_visible(&state.pool, &state.config.domain, target)
        .await?
    {
        return Err(ApiError::NotFound);
    }
    if actor.id == target.id {
        // Mastodon's exact 403 wording.
        return Err(ApiError::Forbidden(
            "Following your own account is not allowed".into(),
        ));
    }
    if target.suspended() {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    if block::exists(&state.pool, actor.id, target.id).await?
        || block::exists(&state.pool, target.id, actor.id).await?
    {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    if !target.is_portable_on(&state.config.domain)
        && let Some(domain) = target.domain.as_deref()
        && account_domain_block::exists(&state.pool, actor.id, domain).await?
    {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    if account_domain_block::blocks_account_domain(&state.pool, target.id, actor.id).await? {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    if follow::find(&state.pool, actor.id, target.id)
        .await?
        .is_some()
    {
        return Ok(());
    }
    if target.is_local() {
        // A purely local follow federates nothing: the follow row is the whole
        // durable effect, and its notification is a post-commit best effort.
        if target.locked {
            follow::create_request(&state.pool, actor.id, target.id, None).await?;
            // A group's join request waits in its moderation queue, not a
            // notification to the group account itself.
            if !target.is_group() {
                notify_after_commit(state, target.id, actor.id, "follow_request", None).await;
            }
        } else {
            follow::create(&state.pool, actor.id, target.id, None).await?;
            notify_after_commit(state, target.id, actor.id, "follow", None).await;
        }
    } else {
        // The outgoing follow row and its `Follow` delivery commit together, so
        // a crash can never leave a pending local edge the remote was never
        // asked about, nor a queued Follow with no matching local edge (QC
        // audit #18).
        let target_uri = target.uri.clone().ok_or(ApiError::NotFound)?;
        let marker = id::next();
        let follow_uri =
            activity::follow_uri_for_actor(&account_uri(&state.config.domain, actor), marker);
        let follow_activity =
            activity::follow(&state.config.domain, &actor.username, marker, &target_uri);
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        follow::create_outgoing(&mut *tx, actor.id, target.id, &follow_uri).await?;
        job::enqueue_tx(&mut *tx, actor.id, &target.inbox_url, &follow_activity).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
    }
    Ok(())
}

/// Authorizes `requester`'s pending follow request toward `actor`: the
/// follow becomes active, the `follow_request` notification is replaced by
/// a `follow` one, and remote requesters get the federated `Accept` — like
/// Mastodon's `AuthorizeFollowService`. No pending request is a 404.
pub async fn authorize_follow_request(
    state: &AppState,
    actor: &Account,
    requester: &Account,
) -> Result<(), ApiError> {
    let edge = follow::find(&state.pool, requester.id, actor.id)
        .await?
        .filter(|edge| edge.pending)
        .ok_or(ApiError::NotFound)?;
    // The follow's activation, its notification cleanup, and the federated
    // `Accept` (remote requesters only) commit together, so a crash can never
    // leave an accepted local edge the requester's server was never told about
    // The Accept is built before the transaction.
    let accept = match (requester.is_local(), &requester.uri) {
        (false, Some(requester_uri)) => {
            let their_follow = activity::follow_as_received(
                edge.uri.as_deref().unwrap_or_default(),
                requester_uri,
                &state.config.domain,
                &actor.username,
            );
            Some(activity::accept_follow(
                &state.config.domain,
                &actor.username,
                id::next(),
                &their_follow,
            ))
        }
        _ => None,
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    follow::mark_accepted(&mut *tx, requester.id, actor.id).await?;
    notification::clear_kind_from(&mut *tx, actor.id, requester.id, "follow_request").await?;
    if let Some(accept) = &accept {
        job::enqueue_tx(&mut *tx, actor.id, &requester.inbox_url, accept).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    notify_after_commit(state, actor.id, requester.id, "follow", None).await;
    tracing::info!(follower = %requester.username, target = %actor.username, "follow request authorized");
    Ok(())
}

/// Rejects `requester`'s pending follow request toward `actor`: the request
/// (and its notification) disappear, and remote requesters get the federated
/// `Reject` — like Mastodon's `RejectFollowService`.
pub async fn reject_follow_request(
    state: &AppState,
    actor: &Account,
    requester: &Account,
) -> Result<(), ApiError> {
    let edge = follow::find(&state.pool, requester.id, actor.id)
        .await?
        .filter(|edge| edge.pending)
        .ok_or(ApiError::NotFound)?;
    // The request's removal, its notification cleanup, and the federated
    // `Reject` commit together.
    let reject = match (requester.is_local(), &requester.uri) {
        (false, Some(requester_uri)) => {
            let their_follow = activity::follow_as_received(
                edge.uri.as_deref().unwrap_or_default(),
                requester_uri,
                &state.config.domain,
                &actor.username,
            );
            Some(activity::reject_follow(
                &state.config.domain,
                &actor.username,
                id::next(),
                their_follow,
            ))
        }
        _ => None,
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    follow::delete(&mut *tx, requester.id, actor.id).await?;
    notification::clear_kind_from(&mut *tx, actor.id, requester.id, "follow_request").await?;
    if let Some(reject) = &reject {
        job::enqueue_tx(&mut *tx, actor.id, &requester.inbox_url, reject).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    tracing::info!(follower = %requester.username, target = %actor.username, "follow request rejected");
    Ok(())
}

/// Removes `follower` from `actor`'s followers — Mastodon's
/// `RemoveFromFollowersService`. The follower's edge toward the actor is
/// severed and, for a remote follower, a `Reject(Follow)` tells their server
/// the relationship no longer holds. A missing edge is an idempotent no-op.
pub async fn remove_from_followers(
    state: &AppState,
    actor: &Account,
    follower: &Account,
) -> Result<(), ApiError> {
    let Some(edge) = follow::find(&state.pool, follower.id, actor.id).await? else {
        return Ok(());
    };
    // The edge removal, any pending-request notification cleanup, and the
    // federated `Reject` commit together.
    let reject = match (follower.is_local(), &edge.uri, &follower.uri) {
        (false, Some(uri), Some(follower_uri)) => {
            let their_follow = activity::follow_as_received(
                uri,
                follower_uri,
                &state.config.domain,
                &actor.username,
            );
            Some(activity::reject_follow(
                &state.config.domain,
                &actor.username,
                id::next(),
                their_follow,
            ))
        }
        _ => None,
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    follow::delete(&mut *tx, follower.id, actor.id).await?;
    // A still-pending request takes its notification with it.
    if edge.pending {
        notification::clear_kind_from(&mut *tx, actor.id, follower.id, "follow_request").await?;
    }
    if let Some(reject) = &reject {
        job::enqueue_tx(&mut *tx, actor.id, &follower.inbox_url, reject).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    tracing::info!(follower = %follower.username, target = %actor.username, "removed from followers");
    Ok(())
}

/// Unfollows `target` (or withdraws a pending request): removes the edge
/// and, for remote targets, federates `Undo(Follow)` referencing the
/// original activity. A missing edge is an idempotent no-op, like Mastodon.
pub async fn unfollow_account(
    state: &AppState,
    actor: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    let Some(edge) = follow::find(&state.pool, actor.id, target.id).await? else {
        return Ok(());
    };
    // The edge removal, the withdrawn follow's notification cleanup, and the
    // federated `Undo(Follow)` commit together.
    let undo = match (target.is_local(), &edge.uri, &target.uri) {
        (false, Some(uri), Some(target_uri)) => {
            let follow_activity =
                activity::follow_as_sent(uri, &state.config.domain, &actor.username, target_uri);
            Some(activity::undo(
                &state.config.domain,
                &actor.username,
                follow_activity,
            ))
        }
        _ => None,
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    follow::delete(&mut *tx, actor.id, target.id).await?;
    // A withdrawn follow (or follow request) takes its notification with it
    // (Mastodon destroys the notification with the Follow/FollowRequest row).
    if target.is_local() {
        let kind = if edge.pending {
            "follow_request"
        } else {
            "follow"
        };
        notification::clear_kind_from(&mut *tx, target.id, actor.id, kind).await?;
    }
    if let Some(undo) = &undo {
        job::enqueue_tx(&mut *tx, actor.id, &target.inbox_url, undo).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Blocks `target`: severs follows in both directions (a remote target
/// learns via `Undo(Follow)` / `Reject(Follow)`), erases the target's
/// notifications and shared DM threads, records the block, and federates a
/// `Block` to remote targets. A self-block is an idempotent no-op and
/// re-blocking is harmless, like Mastodon's `BlockService`.
pub async fn block_account(
    state: &AppState,
    actor: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    if actor.id == target.id {
        return Ok(());
    }
    // Our follow of them: the unfollow path already federates the Undo
    // atomically.
    unfollow_account(state, actor, target).await?;
    // Their follow of us, the block row, its notification/DM cleanup, and both
    // the `Reject(Follow)` and `Block` deliveries commit as one unit, so a
    // crash can never record a block while the target keeps following (or is
    // never told), nor queue a Block with no local block row.
    // Federation activities that depend only on reads are built up front.
    let their_edge = follow::find(&state.pool, target.id, actor.id).await?;
    let reject = match &their_edge {
        Some(edge) => match (target.is_local(), &edge.uri, &target.uri) {
            (false, Some(uri), Some(target_uri)) => {
                let their_follow = activity::follow_as_received(
                    uri,
                    target_uri,
                    &state.config.domain,
                    &actor.username,
                );
                Some(activity::reject_follow(
                    &state.config.domain,
                    &actor.username,
                    id::next(),
                    their_follow,
                ))
            }
            _ => None,
        },
        None => None,
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if their_edge.is_some() {
        follow::delete(&mut *tx, target.id, actor.id).await?;
        if let Some(reject) = &reject {
            job::enqueue_tx(&mut *tx, actor.id, &target.inbox_url, reject).await?;
        }
    }
    let block_row_id = block::create(&mut *tx, actor.id, target.id, None).await?;
    notification::clear_from(&mut *tx, actor.id, target.id).await?;
    conversation::remove_with_participant(&mut *tx, actor.id, target.id).await?;
    if let (false, Some(target_uri)) = (target.is_local(), &target.uri) {
        let block_activity = activity::block(
            &state.config.domain,
            &actor.username,
            block_row_id,
            target_uri,
        );
        job::enqueue_tx(&mut *tx, actor.id, &target.inbox_url, &block_activity).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Removes a block; federates `Undo(Block)` to remote targets. A missing
/// block is an idempotent no-op, like Mastodon's `UnblockService`.
pub async fn unblock_account(
    state: &AppState,
    actor: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    // The block's removal and its `Undo(Block)` delivery commit together (QC
    // audit #18). A missing block deletes nothing, so the empty transaction
    // rolls back harmlessly on drop.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let Some(block_row_id) = block::delete(&mut *tx, actor.id, target.id).await? else {
        return Ok(());
    };
    if let (false, Some(target_uri)) = (target.is_local(), &target.uri) {
        let block_activity = activity::block(
            &state.config.domain,
            &actor.username,
            block_row_id,
            target_uri,
        );
        let undo = activity::undo(&state.config.domain, &actor.username, block_activity);
        job::enqueue_tx(&mut *tx, actor.id, &target.inbox_url, &undo).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Blocks a remote domain for `actor`: records the local domain block, removes
/// existing follows in both directions, rejects pending follow requests from
/// that domain, and erases notifications from its accounts. No federation is
/// sent for the domain block itself; only the relationship cleanups federate.
pub async fn block_domain(state: &AppState, actor: &Account, domain: &str) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    account_domain_block::create(&mut *tx, actor.id, domain).await?;
    // The domain's notifications vanish with the block; the follow edges are
    // severed by the queued job (`crate::severance`), off this request — a
    // block against a large instance is O(all your relationships there)
    // (N+1 close-out). Mastodon's semantics exactly.
    notification::clear_from_domain(&mut *tx, actor.id, domain).await?;
    domain_severance_job::enqueue(&mut *tx, actor.id, domain).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Removes a user-level domain block. Missing rows are an idempotent no-op.
pub async fn unblock_domain(
    state: &AppState,
    actor: &Account,
    domain: &str,
) -> Result<(), ApiError> {
    account_domain_block::delete(&state.pool, actor.id, domain).await?;
    Ok(())
}

/// Mutes `target` — purely local, never federated. `duration_secs` of 0
/// mutes forever; re-muting re-configures the existing mute, like Mastodon's
/// `MuteService`. A self-mute is an idempotent no-op.
pub async fn mute_account(
    state: &AppState,
    actor: &Account,
    target: &Account,
    hide_notifications: bool,
    duration_secs: i64,
) -> Result<(), ApiError> {
    if actor.id == target.id {
        return Ok(());
    }
    let expires_at = (duration_secs > 0)
        .then(|| time::OffsetDateTime::now_utc() + time::Duration::seconds(duration_secs));
    mute::upsert(
        &state.pool,
        actor.id,
        target.id,
        hide_notifications,
        expires_at,
    )
    .await?;
    Ok(())
}

/// Removes a mute; a missing one is an idempotent no-op.
pub async fn unmute_account(
    state: &AppState,
    actor: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    mute::delete(&state.pool, actor.id, target.id).await?;
    Ok(())
}

/// Distinct-`status_ids` ceiling for a locally-filed report. An honest report
/// cites a handful of the target's own statuses; the value matches the inbound
/// `Flag` object cap (`inbox::MAX_FLAG_OBJECTS`) so a forwarded report can never
/// build a body larger than a peer would accept.
const MAX_REPORT_STATUSES: usize = 50;

/// Distinct-`rule_ids` ceiling for a locally-filed report. Instance rules are a
/// small admin-defined set, so this is far above any honest citation.
const MAX_REPORT_RULES: usize = 50;

/// The local-user inputs to [`create_report`].
#[derive(Debug, Default)]
pub struct ReportParams<'a> {
    pub comment: &'a str,
    pub category: Option<&'a str>,
    /// Forward the report to a remote target's origin server.
    pub forward: bool,
    pub status_ids: &'a [i64],
    pub rule_ids: Option<&'a [i64]>,
}

/// Files a report against `target`, mirroring Mastodon's `ReportService`. A
/// report about a remote account, when forwarded, federates as a `Flag` signed
/// by the instance actor — so the reporting user stays anonymous to the
/// target's server. Staff review it at `/admin/reports` (API:
/// `/api/v1/admin/reports`); every staff member who can manage reports gets
/// the Mastodon-compatible `admin.report` notification, and the
/// `report.created` webhook fires.
pub async fn create_report(
    state: &AppState,
    reporter: &Account,
    target: &Account,
    params: ReportParams<'_>,
) -> Result<Report, ApiError> {
    create_report_scoped(state, reporter, target, params, None).await
}

/// Community-scoped variant of [`create_report`], used by the Lemmy adapter
/// so the group's own moderators receive and may resolve the report.
pub async fn create_report_scoped(
    state: &AppState,
    reporter: &Account,
    target: &Account,
    params: ReportParams<'_>,
    group_account_id: Option<i64>,
) -> Result<Report, ApiError> {
    // Mastodon's `Report::COMMENT_SIZE_LIMIT` for locally-filed reports.
    if params.comment.chars().count() > 1000 {
        return Err(ApiError::Unprocessable(
            "Validation failed: Comment is too long (maximum is 1000 characters)".into(),
        ));
    }

    // Bound + deduplicate the client id arrays before any per-id database work
    // An honest report cites a handful of the target's own
    // statuses and the instance's small fixed rule set; capping distinct
    // `status_ids` at the same ceiling as an inbound `Flag` keeps the serial
    // `find_by_id` loop, the stored array, and — when forwarded — the outbound
    // `Flag` body all bounded regardless of what the ~2 MiB body carried.
    let status_ids_input =
        crate::routes::params::bounded_unique_ids(params.status_ids.to_vec(), MAX_REPORT_STATUSES)?;
    let rule_ids_input = params
        .rule_ids
        .map(|ids| crate::routes::params::bounded_unique_ids(ids.to_vec(), MAX_REPORT_RULES))
        .transpose()?;

    // Citing rules forces the `violation` category, like `ReportService`.
    let has_rules = rule_ids_input.as_ref().is_some_and(|ids| !ids.is_empty());
    let category = if has_rules {
        "violation"
    } else {
        params.category.filter(|c| !c.is_empty()).unwrap_or("other")
    };
    if !matches!(category, "other" | "spam" | "legal" | "violation") {
        return Err(ApiError::Unprocessable(
            "Validation failed: Category is not included in the list".into(),
        ));
    }

    // Only the reported account's own statuses may be attached; an unknown or
    // foreign id 404s, like Mastodon's `.find` scoped to the target's posts.
    // Fetch the whole capped, deduplicated set in one query and verify each
    // cited id against it, so validation is a single round trip rather than one
    // `find_by_id` per id.
    let owned: std::collections::HashMap<i64, status::Status> =
        status::find_by_ids(&state.pool, &status_ids_input)
            .await?
            .into_iter()
            .filter(|stored| stored.account_id == target.id)
            .map(|stored| (stored.id, stored))
            .collect();
    let mut statuses = Vec::with_capacity(status_ids_input.len());
    for status_id in &status_ids_input {
        statuses.push(owned.get(status_id).cloned().ok_or(ApiError::NotFound)?);
    }
    let status_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();

    // A report about a remote account can be forwarded to its origin.
    let forwarded = !target.has_local_account_on(&state.config.domain) && params.forward;
    let rule_ids = rule_ids_input.as_deref().filter(|ids| !ids.is_empty());

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let report = report::create_scoped(
        &mut *tx,
        report::NewReport {
            account_id: reporter.id,
            target_account_id: target.id,
            status_ids: &status_ids,
            comment: params.comment,
            category,
            forwarded: Some(forwarded),
            rule_ids,
            // Our report's federated id is derived from the row id at
            // federation time; storing it would cost an extra write for a
            // value never read back.
            uri: None,
        },
        group_account_id,
    )
    .await?;

    if forwarded && let Some(target_uri) = &target.uri {
        let mut object_uris = vec![target_uri.clone()];
        for stored in &statuses {
            object_uris.push(status_uri_for_account(&state.config.domain, stored, target));
        }
        let flag = activity::flag(
            &InstanceActorUrls::new(&state.config.domain).id,
            &report_uri(&state.config.domain, report.id),
            params.comment,
            &object_uris,
        );
        job::enqueue_from_instance_tx(&mut *tx, &target.inbox_url, &flag).await?;
        tracing::info!(reporter = %reporter.username, target = %target.username, "report forwarded");
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if let Err(error) = notify_staff_about_report(state, &report).await {
        tracing::warn!(%error, "post-commit report notification failed");
    }
    crate::webhooks::report_event(state, plamenu_db::webhook::REPORT_CREATED, &report).await;
    Ok(report)
}

/// Emits the Mastodon-compatible `admin.report` in-app notification to every
/// staff member who can manage reports — the counterpart of
/// `registration::notify_staff_about_sign_up` for the moderation queue. The
/// entity embeds the report itself, so a client can jump straight to it.
/// Called for locally-filed reports and inbound `Flag`s alike; the inbound
/// redelivery guard (`report::exists_by_uri`) keeps a re-sent `Flag` from
/// notifying twice, and the self-notification guard in `notification::create*`
/// covers a staff member filing their own report.
pub async fn notify_staff_about_report(state: &AppState, report: &Report) -> Result<(), ApiError> {
    let mut recipients: std::collections::HashSet<i64> =
        role::account_ids_who_can(&state.pool, role::permission::MANAGE_REPORTS)
            .await?
            .into_iter()
            .collect();
    if let Some(group_id) = report.group_account_id {
        recipients.extend(
            group::elevated(&state.pool, group_id)
                .await?
                .into_iter()
                .map(|entry| entry.account_id),
        );
    }
    for account_id in recipients {
        notification::create_admin_report(&state.pool, account_id, report.account_id, report.id)
            .await?;
    }
    Ok(())
}

/// The result of [`follow_remote`], for reporting.
#[derive(Debug)]
pub struct FollowOutcome {
    pub target_uri: String,
    pub target_inbox: String,
}

/// Resolves `target` (`user@domain`), stores the remote account, records a
/// pending follow and enqueues the `Follow` activity.
pub async fn follow_remote(
    state: &AppState,
    username: &str,
    target: &str,
) -> Result<FollowOutcome, ApiError> {
    let local = account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or(ApiError::NotFound)?;
    let acct: Acct = target
        .parse()
        .map_err(|e: plamenu_ap::acct::AcctError| ApiError::BadRequest(e.to_string()))?;
    if state.config.is_local_domain(acct.domain()) {
        return Err(ApiError::BadRequest(
            "local-to-local follows are not federated".into(),
        ));
    }
    if !crate::instance_policy::can_federate_domain(
        &state.pool,
        &state.config.domain,
        acct.domain(),
    )
    .await?
    {
        return Err(ApiError::BadRequest(
            "cannot follow a blocked or non-allowed domain".into(),
        ));
    }

    let resolved = state
        .federation
        .resolve_acct(&acct)
        .await
        .map_err(|e| ApiError::BadRequest(format!("cannot resolve {acct}: {e}")))?;
    if !crate::instance_policy::can_federate_domain(
        &state.pool,
        &state.config.domain,
        resolved.acct.domain(),
    )
    .await?
    {
        return Err(ApiError::BadRequest(
            "cannot follow a blocked or non-allowed domain".into(),
        ));
    }
    let uri = resolved.actor_uri.clone();
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &uri).await? {
        return Err(ApiError::BadRequest(
            "cannot follow a blocked or non-allowed domain".into(),
        ));
    }
    let actor = state
        .federation
        .fetch_actor(&uri)
        .await
        .map_err(|e| ApiError::BadRequest(format!("cannot fetch {uri}: {e}")))?;
    let remote = store_remote_actor_from_resolution(state, &actor, &resolved).await?;

    follow_account(state, &local, &remote).await?;
    Ok(FollowOutcome {
        target_uri: actor.id,
        target_inbox: remote.inbox_url,
    })
}
