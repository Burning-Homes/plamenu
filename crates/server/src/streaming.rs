//! The streaming hub: one Postgres `LISTEN` task per process receives the
//! row-change events of `plamenu_db::streaming`, renders them as Mastodon
//! streaming messages and pushes them to subscribed websocket connections
//! (the `/api/v1/streaming` route).
//!
//! Semantics mirror Mastodon's streaming server: `update`/`status.update`
//! payloads are rendered per recipient on user streams and anonymously on
//! the shared (public/hashtag) streams; routing applies the same block/mute
//! gates as the matching timeline queries, and notifications the same
//! `sender_filtered` gate as the notification listings.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use plamenu_db::status::Status;
use plamenu_db::streaming::{self as events, Event};
use plamenu_db::{account, announcement, conversation, list, mention, notification, status, tag};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::error::ApiError;
use crate::sync::RecoverableMutex as _;

/// A stream a client can subscribe to — Mastodon's channel set, minus the
/// `only_media` variants.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Channel {
    User(i64),
    UserNotification(i64),
    Public,
    PublicLocal,
    PublicRemote,
    /// Lowercased hashtag name.
    Hashtag(String),
    HashtagLocal(String),
    Direct(i64),
    /// A list timeline; only the list's owner may subscribe.
    List(i64),
}

impl Channel {
    /// The `stream` array carried on every message of this channel —
    /// Mastodon's `streamNameFromChannelName`.
    fn stream_name(&self) -> Value {
        match self {
            Self::User(_) => json!(["user"]),
            Self::UserNotification(_) => json!(["user:notification"]),
            Self::Public => json!(["public"]),
            Self::PublicLocal => json!(["public:local"]),
            Self::PublicRemote => json!(["public:remote"]),
            Self::Hashtag(tag) => json!(["hashtag", tag]),
            Self::HashtagLocal(tag) => json!(["hashtag:local", tag]),
            Self::Direct(_) => json!(["direct"]),
            Self::List(id) => json!(["list", id.to_string()]),
        }
    }
}

/// Bounded outbound queue depth per connection. When a client stops reading,
/// at most this many rendered events buffer before the hub drops it as
/// lagging — a hard per-connection memory ceiling in place of the former
/// unbounded channel (finding 27). The connection task ([`crate::routes`]'
/// streaming route) sizes its channel from this, and [`Hub::send_filtered`]
/// relies on the resulting `try_send` overflow to shed a slow reader.
pub(crate) const CONNECTION_QUEUE_CAP: usize = 256;

/// One websocket's interest in one channel.
struct Subscriber {
    sender: mpsc::Sender<String>,
    viewer: i64,
    /// Whether this `user` stream carries notification events — granted
    /// only when the token may read notifications (Mastodon's scope split).
    notifications: bool,
    /// Fires when this connection must close because its bounded outbound
    /// queue overflowed. The connection task selects on it and tears the
    /// socket down (finding 27 — a slow reader is dropped, not buffered).
    close: Arc<Notify>,
}

/// Live websocket connection counts, total and per authenticated viewer.
#[derive(Default)]
struct ConnectionCounts {
    total: usize,
    per_viewer: HashMap<i64, usize>,
}

/// The in-process subscriber registry. Connections register their interest
/// per channel; the listener task routes events to whoever is subscribed.
#[derive(Default)]
pub struct Hub {
    connection_counter: AtomicU64,
    channels: Mutex<HashMap<Channel, HashMap<u64, Subscriber>>>,
    /// Live connection counts, used to bound how many sockets one account
    /// (or the whole process) can hold open at once.
    connections: Mutex<ConnectionCounts>,
    listening: std::sync::atomic::AtomicBool,
}

/// Releases a connection slot admitted by [`Hub::try_open_connection`] when
/// the websocket task ends (normally, on error, or on cancellation).
pub struct ConnectionGuard {
    hub: Arc<Hub>,
    viewer: i64,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.hub.release_connection(self.viewer);
    }
}

impl Hub {
    /// Live websocket connections one account may hold open at once. A
    /// generous ceiling for legitimate multi-device/multi-tab use; it caps
    /// the slow-reader queue memory (`CONNECTION_QUEUE_CAP` × this) an
    /// account can pin.
    pub const MAX_CONNECTIONS_PER_VIEWER: usize = 32;

    /// Live websocket connections the whole process will accept at once.
    pub const MAX_TOTAL_CONNECTIONS: usize = 4096;

    /// Admits one websocket connection for `viewer`, returning a guard that
    /// releases the slot on drop, or `None` when the per-viewer or global
    /// ceiling is already reached. Enforced at the upgrade so an abusive
    /// account cannot open an unbounded number of (slow) sockets.
    #[must_use]
    pub fn try_open_connection(self: &Arc<Self>, viewer: i64) -> Option<ConnectionGuard> {
        let mut counts = self.connections.lock_or_recover();
        if counts.total >= Self::MAX_TOTAL_CONNECTIONS {
            return None;
        }
        let per = counts.per_viewer.entry(viewer).or_default();
        if *per >= Self::MAX_CONNECTIONS_PER_VIEWER {
            return None;
        }
        *per += 1;
        counts.total += 1;
        Some(ConnectionGuard {
            hub: Arc::clone(self),
            viewer,
        })
    }

    fn release_connection(&self, viewer: i64) {
        let mut counts = self.connections.lock_or_recover();
        counts.total = counts.total.saturating_sub(1);
        if let Some(per) = counts.per_viewer.get_mut(&viewer) {
            *per -= 1;
            if *per == 0 {
                counts.per_viewer.remove(&viewer);
            }
        }
    }

    /// Whether the LISTEN connection is currently established — events
    /// published before this turns true are lost (NOTIFY has no replay).
    /// Tests gate on it before triggering events.
    #[must_use]
    pub fn is_listening(&self) -> bool {
        self.listening.load(Ordering::Acquire)
    }

    fn set_listening(&self, listening: bool) {
        self.listening.store(listening, Ordering::Release);
    }

    /// A fresh id for one websocket connection.
    pub fn connection_id(&self) -> u64 {
        self.connection_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers `connection`'s interest in `channel`. Re-subscribing
    /// replaces the previous registration, like Mastodon ignores duplicate
    /// subscribes.
    pub fn subscribe(
        &self,
        connection: u64,
        channel: Channel,
        viewer: i64,
        notifications: bool,
        sender: mpsc::Sender<String>,
        close: Arc<Notify>,
    ) {
        self.channels
            .lock()
            .unwrap()
            .entry(channel)
            .or_default()
            .insert(
                connection,
                Subscriber {
                    sender,
                    viewer,
                    notifications,
                    close,
                },
            );
    }

    pub fn unsubscribe(&self, connection: u64, channel: &Channel) {
        let mut channels = self.channels.lock_or_recover();
        if let Some(subscribers) = channels.get_mut(channel) {
            subscribers.remove(&connection);
            if subscribers.is_empty() {
                channels.remove(channel);
            }
        }
    }

    /// Drops every subscription of a closed connection.
    pub fn disconnect(&self, connection: u64) {
        let mut channels = self.channels.lock_or_recover();
        channels.retain(|_, subscribers| {
            subscribers.remove(&connection);
            !subscribers.is_empty()
        });
    }

    /// Closes every live streaming connection owned by `viewer`. Suspension
    /// calls this immediately after its database transaction commits, so an
    /// already-open websocket cannot keep receiving private/home events with a
    /// token that would now be rejected on reconnect.
    pub fn disconnect_viewer(&self, viewer: i64) {
        let mut closing: HashMap<u64, Arc<Notify>> = HashMap::new();
        let mut channels = self.channels.lock_or_recover();
        channels.retain(|_, subscribers| {
            subscribers.retain(|connection, subscriber| {
                if subscriber.viewer == viewer {
                    closing
                        .entry(*connection)
                        .or_insert_with(|| Arc::clone(&subscriber.close));
                    false
                } else {
                    true
                }
            });
            !subscribers.is_empty()
        });
        drop(channels);
        for close in closing.into_values() {
            close.notify_one();
        }
    }

    /// Total live subscriptions, over all channels (used by tests to wait
    /// for a subscription to land before acting).
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        self.channels
            .lock()
            .unwrap()
            .values()
            .map(HashMap::len)
            .sum()
    }

    fn is_idle(&self) -> bool {
        self.channels.lock_or_recover().is_empty()
    }

    /// Distinct viewer account ids subscribed to `channel`.
    fn viewers_of(&self, channel: &Channel) -> Vec<i64> {
        let channels = self.channels.lock_or_recover();
        let mut viewers: Vec<i64> = channels
            .get(channel)
            .map(|subscribers| subscribers.values().map(|s| s.viewer).collect())
            .unwrap_or_default();
        viewers.sort_unstable();
        viewers.dedup();
        viewers
    }

    /// Distinct account ids with a live `user` stream.
    fn user_stream_viewers(&self) -> Vec<i64> {
        let channels = self.channels.lock_or_recover();
        let mut viewers: Vec<i64> = channels
            .keys()
            .filter_map(|channel| match channel {
                Channel::User(id) => Some(*id),
                _ => None,
            })
            .collect();
        viewers.sort_unstable();
        viewers.dedup();
        viewers
    }

    /// Distinct list ids with a live `list` stream.
    fn list_stream_ids(&self) -> Vec<i64> {
        let channels = self.channels.lock_or_recover();
        channels
            .keys()
            .filter_map(|channel| match channel {
                Channel::List(id) => Some(*id),
                _ => None,
            })
            .collect()
    }

    /// Sends `event` to every subscriber of `channel`. `payload` is already
    /// the wire form: serialized entity JSON, or a bare id for `delete`.
    fn send(&self, channel: &Channel, event: &str, payload: &str) {
        self.send_filtered(channel, event, payload, |_| true);
    }

    /// Sends to the subscribers of `channel` whose viewer is in `allowed`.
    fn send_to_viewers(&self, channel: &Channel, event: &str, payload: &str, allowed: &[i64]) {
        self.send_filtered(channel, event, payload, |s| allowed.contains(&s.viewer));
    }

    /// Sends a notification to `account_id`'s streams: `user:notification`
    /// always, `user` only where the token may read notifications.
    fn send_notification(&self, account_id: i64, payload: &str) {
        self.send(
            &Channel::UserNotification(account_id),
            "notification",
            payload,
        );
        self.send_filtered(&Channel::User(account_id), "notification", payload, |s| {
            s.notifications
        });
    }

    fn send_filtered(
        &self,
        channel: &Channel,
        event: &str,
        payload: &str,
        include: impl Fn(&Subscriber) -> bool,
    ) {
        let mut channels = self.channels.lock_or_recover();
        let Some(subscribers) = channels.get_mut(channel) else {
            return;
        };
        let message = json!({
            "stream": channel.stream_name(),
            "event": event,
            "payload": payload,
        })
        .to_string();
        // A consumer whose bounded queue is full is lagging: rather than
        // hand it a gappy stream (or let its queue grow without bound),
        // signal its task to close and drop the subscription now. A closed
        // receiver means the connection is already going away.
        let mut lagging: Vec<u64> = Vec::new();
        for (connection, subscriber) in subscribers.iter().filter(|(_, s)| include(s)) {
            match subscriber.sender.try_send(message.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    subscriber.close.notify_one();
                    lagging.push(*connection);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => lagging.push(*connection),
            }
        }
        for connection in lagging {
            subscribers.remove(&connection);
        }
        if subscribers.is_empty() {
            channels.remove(channel);
        }
    }
}

/// Publishes a status-created event; failures are logged, never fatal —
/// the action itself has already succeeded.
pub async fn status_created(state: &AppState, status_id: i64) {
    publish(state, &Event::StatusNew { status_id }).await;
}

/// Mastodon's `REAL_TIME_WINDOW`: an inbound status or boost older than this
/// is stored (and notifies) but is not pushed into live feeds — a replayed
/// backlog must not surface day-old posts on top of live timelines.
const REAL_TIME_WINDOW: time::Duration = time::Duration::hours(6);

/// Whether a freshly ingested remote status is recent enough to announce on
/// live timelines (Mastodon's `Status#within_realtime_window?`).
#[must_use]
pub fn within_realtime_window(item: &Status) -> bool {
    item.created_at >= time::OffsetDateTime::now_utc() - REAL_TIME_WINDOW
}

/// Publishes a status-edited event.
pub async fn status_edited(state: &AppState, status_id: i64) {
    publish(state, &Event::StatusEdit { status_id }).await;
}

/// Publishes an announcement-reaction-changed event for one emoji.
pub async fn announcement_reaction(state: &AppState, announcement_id: i64, name: &str) {
    publish(
        state,
        &Event::AnnouncementReaction {
            announcement_id,
            name: name.to_owned(),
        },
    )
    .await;
}

/// Publishes a newly available announcement to every live `user` stream.
pub async fn announcement_published(state: &AppState, announcement_id: i64) {
    publish(state, &Event::Announcement { announcement_id }).await;
}

/// Publishes an announcement deletion to every live `user` stream.
pub async fn announcement_deleted(state: &AppState, announcement_id: i64) {
    publish(state, &Event::AnnouncementDelete { announcement_id }).await;
}

/// Captures the routing data of a status about to be deleted — its tag
/// links and mentions cascade away with the row, so this must run *before*
/// the delete; [`publish`] the event after the delete succeeds.
pub async fn prepare_delete_event(state: &AppState, item: &Status) -> Result<Event, ApiError> {
    let author = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let original = item.reblog_of_id.is_none();
    let public = item.visibility == "public" && original;
    let local_account = author.has_local_account_on(&state.config.domain);
    let local_only = item.visibility == "local" && local_account && original;
    let tags = if public {
        tag::names_for_statuses(&state.pool, &[item.id])
            .await?
            .remove(&item.id)
            .unwrap_or_default()
            .iter()
            .map(|name| name.to_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    let direct_recipients = if item.visibility == "direct" {
        let mentioned = mention::for_statuses(&state.pool, &[item.id], false)
            .await?
            .remove(&item.id)
            .unwrap_or_default();
        let mut recipients: Vec<i64> = mentioned
            .iter()
            .filter(|account| account.is_local())
            .map(|account| account.id)
            .collect();
        if author.is_local() && !recipients.contains(&author.id) {
            recipients.push(author.id);
        }
        recipients
    } else {
        Vec::new()
    };
    Ok(Event::StatusDelete {
        status_id: item.id,
        account_id: item.account_id,
        local: local_account,
        local_only,
        public,
        tags,
        direct_recipients,
    })
}

/// Publishes an event, best-effort: the row change is already committed, so
/// a failed announcement only costs connected clients a live update.
pub async fn publish(state: &AppState, event: &Event) {
    if let Err(error) = events::publish(&state.pool, event).await {
        tracing::warn!(%error, ?event, "failed to publish streaming event");
    }
}

/// Runs the LISTEN loop until the process exits, reconnecting on failure.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("streaming hub started");
        loop {
            if let Err(error) = listen(&state).await {
                state.streaming.set_listening(false);
                if state.shutdown.is_cancelled() {
                    return;
                }
                tracing::warn!(%error, "streaming listener disconnected; reconnecting");
                if !crate::workers::pause(&state, std::time::Duration::from_secs(1)).await {
                    return;
                }
            }
        }
    })
}

async fn listen(state: &AppState) -> Result<(), plamenu_db::DbError> {
    let mut listener = events::listener(&state.pool).await?;
    state.streaming.set_listening(true);
    loop {
        let message = listener.recv().await?;
        if state.streaming.is_idle() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Event>(message.payload()) else {
            tracing::warn!(payload = message.payload(), "unparseable streaming event");
            continue;
        };
        if let Err(error) = handle_event(state, &event).await {
            tracing::warn!(%error, ?event, "failed to route streaming event");
        }
    }
}

/// Routes one already-delivered event to the subscribers it belongs to.
///
/// Public so the benchmark suite can drive it directly. `LISTEN`/`NOTIFY`
/// delivery is asynchronous and unobservable from inside the process, so a
/// benchmark that published an event would be timing "how long until a
/// separate task got round to it"; the routing itself — one full
/// `render_status` per home recipient, serially, inside the single-threaded
/// listener loop — is the part whose cost scales with connected clients.
pub async fn handle_event(state: &AppState, event: &Event) -> Result<(), ApiError> {
    match event {
        Event::StatusNew { status_id } => route_status(state, *status_id, "update").await,
        Event::StatusEdit { status_id } => route_status(state, *status_id, "status.update").await,
        Event::StatusDelete {
            status_id,
            account_id,
            local,
            local_only,
            public,
            tags,
            direct_recipients,
        } => {
            route_delete(
                state,
                *status_id,
                *account_id,
                *local,
                *local_only,
                *public,
                tags,
                direct_recipients,
            )
            .await
        }
        Event::Notification {
            notification_id,
            account_id,
        } => route_notification(state, *notification_id, *account_id).await,
        Event::Conversation { row_id, account_id } => {
            route_conversation(state, *row_id, *account_id).await
        }
        Event::AnnouncementReaction {
            announcement_id,
            name,
        } => route_announcement_reaction(state, *announcement_id, name).await,
        Event::Announcement { announcement_id } => {
            route_announcement(state, *announcement_id).await
        }
        Event::AnnouncementDelete { announcement_id } => {
            route_announcement_delete(state, *announcement_id);
            Ok(())
        }
    }
}

/// Routes an announcement publish: the entity is assembled per viewer so
/// reaction `me` and read state stay correct if an older announcement is
/// re-published — but every fact loads batched across the whole viewer set:
/// one anonymous reaction-group pass plus each viewer's
/// own reaction rows and dismissals, then one viewer-set entity render.
async fn route_announcement(state: &AppState, announcement_id: i64) -> Result<(), ApiError> {
    let hub = &state.streaming;
    let viewers = hub.user_stream_viewers();
    if viewers.is_empty() {
        return Ok(());
    }
    let Some(ann) = announcement::find_by_id(&state.pool, announcement_id).await? else {
        return Ok(());
    };
    if !ann.published {
        return Ok(());
    }
    let base_groups = announcement::reactions_for(&state.pool, &[ann.id], None)
        .await?
        .remove(&ann.id)
        .unwrap_or_default();
    let mine = announcement::viewer_reactions(&state.pool, &[ann.id], &viewers).await?;
    let dismissed = announcement::muted_for_viewers(&state.pool, &viewers, &[ann.id]).await?;
    let set: Vec<Option<i64>> = viewers.iter().copied().map(Some).collect();
    let mut reactions = HashMap::with_capacity(viewers.len());
    let mut read = std::collections::HashSet::with_capacity(viewers.len());
    for &viewer in &viewers {
        // `me` per viewer is membership in that viewer's own reaction rows —
        // exactly the `bool_or(account_id = viewer)` the per-viewer query
        // computed.
        let groups: Vec<announcement::ReactionGroup> = base_groups
            .iter()
            .cloned()
            .map(|mut group| {
                group.me =
                    mine.contains(&(viewer, ann.id, group.name.clone(), group.custom_emoji_id));
                group
            })
            .collect();
        reactions.insert((Some(viewer), ann.id), groups);
        if dismissed.contains(&(viewer, ann.id)) {
            read.insert((Some(viewer), ann.id));
        }
    }
    let mut per_viewer = crate::entities::announcement_json_for_viewer_set(
        state,
        std::slice::from_ref(&ann),
        &set,
        &reactions,
        &read,
    )
    .await?;
    for viewer in viewers {
        if let Some(mut values) = per_viewer.remove(&Some(viewer))
            && let Some(payload) = values.pop()
        {
            hub.send(&Channel::User(viewer), "announcement", &payload.to_string());
        }
    }
    Ok(())
}

/// Routes an announcement deletion: Mastodon sends the announcement id as the
/// payload to every `user` stream.
fn route_announcement_delete(state: &AppState, announcement_id: i64) {
    let hub = &state.streaming;
    for viewer in hub.user_stream_viewers() {
        hub.send(
            &Channel::User(viewer),
            "announcement.delete",
            &announcement_id.to_string(),
        );
    }
}

/// Routes an announcement reaction change: one anonymous `announcement.reaction`
/// payload (the emoji's current tally, `me: false`) broadcast to every live
/// `user` stream — Mastodon's `PublishAnnouncementReactionWorker`, which fans
/// the same payload to every active account.
async fn route_announcement_reaction(
    state: &AppState,
    announcement_id: i64,
    name: &str,
) -> Result<(), ApiError> {
    let hub = &state.streaming;
    let viewers = hub.user_stream_viewers();
    if viewers.is_empty() {
        return Ok(());
    }
    let payload = crate::entities::announcement_reaction_json(
        &state.pool,
        &state.config.domain,
        announcement_id,
        name,
    )
    .await?
    .to_string();
    for viewer in viewers {
        hub.send(&Channel::User(viewer), "announcement.reaction", &payload);
    }
    Ok(())
}

/// The home-timeline audience of a status among the connected `user`-stream
/// viewers: the author and their accepted followers, plus followers of any
/// hashtag the status carries (public, non-reblog only), block/mute-gated
/// like the home timeline query. Sorted and deduped; empty when nobody
/// relevant is connected.
async fn home_stream_audience(state: &AppState, item: &Status) -> Result<Vec<i64>, ApiError> {
    let candidates = state.streaming.user_stream_viewers();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    // The boost target answers two questions at once: whose post was boosted
    // (block/mute gating) and whether it is itself a reply (the per-follow
    // reply switch judging an Announce by what it announces).
    let boost_target = match item.reblog_of_id {
        Some(target_id) => status::find_by_id(&state.pool, target_id).await?,
        None => None,
    };
    let mut recipients = events::home_stream_recipients(
        &state.pool,
        &candidates,
        item.account_id,
        boost_target.as_ref().map(|target| target.account_id),
        item.language.as_deref(),
        item.in_reply_to_id,
        boost_target
            .as_ref()
            .is_some_and(|target| target.in_reply_to_id.is_some()),
    )
    .await?;
    let tag_recipients =
        events::tag_follow_stream_recipients(&state.pool, &candidates, item.id).await?;
    if !tag_recipients.is_empty() {
        recipients.extend(tag_recipients);
    }
    recipients.sort_unstable();
    recipients.dedup();
    Ok(recipients)
}

/// Whether this status must be kept off the shared public/local streams
/// because it answers somebody else. The public and local timeline
/// queries drop the same rows, and the streams have to agree with them or a
/// live page and a reload of it disagree.
///
/// Asked only on the two paths in [`route_status`] that actually reach a
/// shared stream — everything before them returns first, and this costs a
/// query per routed status. Hashtag streams are deliberately *not* gated: the
/// tag timeline query keeps replies, and Mastodon's own suppression of them
/// there contradicts its REST `TagFeed`.
async fn hidden_from_public_streams(state: &AppState, status_id: i64) -> Result<bool, ApiError> {
    Ok(!state.public_timeline_replies().await
        && status::is_non_self_reply(&state.pool, status_id).await?)
}

/// Routes a created or edited status: per-recipient renders on the `user`
/// streams of the home-timeline audience, owner renders on the `list`
/// streams whose timeline carries the status, and one anonymous render on
/// the public/hashtag streams for public originals. Direct statuses never
/// produce `update`s — they surface as `conversation` events, the way they
/// live in `/api/v1/conversations`.
///
/// The home and list audiences render together through ONE viewer-set pass:
/// every distinct recipient is rendered exactly once, so
/// a list owner who also receives the status on their `user` stream shares
/// one document, and the statement count no longer grows with the number of
/// connected viewers or list streams.
async fn route_status(state: &AppState, status_id: i64, event: &str) -> Result<(), ApiError> {
    let hub = &state.streaming;
    let Some(item) = status::find_by_id(&state.pool, status_id).await? else {
        return Ok(()); // already deleted
    };
    // Suspension takes effect before any user/list/public audience is
    // computed; existing posts must not leak through a late edit or replay.
    if account::is_suspended(&state.pool, item.account_id).await? {
        return Ok(());
    }
    if item.visibility == "direct" {
        return Ok(());
    }

    let home_recipients = home_stream_audience(state, &item).await?;
    let list_candidates = hub.list_stream_ids();
    let receiving = if list_candidates.is_empty() {
        Vec::new()
    } else {
        list::streams_receiving(&state.pool, &list_candidates, item.id).await?
    };
    let mut recipients: Vec<i64> = home_recipients
        .iter()
        .copied()
        .chain(receiving.iter().map(|entry| entry.owner_id))
        .collect();
    recipients.sort_unstable();
    recipients.dedup();
    if !recipients.is_empty() {
        let rendered = crate::entities::render_status_for_viewers(
            &state.pool,
            &state.config.domain,
            &item,
            &recipients,
        )
        .await?;
        for viewer in &home_recipients {
            if let Some(entity) = rendered.get(viewer) {
                hub.send(&Channel::User(*viewer), event, &entity.to_string());
            }
        }
        for entry in &receiving {
            if let Some(entity) = rendered.get(&entry.owner_id) {
                hub.send(&Channel::List(entry.list_id), event, &entity.to_string());
            }
        }
    }

    // Silenced authors (own silence or a domain silence) never reach the
    // shared public/local/hashtag streams — the home and list delivery above
    // is unaffected, mirroring the timeline queries.
    if account::effectively_silenced(&state.pool, item.account_id).await? {
        return Ok(());
    }

    if item.visibility == "local" {
        if item.reblog_of_id.is_some() || hidden_from_public_streams(state, item.id).await? {
            return Ok(());
        }
        let author = account::find_by_id(&state.pool, item.account_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if !author.has_local_account_on(&state.config.domain) {
            return Ok(());
        }
        let viewers = hub.viewers_of(&Channel::PublicLocal);
        if viewers.is_empty() {
            return Ok(());
        }
        let allowed = events::unhidden_viewers(&state.pool, &viewers, item.account_id).await?;
        let entity =
            crate::entities::render_status(&state.pool, &state.config.domain, &item, None).await?;
        hub.send_to_viewers(&Channel::PublicLocal, event, &entity.to_string(), &allowed);
        return Ok(());
    }

    // Shared streams carry public originals only, like the public/tag
    // timeline queries.
    if item.visibility != "public" || item.reblog_of_id.is_some() {
        return Ok(());
    }
    let author = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut channels = if hidden_from_public_streams(state, item.id).await? {
        Vec::new()
    } else {
        vec![
            Channel::Public,
            if author.has_local_account_on(&state.config.domain) {
                Channel::PublicLocal
            } else {
                Channel::PublicRemote
            },
        ]
    };
    let tags = tag::names_for_statuses(&state.pool, &[item.id])
        .await?
        .remove(&item.id)
        .unwrap_or_default();
    for name in &tags {
        let lower = name.to_lowercase();
        if author.has_local_account_on(&state.config.domain) {
            channels.push(Channel::HashtagLocal(lower.clone()));
        }
        channels.push(Channel::Hashtag(lower));
    }
    fan_out_to_shared_channels(state, &item, event, channels).await
}

/// Sends one anonymous status payload to every non-empty shared channel. One
/// block/mute pass serves them all: the union of subscribed viewers is
/// filtered against the author once, and each channel sends to its own
/// subscribers under that shared verdict.
async fn fan_out_to_shared_channels(
    state: &AppState,
    item: &Status,
    event: &str,
    channels: Vec<Channel>,
) -> Result<(), ApiError> {
    let hub = &state.streaming;
    let channel_viewers: Vec<(Channel, Vec<i64>)> = channels
        .into_iter()
        .map(|channel| {
            let viewers = hub.viewers_of(&channel);
            (channel, viewers)
        })
        .filter(|(_, viewers)| !viewers.is_empty())
        .collect();
    if channel_viewers.is_empty() {
        return Ok(());
    }
    let mut candidates: Vec<i64> = channel_viewers
        .iter()
        .flat_map(|(_, viewers)| viewers.iter().copied())
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    let allowed = events::unhidden_viewers(&state.pool, &candidates, item.account_id).await?;
    // One anonymous render serves every shared stream, like Mastodon's
    // broadcast payloads.
    let entity =
        crate::entities::render_status(&state.pool, &state.config.domain, item, None).await?;
    let payload = entity.to_string();
    for (channel, viewers) in channel_viewers {
        let channel_allowed: Vec<i64> = viewers
            .into_iter()
            .filter(|viewer| allowed.contains(viewer))
            .collect();
        hub.send_to_viewers(&channel, event, &payload, &channel_allowed);
    }
    Ok(())
}

/// Routes a deletion: the payload is the bare status id, sent to every
/// stream the status appeared on. Deletes are not block/mute-filtered
/// beyond the home audience — an id leaks nothing (Mastodon does the same).
#[allow(clippy::too_many_arguments)]
async fn route_delete(
    state: &AppState,
    status_id: i64,
    account_id: i64,
    local: bool,
    local_only: bool,
    public: bool,
    tags: &[String],
    direct_recipients: &[i64],
) -> Result<(), ApiError> {
    let hub = &state.streaming;
    let payload = status_id.to_string();

    let candidates = hub.user_stream_viewers();
    if !candidates.is_empty() {
        let mut recipients = events::home_stream_recipients(
            &state.pool,
            &candidates,
            account_id,
            None,
            None,
            None,
            false,
        )
        .await?;
        // Tag followers that received the status via hashtag injection also
        // need the delete (a bare id, so no visibility re-check).
        if public && !tags.is_empty() {
            recipients
                .extend(events::tag_follow_viewers_by_names(&state.pool, &candidates, tags).await?);
            recipients.sort_unstable();
            recipients.dedup();
        }
        for viewer in recipients {
            hub.send(&Channel::User(viewer), "delete", &payload);
        }
    }
    let list_candidates = hub.list_stream_ids();
    if !list_candidates.is_empty() {
        for list_id in list::streams_with_member(&state.pool, &list_candidates, account_id).await? {
            hub.send(&Channel::List(list_id), "delete", &payload);
        }
    }
    for recipient in direct_recipients {
        hub.send(&Channel::User(*recipient), "delete", &payload);
        hub.send(&Channel::Direct(*recipient), "delete", &payload);
    }
    if local_only {
        hub.send(&Channel::PublicLocal, "delete", &payload);
        return Ok(());
    }
    if public {
        hub.send(&Channel::Public, "delete", &payload);
        let origin = if local {
            Channel::PublicLocal
        } else {
            Channel::PublicRemote
        };
        hub.send(&origin, "delete", &payload);
        for tag_name in tags {
            hub.send(&Channel::Hashtag(tag_name.clone()), "delete", &payload);
            if local {
                hub.send(&Channel::HashtagLocal(tag_name.clone()), "delete", &payload);
            }
        }
    }
    Ok(())
}

async fn route_notification(
    state: &AppState,
    notification_id: i64,
    account_id: i64,
) -> Result<(), ApiError> {
    let hub = &state.streaming;
    if hub.viewers_of(&Channel::User(account_id)).is_empty()
        && hub
            .viewers_of(&Channel::UserNotification(account_id))
            .is_empty()
    {
        return Ok(());
    }
    let Some(item) = notification::find_by_id(&state.pool, account_id, notification_id).await?
    else {
        return Ok(());
    };
    // Policy-filtered notifications are held for the requests inbox, never
    // pushed live — Mastodon's NotifyService skips streaming for them.
    if item.filtered {
        return Ok(());
    }
    if events::notification_filtered(&state.pool, account_id, item.from_account_id).await? {
        return Ok(());
    }
    let rendered = crate::entities::render_notifications(
        &state.pool,
        &state.config.domain,
        std::slice::from_ref(&item),
        account_id,
    )
    .await?;
    let Some(entity) = rendered.first() else {
        return Ok(());
    };
    hub.send_notification(account_id, &entity.to_string());
    Ok(())
}

async fn route_conversation(
    state: &AppState,
    row_id: i64,
    account_id: i64,
) -> Result<(), ApiError> {
    let hub = &state.streaming;
    if hub.viewers_of(&Channel::User(account_id)).is_empty()
        && hub.viewers_of(&Channel::Direct(account_id)).is_empty()
    {
        return Ok(());
    }
    let Some(row) = conversation::find_for(&state.pool, account_id, row_id).await? else {
        return Ok(());
    };
    let entity =
        crate::entities::render_conversation(&state.pool, &state.config.domain, account_id, &row)
            .await?;
    let payload = entity.to_string();
    hub.send(&Channel::User(account_id), "conversation", &payload);
    hub.send(&Channel::Direct(account_id), "conversation", &payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disconnect_viewer_closes_all_of_only_that_viewers_subscriptions() {
        let hub = Hub::default();
        let (sender, _receiver) = mpsc::channel::<String>(4);
        let close = Arc::new(Notify::new());
        hub.subscribe(
            1,
            Channel::User(7),
            7,
            true,
            sender.clone(),
            Arc::clone(&close),
        );
        hub.subscribe(
            1,
            Channel::Public,
            7,
            false,
            sender.clone(),
            Arc::clone(&close),
        );
        let other_close = Arc::new(Notify::new());
        hub.subscribe(2, Channel::Public, 8, false, sender, other_close);
        let notified = close.notified();
        tokio::pin!(notified);

        hub.disconnect_viewer(7);
        assert_eq!(hub.subscription_count(), 1, "the other viewer remains");
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .expect("every suspended viewer connection is signalled to close");
    }

    /// A subscriber whose bounded queue fills up is dropped from the hub and
    /// its connection is signalled to close, instead of the queue growing.
    #[tokio::test]
    async fn full_queue_drops_the_lagging_subscriber_and_signals_close() {
        let hub = Hub::default();
        // Capacity 1, never drained — the second send has nowhere to go.
        let (sender, _receiver) = mpsc::channel::<String>(1);
        let close = Arc::new(Notify::new());
        let channel = Channel::Public;
        hub.subscribe(1, channel.clone(), 7, false, sender, Arc::clone(&close));
        assert_eq!(hub.subscription_count(), 1);

        // The first event fills the single queue slot; the subscriber stays.
        hub.send(&channel, "update", "a");
        assert_eq!(hub.subscription_count(), 1);

        // The queue is now full: the next event drops the lagging subscriber
        // and fires its close signal.
        hub.send(&channel, "update", "b");
        assert_eq!(hub.subscription_count(), 0);
        tokio::time::timeout(std::time::Duration::from_secs(1), close.notified())
            .await
            .expect("the lagging connection is signalled to close");
    }

    /// A slow (never-draining) reader cannot make the hub accumulate memory in
    /// proportion to the event stream: its outbound queue is hard-bounded by
    /// `CONNECTION_QUEUE_CAP`, and once that fills the subscriber is dropped, so
    /// a flood of further events buffers nothing more. Resident memory pinned by
    /// one lagging consumer is O(queue cap), not O(events) (finding 27).
    #[tokio::test]
    async fn slow_reader_flood_stays_bounded_by_the_queue_cap() {
        // Far more events than the queue can ever hold. Only the events up to
        // the cap can buffer; the first overflow drops the subscriber, so the
        // remaining ~99% of this flood find no subscriber and cost nothing.
        const FLOOD: usize = 100_000;

        let hub = Hub::default();
        // The production per-connection queue size; the reader never drains it.
        let (sender, mut receiver) = mpsc::channel::<String>(CONNECTION_QUEUE_CAP);
        let close = Arc::new(Notify::new());
        let channel = Channel::Public;
        hub.subscribe(1, channel.clone(), 7, false, sender, Arc::clone(&close));

        // Fire the flood; the reader never drains its queue.
        for _ in 0..FLOOD {
            hub.send(&channel, "update", "payload");
        }

        // The lagging subscriber was shed on the first overflow and signalled to
        // close, so no later event could have grown its queue.
        assert_eq!(hub.subscription_count(), 0);
        tokio::time::timeout(std::time::Duration::from_secs(1), close.notified())
            .await
            .expect("the lagging connection is signalled to close");

        // The queue never buffered more than its cap regardless of the flood
        // size — the resident-memory bound this whole design exists to provide.
        let mut buffered = 0;
        while receiver.try_recv().is_ok() {
            buffered += 1;
        }
        assert!(
            buffered <= CONNECTION_QUEUE_CAP,
            "buffered {buffered} exceeds the {CONNECTION_QUEUE_CAP}-event cap",
        );
    }

    /// Connection admission caps live sockets per viewer and globally, and a
    /// dropped guard frees the slot again.
    #[test]
    fn connection_admission_caps_per_viewer_and_frees_on_drop() {
        let hub = Arc::new(Hub::default());
        let viewer = 42;
        let mut guards = Vec::new();
        for _ in 0..Hub::MAX_CONNECTIONS_PER_VIEWER {
            guards.push(
                hub.try_open_connection(viewer)
                    .expect("admitted under the per-viewer cap"),
            );
        }
        // One more for the same viewer is refused...
        assert!(hub.try_open_connection(viewer).is_none());
        // ...but another account is still admitted.
        assert!(hub.try_open_connection(viewer + 1).is_some());
        // Dropping a guard frees a slot for the capped viewer again.
        guards.pop();
        assert!(hub.try_open_connection(viewer).is_some());
    }
}
