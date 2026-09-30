//! Bounded, in-process PeerTube-compatible WebRTC signaling for local videos.
//! Only published VOD/on-air live infohashes are eligible; no media is relayed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::error::ApiError;
use crate::state::AppState;
use crate::sync::RecoverableMutex as _;

const MAX_CONNECTIONS: usize = 2048;
const MAX_SWARMS_PER_SOCKET: usize = 4;
const MAX_PEERS_PER_SWARM: usize = 128;
const MAX_OFFERS: usize = 16;
const MAX_MESSAGE: usize = 64 * 1024;
const MAX_SIGNAL: usize = 16 * 1024;
const OUTBOUND_QUEUE: usize = 8;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const AUTHORITY_TIMEOUT: Duration = Duration::from_secs(2);
const FIRST_ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(15);
// The advertised announce interval is 120 seconds. Pings/pongs are not an
// announce: an idle connection must not reserve admission forever.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(180);

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
static CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static SWARMS: LazyLock<Mutex<HashMap<String, HashMap<String, Peer>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
struct Peer {
    session: u64,
    complete: bool,
    outbound: mpsc::Sender<String>,
}

struct Admission(&'static AtomicUsize);

impl Admission {
    fn try_new() -> Option<Self> {
        Self::try_on(&CONNECTIONS)
    }

    fn try_on(connections: &'static AtomicUsize) -> Option<Self> {
        connections
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| {
                (count < MAX_CONNECTIONS).then_some(count + 1)
            })
            .ok()
            .map(|_| Self(connections))
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub async fn socket(
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let guard = Admission::try_new().ok_or(ApiError::TooManyRequests)?;
    Ok(upgrade
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(MAX_MESSAGE * 2)
        .on_upgrade(move |socket| serve(state, socket, guard)))
}

async fn serve(state: AppState, mut socket: WebSocket, _guard: Admission) {
    let session = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let (outbound, mut incoming) = mpsc::channel::<String>(OUTBOUND_QUEUE);
    let mut recent = Instant::now();
    let mut announces = 0_usize;
    let mut swarms = Vec::<String>::new();
    let mut announce_deadline = tokio::time::Instant::now() + FIRST_ANNOUNCE_TIMEOUT;
    let mut keepalive = tokio::time::interval(Duration::from_secs(45));
    keepalive.tick().await;
    loop {
        tokio::select! {
            message = incoming.recv() => {
                let Some(message) = message else { break };
                match tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Text(message.into()))).await {
                    Ok(Ok(())) => {}, _ => break,
                }
            }
            message = socket.recv() => match message {
                Some(Ok(Message::Text(text))) => {
                    if recent.elapsed() >= Duration::from_secs(60) { recent = Instant::now(); announces = 0; }
                    announces += 1;
                    if announces > 90 { break; }
                    if let Err(reason) = announce(&state, session, &outbound, &mut swarms, text.as_str()).await {
                        let failure = json!({"failure reason": reason}).to_string();
                        let _ = tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Text(failure.into()))).await;
                        break;
                    }
                    announce_deadline = tokio::time::Instant::now() + ANNOUNCE_TIMEOUT;
                }
                Some(Ok(Message::Close(_) | Message::Binary(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = keepalive.tick() => {
                if !matches!(tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(()))) { break; }
            }
            () = tokio::time::sleep_until(announce_deadline) => break,
        }
    }
    let mut registry = SWARMS.lock_or_recover();
    for hash in swarms {
        if let Some(peers) = registry.get_mut(&hash) {
            peers.retain(|_, peer| peer.session != session);
            if peers.is_empty() {
                registry.remove(&hash);
            }
        }
    }
}

fn binary_id(value: Option<&Value>) -> Option<&str> {
    value?
        .as_str()
        .filter(|id| id.chars().count() == 20 && id.chars().all(|c| u32::from(c) <= 255))
}

fn infohash(value: Option<&Value>) -> Option<&str> {
    let hash = value?.as_str()?;
    (hash.len() == 20
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/'))
    .then_some(hash)
}

async fn announce(
    state: &AppState,
    session: u64,
    outbound: &mpsc::Sender<String>,
    swarms: &mut Vec<String>,
    text: &str,
) -> Result<(), &'static str> {
    let request: Value = serde_json::from_str(text).map_err(|_| "invalid announce")?;
    let (hash, peer_id, event) = validate_request(&request)?;
    if !swarms.iter().any(|existing| existing == hash) && swarms.len() >= MAX_SWARMS_PER_SOCKET {
        return Err("too many swarms");
    }
    let allowed = tokio::time::timeout(AUTHORITY_TIMEOUT, async {
        if plamenu_db::native_video_hls::public_infohash_exists(&state.pool, hash)
            .await
            .map_err(|_| "tracker unavailable")?
        {
            return Ok(true);
        }
        plamenu_db::native_live::public_infohash_exists(&state.pool, hash)
            .await
            .map_err(|_| "tracker unavailable")
    })
    .await
    .map_err(|_| "tracker unavailable")??;
    if !allowed {
        return Err("unknown info_hash");
    }
    if !swarms.iter().any(|existing| existing == hash) {
        swarms.push(hash.to_owned());
    }
    let answer = request.get("answer");
    let mut registry = SWARMS.lock_or_recover();
    let peers = registry.entry(hash.to_owned()).or_default();
    if event == "stopped" {
        if peers
            .get(peer_id)
            .is_some_and(|peer| peer.session == session)
        {
            peers.remove(peer_id);
        }
        let _ = outbound.try_send(json!({"action":"announce","info_hash":hash,"interval":120,"complete":0,"incomplete":peers.len()}).to_string());
        return Ok(());
    }
    if peers.len() >= MAX_PEERS_PER_SWARM && !peers.contains_key(peer_id) {
        return Err("swarm full");
    }
    if peers
        .get(peer_id)
        .is_some_and(|peer| peer.session != session)
    {
        return Err("peer_id already in use");
    }
    if peers
        .iter()
        .any(|(id, peer)| id != peer_id && peer.session == session)
    {
        return Err("one peer_id per socket and swarm");
    }
    let complete = request.get("left").and_then(Value::as_u64) == Some(0) || event == "completed";
    peers.insert(
        peer_id.to_owned(),
        Peer {
            session,
            complete,
            outbound: outbound.clone(),
        },
    );
    if answer.is_none() {
        outbound
            .try_send(
                json!({"action":"announce", "info_hash":hash, "interval":120,
            "complete":peers.values().filter(|peer| peer.complete).count(),
            "incomplete":peers.values().filter(|peer| !peer.complete).count()})
                .to_string(),
            )
            .map_err(|_| "outbound queue full")?;
    }
    forward_signals(peers, &request, hash, peer_id);
    Ok(())
}

/// Validate the entire message before database work, peer admission, an ack or
/// any relay. Otherwise a malformed later offer can leak earlier signaling,
/// and malformed unused offers can be accepted when there are no recipients.
fn validate_request(request: &Value) -> Result<(&str, &str, &str), &'static str> {
    if request.get("action").and_then(Value::as_str) != Some("announce") {
        return Err("unsupported tracker action");
    }
    let hash = infohash(request.get("info_hash")).ok_or("invalid info_hash")?;
    let peer_id = binary_id(request.get("peer_id")).ok_or("invalid peer_id")?;
    let event = match request.get("event") {
        None => "update",
        Some(event) => event.as_str().ok_or("invalid event")?,
    };
    if !matches!(
        event,
        "started" | "stopped" | "completed" | "update" | "paused"
    ) {
        return Err("invalid event");
    }
    if let Some(offers) = request.get("offers") {
        let offers = offers.as_array().ok_or("invalid offers")?;
        if offers.len() > MAX_OFFERS {
            return Err("too many offers");
        }
        for offer in offers {
            let signal = offer.get("offer").ok_or("missing offer")?;
            if !signal.is_object() {
                return Err("invalid offer");
            }
            if binary_id(offer.get("offer_id")).is_none() {
                return Err("invalid offer_id");
            }
            if signal.to_string().len() > MAX_SIGNAL {
                return Err("offer too large");
            }
        }
    }
    if let Some(answer) = request.get("answer") {
        if !answer.is_object() {
            return Err("invalid answer");
        }
        if answer.to_string().len() > MAX_SIGNAL {
            return Err("answer too large");
        }
        if binary_id(request.get("to_peer_id")).is_none() {
            return Err("invalid to_peer_id");
        }
        if binary_id(request.get("offer_id")).is_none() {
            return Err("invalid offer_id");
        }
    }
    Ok((hash, peer_id, event))
}

fn forward_signals(peers: &HashMap<String, Peer>, request: &Value, hash: &str, peer_id: &str) {
    let offers = request.get("offers").and_then(Value::as_array);
    if let Some(offers) = offers {
        for (target, offer) in peers
            .iter()
            .filter(|(id, _)| id.as_str() != peer_id)
            .take(offers.len())
            .map(|(_, peer)| peer)
            .zip(offers)
        {
            // Every offer (including unused ones) was validated before admission.
            let signal = &offer["offer"];
            let offer_id = &offer["offer_id"];
            let _ = target.outbound.try_send(
                json!({"action":"announce", "info_hash":hash,
                "peer_id":peer_id, "offer":signal, "offer_id":offer_id})
                .to_string(),
            );
        }
    }
    if let Some(answer) = request.get("answer") {
        let offer_id = &request["offer_id"];
        if let Some(target) = request
            .get("to_peer_id")
            .and_then(Value::as_str)
            .and_then(|to| peers.get(to))
        {
            let _ = target.outbound.try_send(
                json!({"action":"announce", "info_hash":hash,
                "peer_id":peer_id, "answer":answer, "offer_id":offer_id})
                .to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({"action":"announce", "info_hash":"AAAAAAAAAAAAAAAAAAAA",
            "peer_id":"ABCDEFGHIJKLMNOPQRST", "event":"started", "offers":[]})
    }

    #[test]
    fn global_admission_caps_and_reclaims_every_guard() {
        // Same production ceiling/algorithm, separate counter so unrelated
        // concurrent tests or upgrades cannot affect this assertion.
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let mut admitted: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| Admission::try_on(&COUNT).expect("slot below the ceiling"))
            .collect();
        assert_eq!(COUNT.load(Ordering::Acquire), MAX_CONNECTIONS);
        assert!(Admission::try_on(&COUNT).is_none());
        admitted.pop();
        let reclaimed = Admission::try_on(&COUNT).expect("dropped slot must be reusable");
        drop(reclaimed);
        drop(admitted);
        assert_eq!(COUNT.load(Ordering::Acquire), 0);
    }

    #[test]
    fn validates_all_offers_even_without_recipients() {
        let mut message = request();
        message["offers"] = json!([
            {"offer": {"type":"offer", "sdp":"valid"}, "offer_id":"ABCDEFGHIJKLMNOPQRST"},
            {"offer": {"type":"offer", "sdp":"bad"}, "offer_id":"short"}]);
        assert_eq!(validate_request(&message).unwrap_err(), "invalid offer_id");
        message["offers"][1]["offer_id"] = json!("ABCDEFGHIJKLMNOPQRST");
        assert!(validate_request(&message).is_ok());
        message["offers"][1]["offer"]["sdp"] = json!("a".repeat(MAX_SIGNAL));
        assert_eq!(validate_request(&message).unwrap_err(), "offer too large");
    }

    #[test]
    fn accepts_wire_binary_ids_but_not_unicode_or_malformed_signals() {
        let mut message = request();
        message["peer_id"] = json!("\u{00ff}".repeat(20));
        assert!(validate_request(&message).is_ok());
        message["peer_id"] = json!("\u{0100}".repeat(20));
        assert_eq!(validate_request(&message).unwrap_err(), "invalid peer_id");
        message = request();
        message["offers"] = json!({});
        assert_eq!(validate_request(&message).unwrap_err(), "invalid offers");
        message["offers"] = json!([]);
        message["answer"] = Value::Null;
        assert_eq!(validate_request(&message).unwrap_err(), "invalid answer");
        message["answer"] = json!({"type":"answer", "sdp":"valid"});
        assert_eq!(
            validate_request(&message).unwrap_err(),
            "invalid to_peer_id"
        );
        message["to_peer_id"] = json!("ABCDEFGHIJKLMNOPQRST");
        message["offer_id"] = json!("ABCDEFGHIJKLMNOPQRST");
        assert!(validate_request(&message).is_ok());
        message["answer"]["sdp"] = json!("a".repeat(MAX_SIGNAL));
        assert_eq!(validate_request(&message).unwrap_err(), "answer too large");
    }

    #[test]
    fn whole_offer_limit_and_event_are_validated() {
        let mut message = request();
        let offer = json!({"offer":{}, "offer_id":"ABCDEFGHIJKLMNOPQRST"});
        message["offers"] = json!(vec![offer.clone(); MAX_OFFERS]);
        assert!(validate_request(&message).is_ok());
        message["offers"] = json!(vec![offer; MAX_OFFERS + 1]);
        assert_eq!(validate_request(&message).unwrap_err(), "too many offers");
        message["offers"] = json!([]);
        message["event"] = Value::Null;
        assert_eq!(validate_request(&message).unwrap_err(), "invalid event");
        message.as_object_mut().unwrap().remove("event");
        assert_eq!(validate_request(&message).unwrap().2, "update");
    }

    #[tokio::test]
    async fn signaling_stays_within_swarm_and_drops_backpressure() {
        let (outbound, mut receiver) = mpsc::channel(1);
        let peers = HashMap::from([(
            "ABCDEFGHIJKLMNOPQRST".to_owned(),
            Peer {
                session: 1,
                complete: false,
                outbound,
            },
        )]);
        let message = json!({"answer":{"type":"answer", "sdp":"valid"},
            "to_peer_id":"unknown-peer-id------", "offer_id":"ABCDEFGHIJKLMNOPQRST"});
        forward_signals(
            &peers,
            &message,
            "AAAAAAAAAAAAAAAAAAAA",
            "QRSTUVWXYZABCDEFGHIJ",
        );
        assert!(receiver.try_recv().is_err());
        let mut message = message;
        message["to_peer_id"] = json!("ABCDEFGHIJKLMNOPQRST");
        forward_signals(
            &peers,
            &message,
            "AAAAAAAAAAAAAAAAAAAA",
            "QRSTUVWXYZABCDEFGHIJ",
        );
        forward_signals(
            &peers,
            &message,
            "AAAAAAAAAAAAAAAAAAAA",
            "QRSTUVWXYZABCDEFGHIJ",
        );
        assert_eq!(receiver.len(), 1, "full queues must not grow or block");
        let relayed: Value = serde_json::from_str(&receiver.recv().await.unwrap()).unwrap();
        assert_eq!(relayed["peer_id"], "QRSTUVWXYZABCDEFGHIJ");
        assert_eq!(relayed["info_hash"], "AAAAAAAAAAAAAAAAAAAA");
    }
}
