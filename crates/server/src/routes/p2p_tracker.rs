//! Bounded, in-process PeerTube-compatible WebRTC signaling for local VOD.
//! Only published native-video infohashes are eligible; no media is relayed.

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

struct Admission;

impl Admission {
    fn try_new() -> Option<Self> {
        CONNECTIONS
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| {
                (count < MAX_CONNECTIONS).then_some(count + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
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
    let mut keepalive = tokio::time::interval(Duration::from_secs(45));
    keepalive.tick().await;
    loop {
        tokio::select! {
            message = incoming.recv() => {
                let Some(message) = message else { break };
                match tokio::time::timeout(Duration::from_secs(5), socket.send(Message::Text(message.into()))).await {
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
                        let _ = socket.send(Message::Text(failure.into())).await;
                        break;
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = keepalive.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
            }
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
    if request.get("action").and_then(Value::as_str) != Some("announce") {
        return Err("unsupported tracker action");
    }
    let hash = infohash(request.get("info_hash")).ok_or("invalid info_hash")?;
    let peer_id = binary_id(request.get("peer_id")).ok_or("invalid peer_id")?;
    if !swarms.iter().any(|existing| existing == hash) {
        if swarms.len() >= MAX_SWARMS_PER_SOCKET {
            return Err("too many swarms");
        }
        swarms.push(hash.to_owned());
    }
    let vod_allowed = plamenu_db::native_video_hls::public_infohash_exists(&state.pool, hash)
        .await
        .map_err(|_| "tracker unavailable")?;
    let live_allowed = if vod_allowed {
        false
    } else {
        plamenu_db::native_live::public_infohash_exists(&state.pool, hash)
            .await
            .map_err(|_| "tracker unavailable")?
    };
    if !vod_allowed && !live_allowed {
        return Err("unknown info_hash");
    }
    let event = request
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("update");
    if !matches!(
        event,
        "started" | "stopped" | "completed" | "update" | "paused"
    ) {
        return Err("invalid event");
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
    forward_signals(peers, &request, hash, peer_id)?;
    Ok(())
}

fn forward_signals(
    peers: &HashMap<String, Peer>,
    request: &Value,
    hash: &str,
    peer_id: &str,
) -> Result<(), &'static str> {
    let offers = request.get("offers").and_then(Value::as_array);
    if offers.is_some_and(|items| items.len() > MAX_OFFERS) {
        return Err("too many offers");
    }
    if let Some(offers) = offers {
        for (target, offer) in peers
            .iter()
            .filter(|(id, _)| id.as_str() != peer_id)
            .take(offers.len())
            .map(|(_, peer)| peer)
            .zip(offers)
        {
            let signal = offer.get("offer").ok_or("missing offer")?;
            let offer_id = binary_id(offer.get("offer_id")).ok_or("invalid offer_id")?;
            if signal.to_string().len() > MAX_SIGNAL {
                return Err("offer too large");
            }
            let _ = target.outbound.try_send(
                json!({"action":"announce", "info_hash":hash,
                "peer_id":peer_id, "offer":signal, "offer_id":offer_id})
                .to_string(),
            );
        }
    }
    if let Some(answer) = request.get("answer") {
        if answer.to_string().len() > MAX_SIGNAL {
            return Err("answer too large");
        }
        let to = binary_id(request.get("to_peer_id")).ok_or("invalid to_peer_id")?;
        let offer_id = binary_id(request.get("offer_id")).ok_or("invalid offer_id")?;
        if let Some(target) = peers.get(to) {
            let _ = target.outbound.try_send(
                json!({"action":"announce", "info_hash":hash,
                "peer_id":peer_id, "answer":answer, "offer_id":offer_id})
                .to_string(),
            );
        }
    }
    Ok(())
}
