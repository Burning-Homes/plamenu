//! Ephemeral FEP-752d transport. Only control metadata is read from Postgres;
//! packets, duplicate suppression and connected-client queues live in memory.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use base64::Engine as _;
use plamenu_db::{account::Account, id};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::federation::Delivery;
use crate::sync::RecoverableMutex as _;
use crate::{AppState, webxdc::PROTOCOL};

pub const MAX_BYTES: usize = 128_000;
const TTL: Duration = Duration::from_secs(2);
const PACKET_LIFETIME: i64 = 5;
const MAX_CONNECTIONS: usize = 1024;
const MAX_CACHE: usize = 1024;
const MAX_SEEN: usize = 8192;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Peer {
    Account(i64),
    Guest(i64),
}

struct Frame {
    bytes: Vec<u8>,
    expires: OffsetDateTime,
}

struct Client {
    session: i64,
    peer: Peer,
    // One queued packet per connection; slow consumers lose traffic.
    sender: mpsc::Sender<Arc<Frame>>,
    cancelled: CancellationToken,
}

#[derive(Clone)]
struct Member {
    peer: Peer,
    uri: String,
    inbox: Option<String>,
}

struct Route {
    id: i64,
    account_id: i64,
    uri: String,
    local: bool,
    inbox: String,
    members: Vec<Member>,
}

#[derive(Default)]
struct Memory {
    clients: HashMap<i64, Client>,
    routes: HashMap<String, (Instant, Arc<Route>)>,
    seen: HashMap<(String, String), Instant>,
    rates: HashMap<(i64, Peer), (Instant, usize, usize)>,
    generation: u64,
    inflight: HashMap<i64, (i64, CancellationToken)>,
}

pub struct Hub {
    memory: Mutex<Memory>,
    // Admission precedes spawning; overload cannot grow detached tasks.
    deliveries: Arc<Semaphore>,
    signing_keys:
        tokio::sync::Mutex<HashMap<i64, (Instant, Arc<crate::key_store::DecryptedActorKey>)>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            memory: Mutex::default(),
            deliveries: Arc::new(Semaphore::new(64)),
            signing_keys: tokio::sync::Mutex::default(),
        }
    }
}

// Drop guards also release registries when an upgraded socket/task is aborted.
struct Registration {
    hub: Arc<Hub>,
    id: i64,
    delivery: bool,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut memory = self.hub.memory.lock_or_recover();
        if self.delivery {
            memory.inflight.remove(&self.id);
        } else {
            memory.clients.remove(&self.id);
        }
    }
}

impl Hub {
    pub fn refresh(&self, session: i64) {
        let mut memory = self.memory.lock_or_recover();
        memory.generation = memory.generation.wrapping_add(1);
        memory.routes.retain(|_, (_, route)| route.id != session);
    }

    async fn signing_key(
        &self,
        state: &AppState,
        account_id: i64,
    ) -> Option<Arc<crate::key_store::DecryptedActorKey>> {
        let mut keys = self.signing_keys.lock().await;
        keys.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(30));
        if let Some((_, key)) = keys.get(&account_id) {
            return Some(Arc::clone(key));
        }
        let key = Arc::new(
            crate::key_store::account_signing_key(state, account_id, "rsa")
                .await
                .ok()?,
        );
        if keys.len() < MAX_CACHE {
            keys.insert(account_id, (Instant::now(), Arc::clone(&key)));
        }
        Some(key)
    }

    /// Lifecycle changes discard cached authorization and close affected
    /// sockets. A database refresh also catches changes from other processes.
    pub fn invalidate(&self, session: i64, peer: Option<Peer>) {
        let mut memory = self.memory.lock_or_recover();
        memory.generation = memory.generation.wrapping_add(1);
        memory.routes.retain(|_, (_, route)| route.id != session);
        for (packet_session, cancelled) in memory.inflight.values() {
            if *packet_session == session {
                cancelled.cancel();
            }
        }
        for client in memory.clients.values() {
            if client.session == session && peer.is_none_or(|peer| peer == client.peer) {
                client.cancelled.cancel();
            }
        }
    }

    fn publish(&self, route: &Route, origin: &str, bytes: Vec<u8>, expires: OffsetDateTime) {
        if expires <= OffsetDateTime::now_utc() {
            return;
        }
        let bytes = Arc::new(Frame { bytes, expires });
        let memory = self.memory.lock_or_recover();
        for client in memory.clients.values() {
            if client.session == route.id
                && !client.cancelled.is_cancelled()
                && route
                    .members
                    .iter()
                    .any(|m| m.peer == client.peer && m.uri != origin)
            {
                let _ = client.sender.try_send(Arc::clone(&bytes));
            }
        }
    }

    fn first_seen(&self, session: &str, packet_id: &str) -> bool {
        let mut memory = self.memory.lock_or_recover();
        memory
            .seen
            .retain(|_, at| at.elapsed() < Duration::from_secs(10));
        let key = (session.to_owned(), packet_id.to_owned());
        if memory.seen.contains_key(&key) || memory.seen.len() >= MAX_SEEN {
            return false;
        }
        memory.seen.insert(key, Instant::now());
        true
    }

    fn allow_send(&self, session: i64, peer: Peer, bytes: usize) -> bool {
        let mut memory = self.memory.lock_or_recover();
        memory
            .rates
            .retain(|_, (at, _, _)| at.elapsed() < Duration::from_secs(1));
        if memory.rates.len() >= MAX_CACHE && !memory.rates.contains_key(&(session, peer)) {
            return false;
        }
        let (_, count, total) =
            memory
                .rates
                .entry((session, peer))
                .or_insert((Instant::now(), 0, 0));
        *count += 1;
        *total += bytes;
        *count <= 30 && *total <= 512_000
    }
}

async fn route(state: &AppState, uri: &str) -> Result<Arc<Route>, ApiError> {
    let generation = {
        let memory = state.webxdc_realtime.memory.lock_or_recover();
        if let Some((at, route)) = memory.routes.get(uri)
            && at.elapsed() < TTL
        {
            return Ok(Arc::clone(route));
        }
        memory.generation
    };
    // Do not fetch the bundle or durable history on the packet path.
    let (session_id, account_id, domain, inbox): (i64, i64, Option<String>, String) =
        sqlx::query_as("SELECT s.id, s.account_id, a.domain, a.inbox_url FROM webxdc_sessions s JOIN accounts a ON a.id = s.account_id WHERE s.coordinator_uri = $1 AND s.ended_at IS NULL AND a.suspended_at IS NULL")
            .bind(uri).fetch_optional(&state.pool).await.map_err(plamenu_db::DbError::from)?
            .ok_or(ApiError::Gone)?;
    let rows: Vec<(i64, String, Option<String>, String)> = sqlx::query_as(
        "SELECT m.participant_account_id, m.participant_uri, a.domain, a.inbox_url FROM webxdc_memberships m JOIN accounts a ON a.id = m.participant_account_id WHERE m.session_id = $1 AND m.accepted AND a.suspended_at IS NULL")
        .bind(session_id).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let mut members = Vec::new();
    for (id, uri, domain, inbox) in rows {
        if domain.is_some()
            && !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &uri)
                .await?
        {
            continue;
        }
        let inbox = if domain.is_some()
            && !inbox.is_empty()
            && crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &inbox)
                .await?
        {
            Some(inbox)
        } else {
            None
        };
        members.push(Member {
            peer: Peer::Account(id),
            uri,
            inbox,
        });
    }
    if domain.is_none() {
        let guests: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, participant_uri FROM webxdc_guests WHERE session_id = $1 AND accepted",
        )
        .bind(session_id)
        .fetch_all(&state.pool)
        .await
        .map_err(plamenu_db::DbError::from)?;
        members.extend(guests.into_iter().map(|(id, uri)| Member {
            peer: Peer::Guest(id),
            uri,
            inbox: None,
        }));
    } else if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &inbox)
        .await?
    {
        return Err(ApiError::Forbidden("session coordinator is blocked".into()));
    }
    let route = Arc::new(Route {
        id: session_id,
        account_id,
        uri: uri.to_owned(),
        local: domain.is_none(),
        inbox,
        members,
    });
    let mut memory = state.webxdc_realtime.memory.lock_or_recover();
    // Fail closed if a lifecycle transition raced this database snapshot.
    if memory.generation != generation {
        return Err(ApiError::Gone);
    }
    memory.routes.retain(|_, (at, _)| at.elapsed() < TTL);
    if memory.routes.len() < MAX_CACHE {
        memory
            .routes
            .insert(uri.to_owned(), (Instant::now(), Arc::clone(&route)));
    }
    Ok(route)
}

#[must_use]
pub fn is_packet(raw: &Value) -> bool {
    let object = if raw["type"] == "Announce" {
        &raw["object"]["object"]
    } else {
        &raw["object"]
    };
    object["type"] == "WebxdcEphemeral"
}

fn timestamp(value: &Value) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value.as_str()?, &Rfc3339).ok()
}

fn parse_packet<'a>(
    raw: &'a Value,
    session: &str,
) -> Result<(&'a str, &'a str, Vec<u8>), ApiError> {
    let invalid = || ApiError::BadRequest("invalid Webxdc ephemeral packet".into());
    let object = &raw["object"];
    let actor = raw["actor"].as_str().ok_or_else(invalid)?;
    let packet_id = raw["id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 2048)
        .ok_or_else(invalid)?;
    let published = timestamp(&raw["published"]).ok_or_else(invalid)?;
    let end = timestamp(&raw["endTime"]).ok_or_else(invalid)?;
    let now = OffsetDateTime::now_utc();
    if raw["type"] != "Create"
        || raw["webxdcProtocol"] != PROTOCOL
        || raw["to"] != session
        || raw["context"] != session
        || raw["audience"] != session
        || raw.get("webxdcSerial").is_some()
        || object["type"] != "WebxdcEphemeral"
        || object["attributedTo"] != actor
        || object["context"] != session
        || object["id"].as_str().is_none_or(str::is_empty)
        || end <= now
        || published > now + time::Duration::seconds(1)
        || end <= published
        || end - published > time::Duration::seconds(PACKET_LIFETIME)
    {
        return Err(invalid());
    }
    let encoded = object["webxdcData"].as_str().ok_or_else(invalid)?;
    if encoded.len() > MAX_BYTES.div_ceil(3) * 4 {
        return Err(ApiError::PayloadTooLarge);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| invalid())?;
    if bytes.len() > MAX_BYTES {
        return Err(ApiError::PayloadTooLarge);
    }
    Ok((actor, packet_id, bytes))
}

#[must_use]
pub fn packet(session: &str, actor: &str, bytes: &[u8]) -> Value {
    let now = OffsetDateTime::now_utc();
    let marker = id::next();
    json!({
        "@context": [plamenu_ap::AS_CONTEXT, PROTOCOL],
        "id": format!("{actor}/ephemeral/{marker}"), "type": "Create", "actor": actor,
        "to": session, "context": session, "audience": session, "webxdcProtocol": PROTOCOL,
        "published": now.format(&Rfc3339).unwrap_or_default(),
        "endTime": (now + time::Duration::seconds(PACKET_LIFETIME)).format(&Rfc3339).unwrap_or_default(),
        "object": { "id": format!("{actor}/ephemeral/{marker}#packet"), "type": "WebxdcEphemeral",
            "attributedTo": actor, "context": session,
            "webxdcData": base64::engine::general_purpose::STANDARD.encode(bytes) }
    })
}

// No retry queue, reachability records, signature preference writes or object
// proofs. One ordinary HTTP-signed attempt, cancelled at the packet deadline.
fn deliver(
    state: &AppState,
    session_id: i64,
    account_id: i64,
    inbox: String,
    activity: Value,
    expires: OffsetDateTime,
) {
    let Ok(permit) = Arc::clone(&state.webxdc_realtime.deliveries).try_acquire_owned() else {
        return;
    };
    let state = state.clone();
    let cancelled = CancellationToken::new();
    let delivery_id = id::next();
    state
        .webxdc_realtime
        .memory
        .lock_or_recover()
        .inflight
        .insert(delivery_id, (session_id, cancelled.clone()));
    let registration = Registration {
        hub: Arc::clone(&state.webxdc_realtime),
        id: delivery_id,
        delivery: true,
    };
    tokio::spawn(async move {
        let _registration = registration;
        let _permit = permit;
        let remaining = (expires - OffsetDateTime::now_utc())
            .try_into()
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return;
        }
        let attempt = tokio::time::timeout(remaining, async {
            let Some(key) = state.webxdc_realtime.signing_key(&state, account_id).await else {
                return;
            };
            let Ok(private) = key.private.expose_str() else {
                return;
            };
            let _ = state
                .federation
                .deliver(Delivery {
                    activity,
                    inbox_url: inbox,
                    headers: Vec::new(),
                    private_key_pem: zeroize::Zeroizing::new(private.to_owned()),
                    key_id: key.record.key_uri.clone(),
                    try_rfc9421: false,
                })
                .await;
        });
        tokio::select! {
            () = cancelled.cancelled() => {},
            () = state.shutdown.cancelled() => {},
            _ = attempt => {},
        }
    });
}

fn redistribute(state: &AppState, route: &Route, raw: &Value, origin: &str, bytes: Vec<u8>) {
    state.webxdc_realtime.publish(
        route,
        origin,
        bytes,
        timestamp(&raw["endTime"]).unwrap_or(OffsetDateTime::UNIX_EPOCH),
    );
    let announce = json!({
        "@context": [plamenu_ap::AS_CONTEXT, PROTOCOL],
        "id": format!("{}/ephemeral/{}", route.uri, id::next()),
        "type": "Announce", "actor": route.uri, "object": raw,
        "webxdcProtocol": PROTOCOL, "audience": route.uri,
        "to": format!("{}/followers", route.uri),
        "published": raw["published"], "endTime": raw["endTime"],
    });
    let expires = timestamp(&raw["endTime"]).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let inboxes: HashSet<_> = route
        .members
        .iter()
        .filter_map(|m| m.inbox.clone())
        .collect();
    for inbox in inboxes {
        deliver(
            state,
            route.id,
            route.account_id,
            inbox,
            announce.clone(),
            expires,
        );
    }
}

pub async fn handle(state: &AppState, sender: &Account, raw: &Value) -> Result<bool, ApiError> {
    if !is_packet(raw) {
        return Ok(false);
    }
    if sender.suspended() {
        return Ok(true);
    }
    let announced = raw["type"] == "Announce";
    let inner = if announced { &raw["object"] } else { raw };
    let session = inner["context"]
        .as_str()
        .ok_or_else(|| ApiError::BadRequest("missing session".into()))?;
    let (origin, packet_id, bytes) = parse_packet(inner, session)?;
    let route = match route(state, session).await {
        Ok(route) => route,
        Err(ApiError::Gone) => return Ok(true),
        Err(error) => return Err(error),
    };
    if announced {
        if route.local
            || sender.id != route.account_id
            || raw["actor"] != route.uri
            || raw["audience"] != route.uri
            || raw["webxdcProtocol"] != PROTOCOL
            || raw["to"] != format!("{}/followers", route.uri)
            || raw["endTime"] != inner["endTime"]
            || raw["published"] != inner["published"]
            || raw.get("webxdcSerial").is_some()
            || raw["id"].as_str().is_none_or(str::is_empty)
        {
            return Err(ApiError::Forbidden("wrong ephemeral coordinator".into()));
        }
    } else if !route.local
        || sender.uri.as_deref() != Some(origin)
        || !route
            .members
            .iter()
            .any(|m| m.peer == Peer::Account(sender.id) && m.uri == origin)
    {
        return Err(ApiError::Forbidden(
            "not an active Webxdc participant".into(),
        ));
    }
    if !state
        .webxdc_realtime
        .allow_send(route.id, Peer::Account(sender.id), bytes.len())
        || !state.webxdc_realtime.first_seen(session, packet_id)
    {
        return Ok(true);
    }
    if announced {
        state.webxdc_realtime.publish(
            &route,
            origin,
            bytes,
            timestamp(&inner["endTime"]).unwrap_or(OffsetDateTime::UNIX_EPOCH),
        );
    } else {
        redistribute(state, &route, raw, origin, bytes);
    }
    Ok(true)
}

pub async fn submit(
    state: &AppState,
    session: &str,
    peer: Peer,
    bytes: Vec<u8>,
) -> Result<(), ApiError> {
    if bytes.len() > MAX_BYTES {
        return Err(ApiError::PayloadTooLarge);
    }
    let route = route(state, session).await?;
    let member = route
        .members
        .iter()
        .find(|m| m.peer == peer)
        .ok_or_else(|| ApiError::Forbidden("not an active Webxdc participant".into()))?;
    if !state
        .webxdc_realtime
        .allow_send(route.id, peer, bytes.len())
    {
        return Ok(());
    }
    let raw = packet(session, &member.uri, &bytes);
    if route.local {
        redistribute(state, &route, &raw, &member.uri, bytes);
    } else if let Peer::Account(account_id) = peer {
        deliver(
            state,
            route.id,
            account_id,
            route.inbox.clone(),
            raw.clone(),
            timestamp(&raw["endTime"]).unwrap(),
        );
    }
    Ok(())
}

pub async fn socket(state: AppState, session: String, peer: Peer, mut socket: WebSocket) {
    let Ok(route) = route(&state, &session).await else {
        close_socket(&mut socket).await;
        return;
    };
    if !route.members.iter().any(|m| m.peer == peer) {
        close_socket(&mut socket).await;
        return;
    }
    let connection_id = id::next();
    let (sender, mut receiver) = mpsc::channel(1);
    let cancelled = CancellationToken::new();
    {
        let mut memory = state.webxdc_realtime.memory.lock_or_recover();
        if memory.clients.len() >= MAX_CONNECTIONS
            || memory
                .clients
                .values()
                .filter(|c| c.session == route.id && c.peer == peer)
                .count()
                >= 8
        {
            return;
        }
        memory.clients.insert(
            connection_id,
            Client {
                session: route.id,
                peer,
                sender,
                cancelled: cancelled.clone(),
            },
        );
    }
    let _registration = Registration {
        hub: Arc::clone(&state.webxdc_realtime),
        id: connection_id,
        delivery: false,
    };
    let mut check = tokio::time::interval(TTL);
    let mut last_seen = Instant::now();
    let mut last_ping = Instant::now();
    loop {
        tokio::select! {
            () = cancelled.cancelled() => break,
            () = state.shutdown.cancelled() => break,
            _ = check.tick() => {
                let Ok(current) = self::route(&state, &session).await else { break; };
                if !current.members.iter().any(|m| m.peer == peer) || last_seen.elapsed() > Duration::from_secs(60) { break; }
                if last_ping.elapsed() >= Duration::from_secs(15) {
                    if !matches!(tokio::time::timeout(Duration::from_secs(1), socket.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(()))) { break; }
                    last_ping = Instant::now();
                }
            }
            outgoing = receiver.recv() => {
                let Some(bytes) = outgoing else { break; };
                if cancelled.is_cancelled() { break; }
                let remaining: Duration = (bytes.expires - OffsetDateTime::now_utc()).try_into().unwrap_or(Duration::ZERO);
                if remaining.is_zero() { continue; }
                if !matches!(tokio::time::timeout(remaining.min(Duration::from_secs(1)), socket.send(Message::Binary(bytes.bytes.clone().into()))).await, Ok(Ok(()))) { break; }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(bytes))) => {
                    last_seen = Instant::now();
                    if cancelled.is_cancelled() || submit(&state, &session, peer, bytes.to_vec()).await.is_err() { break; }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                _ => { last_seen = Instant::now(); }
            }
        }
    }
    state
        .webxdc_realtime
        .memory
        .lock_or_recover()
        .clients
        .remove(&connection_id);
    close_socket(&mut socket).await;
}

async fn close_socket(socket: &mut WebSocket) {
    let close = Message::Close(Some(axum::extract::ws::CloseFrame {
        code: 1000,
        reason: "Webxdc channel closed".into(),
    }));
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.send(close)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "https://host.example/webxdc/1";
    const ACTOR: &str = "https://peer.example/users/a";

    #[test]
    fn binary_packet_limits_and_envelope_validation() {
        for bytes in [Vec::new(), vec![0, 1, 127, 128, 255], vec![255; MAX_BYTES]] {
            let raw = packet(SESSION, ACTOR, &bytes);
            assert_eq!(parse_packet(&raw, SESSION).unwrap().2, bytes);
            assert!(parse_packet(&raw, "https://host.example/webxdc/2").is_err());
        }
        assert!(parse_packet(&packet(SESSION, ACTOR, &vec![0; MAX_BYTES + 1]), SESSION).is_err());
        for (field, value) in [
            ("webxdcData", json!("not base64!")),
            ("webxdcData", json!("AA")),
            ("attributedTo", json!(SESSION)),
            ("type", json!("WebxdcUpdate")),
        ] {
            let mut raw = packet(SESSION, ACTOR, &[1]);
            raw["object"][field] = value;
            assert!(parse_packet(&raw, SESSION).is_err());
        }
        let mut expired = packet(SESSION, ACTOR, &[1]);
        expired["endTime"] = json!(
            (OffsetDateTime::now_utc() - time::Duration::seconds(1))
                .format(&Rfc3339)
                .unwrap()
        );
        assert!(parse_packet(&expired, SESSION).is_err());
        let mut future = packet(SESSION, ACTOR, &[1]);
        future["published"] = json!(
            (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                .format(&Rfc3339)
                .unwrap()
        );
        assert!(parse_packet(&future, SESSION).is_err());
        let mut too_long = packet(SESSION, ACTOR, &[1]);
        too_long["endTime"] = json!(
            (OffsetDateTime::now_utc() + time::Duration::seconds(10))
                .format(&Rfc3339)
                .unwrap()
        );
        assert!(parse_packet(&too_long, SESSION).is_err());
        let mut serial = packet(SESSION, ACTOR, &[1]);
        serial["webxdcSerial"] = json!(1);
        assert!(parse_packet(&serial, SESSION).is_err());
    }

    #[tokio::test]
    async fn live_fanout_has_no_replay_echo_or_cross_session_delivery() {
        let hub = Hub::default();
        let route = Route {
            id: 1,
            account_id: 10,
            uri: SESSION.into(),
            local: true,
            inbox: String::new(),
            members: vec![
                Member {
                    peer: Peer::Account(1),
                    uri: ACTOR.into(),
                    inbox: None,
                },
                Member {
                    peer: Peer::Account(2),
                    uri: "https://peer.example/users/b".into(),
                    inbox: None,
                },
            ],
        };
        let expires = OffsetDateTime::now_utc() + time::Duration::seconds(5);
        hub.publish(&route, ACTOR, vec![0], expires); // nobody connected
        let mut receivers = Vec::new();
        for (id, session, peer) in [(1, 1, 1), (2, 1, 2), (3, 2, 2)] {
            let (sender, receiver) = mpsc::channel(1);
            hub.memory.lock_or_recover().clients.insert(
                id,
                Client {
                    session,
                    peer: Peer::Account(peer),
                    sender,
                    cancelled: CancellationToken::new(),
                },
            );
            receivers.push(receiver);
        }
        assert!(receivers[1].try_recv().is_err());
        hub.publish(&route, ACTOR, vec![1], expires);
        hub.publish(&route, ACTOR, vec![2], expires); // full queue drops
        assert_eq!(receivers[1].try_recv().unwrap().bytes, [1]);
        assert!(receivers.iter_mut().all(|rx| rx.try_recv().is_err()));
        hub.invalidate(1, Some(Peer::Account(2)));
        hub.publish(&route, ACTOR, vec![3], expires);
        assert!(receivers[1].try_recv().is_err());
        assert!(hub.first_seen(SESSION, "one"));
        assert!(!hub.first_seen(SESSION, "one"));
        assert!(hub.first_seen("another-session", "one"));
    }
}
