//! Website failures must not suppress signed `ActivityPub` at the same URL.
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response, StatusCode};
use plamenu::crypto::FederationKeyring;
use plamenu::federation::{FederationApi, HttpFederation};
use plamenu_db::PgPool;
use plamenu_federation::{FederationClient, FederationError, ProxyConfig};

struct Peer {
    html_status: StatusCode,
    html_hits: AtomicUsize,
    signed_hits: AtomicUsize,
    actor: String,
}

async fn respond(State(peer): State<Arc<Peer>>, request: Request<Body>) -> Response<Body> {
    assert_eq!(request.method(), "GET");
    assert!(!request.headers().contains_key("authorization"));
    assert!(!request.headers().contains_key("cookie"));
    if request.headers().contains_key("signature") {
        peer.signed_hits.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .header("content-type", "application/activity+json")
            .body(Body::from(peer.actor.clone()))
            .unwrap()
    } else {
        peer.html_hits.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .status(peer.html_status)
            .body(Body::empty())
            .unwrap()
    }
}

struct Fixture {
    peer: Arc<Peer>,
    federation: HttpFederation,
    server: tokio::task::JoinHandle<()>,
    uri: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture(pool: &PgPool, status: StatusCode) -> Fixture {
    let domain = format!("budget-{}.onion", status.as_u16());
    let user = common::RemoteUser::new(&domain, "erin");
    let uri = format!("http://{domain}/users/erin");
    let actor = serde_json::json!({"id":uri,"type":"Person","preferredUsername":"erin",
        "inbox":format!("{uri}/inbox"),"outbox":format!("{uri}/outbox"),
        "publicKey":{"id":format!("{uri}#main-key"),"owner":uri,"publicKeyPem":user.keys.public_pem}}).to_string();
    let peer = Arc::new(Peer {
        html_status: status,
        html_hits: AtomicUsize::new(0),
        signed_hits: AtomicUsize::new(0),
        actor,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(respond).with_state(peer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = common::test_config();
    let signer = common::RemoteUser::new("plamenu.test", "instance").signer();
    let client = FederationClient::new("webmention-budget-test", true)
        .unwrap()
        .with_proxies(&ProxyConfig {
            onion_proxy_url: Some(proxy),
            ..ProxyConfig::default()
        })
        .unwrap()
        .with_fetch_signer(signer);
    let keyring = Arc::new(FederationKeyring::from_config(&config).unwrap());
    let federation = HttpFederation::new(client, pool.clone(), config.domain, keyring);
    Fixture {
        peer,
        federation,
        server,
        uri,
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn unsigned_website_refusal_does_not_suppress_signed_actor_or_lose_its_own_budget(
    pool: PgPool,
) {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
    ] {
        let f = fixture(&pool, status).await;
        assert!(matches!(f.federation.fetch_mention_page(&f.uri).await,
            Err(FederationError::Status(code)) if code == status.as_u16()));
        assert_eq!(f.federation.fetch_actor(&f.uri).await.unwrap().id, f.uri);
        // AP success must not clear the independent unsigned website refusal.
        assert!(matches!(
            f.federation.fetch_mention_page(&f.uri).await,
            Err(FederationError::FetchSuppressed(_))
        ));
        assert_eq!(f.peer.html_hits.load(Ordering::SeqCst), 1);
        assert_eq!(f.peer.signed_hits.load(Ordering::SeqCst), 1);
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn website_server_failure_still_suppresses_the_host_for_signed_fetches(pool: PgPool) {
    let f = fixture(&pool, StatusCode::SERVICE_UNAVAILABLE).await;
    assert!(matches!(
        f.federation.fetch_mention_page(&f.uri).await,
        Err(FederationError::Status(503))
    ));
    assert!(matches!(
        f.federation.fetch_actor(&f.uri).await,
        Err(FederationError::FetchSuppressed(_))
    ));
    assert_eq!(f.peer.html_hits.load(Ordering::SeqCst), 1);
    assert_eq!(f.peer.signed_hits.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn link_preview_refusal_does_not_suppress_signed_actor(pool: PgPool) {
    let f = fixture(&pool, StatusCode::NOT_FOUND).await;
    assert!(matches!(
        f.federation.fetch_page(&f.uri, "text/html").await,
        Err(FederationError::Status(404))
    ));
    assert_eq!(f.federation.fetch_actor(&f.uri).await.unwrap().id, f.uri);
    assert!(matches!(
        f.federation.fetch_page(&f.uri, "text/html").await,
        Err(FederationError::FetchSuppressed(_))
    ));
    assert_eq!(f.peer.html_hits.load(Ordering::SeqCst), 1);
    assert_eq!(f.peer.signed_hits.load(Ordering::SeqCst), 1);
}
