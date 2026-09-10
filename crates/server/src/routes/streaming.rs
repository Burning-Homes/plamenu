//! `/api/v1/streaming` — the websocket streaming API, speaking Mastodon's
//! streaming protocol: authenticate at the upgrade, subscribe via the
//! `stream` query parameter or `{"type":"subscribe",...}` messages, and
//! receive `{"stream":[...],"event":...,"payload":...}` messages.
//!
//! Like Mastodon (4.2+), every connection requires a valid access token —
//! the public streams too. The token arrives as an `Authorization` header,
//! an `access_token` query parameter, or the websocket subprotocol (which
//! must be echoed back for browser clients).

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::response::Response;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use crate::auth::{self, CurrentUser};
use crate::error::ApiError;
use crate::state::AppState;
use crate::streaming::{CONNECTION_QUEUE_CAP, Channel, ConnectionGuard, Hub};

/// How often the server pings, mirroring Mastodon's streaming server.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a single frame write may block on a slow reader before the
/// connection is dropped. Together with the bounded queue this bounds how
/// long a lagging consumer can pin its buffered messages.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Inbound size limits. Subscription commands are a few hundred bytes, so the
/// Tungstenite defaults (64 MiB message, 16 MiB frame, unbounded write
/// buffer) are far larger than this protocol ever needs.
const MAX_MESSAGE_SIZE: usize = 64 * 1024;
const MAX_FRAME_SIZE: usize = 64 * 1024;
const MAX_WRITE_BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Deserialize)]
pub struct StreamingQuery {
    access_token: Option<String>,
    stream: Option<String>,
    tag: Option<String>,
    list: Option<String>,
}

/// `GET /api/v1/streaming/health` — liveness, no authentication.
pub async fn health() -> &'static str {
    "OK"
}

/// `GET /api/v1/streaming` — the websocket upgrade.
pub async fn streaming(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(query): Query<StreamingQuery>,
) -> Result<Response, ApiError> {
    let header_token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let protocol_token = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    // The built-in web client cannot read its HttpOnly session token (by
    // design), but a same-origin WebSocket carries that cookie automatically.
    // Validate Origin before treating ambient cookie authority as streaming
    // authentication; explicit bearer/query/subprotocol clients remain
    // governed by their token alone.
    let session_token = crate::web::session::raw_session_token(&headers)
        .filter(|_| crate::auth::same_origin_request(&headers, &state.config.domain));
    let token = header_token
        .or(query.access_token.as_deref())
        .or(protocol_token)
        .or(session_token)
        .ok_or_else(|| ApiError::Unauthorized("Missing access token".into()))?;
    let current = auth::user_for_token(&state, token).await?;

    // Admit the connection against the per-account/global ceiling before
    // upgrading, so an abusive account cannot open unbounded (slow) sockets.
    let guard = state
        .streaming
        .try_open_connection(current.account.id)
        .ok_or(ApiError::TooManyRequests)?;

    // Bound inbound frame/message sizes and the write buffer; the Tungstenite
    // defaults are far larger than this protocol needs.
    let ws = ws
        .max_message_size(MAX_MESSAGE_SIZE)
        .max_frame_size(MAX_FRAME_SIZE)
        .max_write_buffer_size(MAX_WRITE_BUFFER_SIZE);

    // A client that offered the token as the subprotocol requires it
    // selected in the response, or the browser drops the connection.
    let ws = match protocol_token {
        Some(protocol) => ws.protocols([protocol.to_owned()]),
        None => ws,
    };
    Ok(ws.on_upgrade(move |socket| run_connection(state, socket, current, query, guard)))
}

/// A subscription failure, reported to the client as Mastodon does:
/// `{"error": ..., "status": ...}` over the open socket.
struct StreamError {
    message: &'static str,
    status: u16,
}

impl StreamError {
    const fn new(message: &'static str, status: u16) -> Self {
        Self { message, status }
    }
}

/// Resolves a stream name (+ params) to a channel, enforcing Mastodon's
/// scope rules: `read` grants everything; otherwise `read:statuses` for
/// status streams and `read:notifications` for the notification stream.
/// A `list` stream additionally requires the list to be the caller's own.
/// Returns the channel and whether a `user` stream carries notifications.
async fn resolve_channel(
    state: &AppState,
    current: &CurrentUser,
    stream: &str,
    tag: Option<&str>,
    list: Option<&str>,
) -> Result<(Channel, bool), StreamError> {
    let scopes: Vec<&str> = current.scopes.split_whitespace().collect();
    let specific = if stream == "user:notification" {
        "read:notifications"
    } else {
        "read:statuses"
    };
    if !scopes.contains(&"read") && !scopes.contains(&specific) {
        return Err(StreamError::new(
            "Access token does not have the required scopes",
            401,
        ));
    }
    let me = current.account.id;
    let notifications = scopes.contains(&"read") || scopes.contains(&"read:notifications");
    let hashtag = || -> Result<String, StreamError> {
        tag.map(|t| t.trim_start_matches('#').to_lowercase())
            .filter(|t| !t.is_empty())
            .ok_or(StreamError::new("Missing tag name parameter", 400))
    };
    let channel = match stream {
        "user" => Channel::User(me),
        "user:notification" => Channel::UserNotification(me),
        "public" => Channel::Public,
        "public:local" => Channel::PublicLocal,
        "public:remote" => Channel::PublicRemote,
        "hashtag" => Channel::Hashtag(hashtag()?),
        "hashtag:local" => Channel::HashtagLocal(hashtag()?),
        "direct" => Channel::Direct(me),
        "list" => {
            let param = list.ok_or(StreamError::new("Missing list name parameter", 400))?;
            // A malformed id, an unknown list and someone else's list all
            // answer the same way, like Mastodon's `authorizeListAccess`.
            let not_authorized = || StreamError::new("Not authorized to stream this list", 401);
            let list_id: i64 = param.parse().map_err(|_| not_authorized())?;
            let owned = plamenu_db::list::find_owned(&state.pool, me, list_id)
                .await
                .map_err(|_| not_authorized())?;
            if owned.is_none() {
                return Err(not_authorized());
            }
            Channel::List(list_id)
        }
        _ => return Err(StreamError::new("Unknown stream type", 400)),
    };
    Ok((channel, notifications))
}

struct Session {
    state: AppState,
    connection: u64,
    current: CurrentUser,
    sender: mpsc::Sender<String>,
    /// Shared per-connection close signal, handed to every subscription so
    /// the hub can drop this connection if its queue overflows.
    close: Arc<Notify>,
}

impl Session {
    fn hub(&self) -> &Hub {
        &self.state.streaming
    }

    async fn subscribe(
        &self,
        stream: &str,
        tag: Option<&str>,
        list: Option<&str>,
    ) -> Result<(), StreamError> {
        let (channel, notifications) =
            resolve_channel(&self.state, &self.current, stream, tag, list).await?;
        self.hub().subscribe(
            self.connection,
            channel,
            self.current.account.id,
            notifications,
            self.sender.clone(),
            self.close.clone(),
        );
        Ok(())
    }

    async fn unsubscribe(
        &self,
        stream: &str,
        tag: Option<&str>,
        list: Option<&str>,
    ) -> Result<(), StreamError> {
        let (channel, _) = resolve_channel(&self.state, &self.current, stream, tag, list).await?;
        self.hub().unsubscribe(self.connection, &channel);
        Ok(())
    }
}

async fn run_connection(
    state: AppState,
    mut socket: WebSocket,
    current: CurrentUser,
    query: StreamingQuery,
    // Held for the connection's lifetime; dropping it releases the admission
    // slot claimed at the upgrade (finding 27).
    _guard: ConnectionGuard,
) {
    let hub = state.streaming.clone();
    let (sender, mut outbound) = mpsc::channel::<String>(CONNECTION_QUEUE_CAP);
    let close = Arc::new(Notify::new());
    let session = Session {
        connection: hub.connection_id(),
        state,
        current,
        sender,
        close,
    };

    if let Some(stream) = query.stream.as_deref()
        && let Err(error) = session
            .subscribe(stream, query.tag.as_deref(), query.list.as_deref())
            .await
        && !send_error(&mut socket, &error).await
    {
        hub.disconnect(session.connection);
        return;
    }

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            // The hub overflowed this connection's queue: drop it.
            () = session.close.notified() => break,
            message = outbound.recv() => {
                // The session keeps its own sender alive, so `recv` never
                // returns `None`; handle it defensively by ending the loop.
                let Some(message) = message else { break };
                // Bound how long one slow write may block: a reader too slow
                // to drain within the timeout is a lagging consumer we drop.
                match tokio::time::timeout(
                    SEND_TIMEOUT,
                    socket.send(Message::Text(message.into())),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) | Err(_) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_message(&session, &mut socket, text.as_str()).await;
                    }
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    // Pings are answered by the protocol layer; pongs and
                    // binary frames carry nothing for us.
                    Some(Ok(_)) => {}
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    hub.disconnect(session.connection);
}

/// Handles one client message: `{"type":"subscribe"|"unsubscribe",
/// "stream":...,"tag":...,"list":...}`. Unknown types are ignored, like
/// Mastodon.
async fn handle_client_message(session: &Session, socket: &mut WebSocket, text: &str) {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let kind = message.get("type").and_then(Value::as_str);
    let Some(stream) = message.get("stream").and_then(Value::as_str) else {
        return;
    };
    let tag = message.get("tag").and_then(Value::as_str);
    // Clients send the list id as a string or a bare number.
    let list = message.get("list").and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let result = match kind {
        Some("subscribe") => session.subscribe(stream, tag, list.as_deref()).await,
        Some("unsubscribe") => session.unsubscribe(stream, tag, list.as_deref()).await,
        _ => Ok(()),
    };
    if let Err(error) = result {
        send_error(socket, &error).await;
    }
}

/// Reports a subscription error over the socket; the connection stays open
/// (Mastodon's behavior). Returns whether the socket is still usable.
async fn send_error(socket: &mut WebSocket, error: &StreamError) -> bool {
    let message = json!({ "error": error.message, "status": error.status }).to_string();
    socket.send(Message::Text(message.into())).await.is_ok()
}
