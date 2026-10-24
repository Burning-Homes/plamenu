//! Real production `HttpFederation` redirect/policy wiring over an owned proxy.
//! Proposed installation: `crates/server/tests/actor_reconciliation_transport.rs`.
//! HTTP is restricted to the existing hidden-service lane; the explicit
//! loopback proxy handles every logical destination without external DNS.
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use plamenu::crypto::FederationKeyring;
use plamenu::federation::{FederationApi, HttpFederation};
use plamenu_db::{PgPool, instance_policy};
use plamenu_federation::{FederationClient, FederationError, ProxyConfig};
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

const ACTOR: &str = "http://actor-reconciliation-origin.onion/users/bob";
const SAME_ORIGIN_DESTINATION: &str = "http://actor-reconciliation-origin.onion/actor/current";
const FOREIGN_DESTINATION: &str = "http://actor-reconciliation-foreign.onion/actor/current";
const ORIGIN_HOST: &str = "actor-reconciliation-origin.onion";

struct ProxyState {
    destination: &'static str,
    hold_origin_response: bool,
    origin_received: Notify,
    release_origin_response: Semaphore,
    origin_hits: AtomicUsize,
    destination_hits: AtomicUsize,
    foreign_hits: AtomicUsize,
    signed_hits: AtomicUsize,
    authorization_or_cookie_hits: AtomicUsize,
    bad_method_or_body_hits: AtomicUsize,
}

async fn proxy(State(state): State<Arc<ProxyState>>, request: Request<Body>) -> Response<Body> {
    // Count only nonsecret properties. Never retain or print signature values,
    // headers, signing material or request bodies.
    let host = request.uri().host().unwrap_or_default().to_owned();
    let path = request.uri().path().to_owned();
    if request.headers().contains_key("signature") {
        state.signed_hits.fetch_add(1, Ordering::SeqCst);
    }
    if request.headers().contains_key(header::AUTHORIZATION)
        || request.headers().contains_key(header::COOKIE)
    {
        state
            .authorization_or_cookie_hits
            .fetch_add(1, Ordering::SeqCst);
    }
    let get = request.method() == "GET";
    let empty = request
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .is_empty();
    if !get || !empty {
        state.bad_method_or_body_hits.fetch_add(1, Ordering::SeqCst);
    }
    if host != ORIGIN_HOST {
        state.foreign_hits.fetch_add(1, Ordering::SeqCst);
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::empty())
            .unwrap();
    }
    if path == "/users/bob" {
        state.origin_hits.fetch_add(1, Ordering::SeqCst);
        state.origin_received.notify_one();
        if state.hold_origin_response {
            let _permit = state.release_origin_response.acquire().await.unwrap();
        }
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, state.destination)
            .body(Body::empty())
            .unwrap();
    }
    state.destination_hits.fetch_add(1, Ordering::SeqCst);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/activity+json")
        .body(Body::from(
            serde_json::to_vec(&json!({"id":ACTOR,"type":"Person","name":"Current origin"}))
                .unwrap(),
        ))
        .unwrap()
}

struct Fixture {
    federation: Arc<HttpFederation>,
    proxy: Arc<ProxyState>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(pool: &PgPool, destination: &'static str, hold_origin_response: bool) -> Fixture {
    let proxy_state = Arc::new(ProxyState {
        destination,
        hold_origin_response,
        origin_received: Notify::new(),
        release_origin_response: Semaphore::new(0),
        origin_hits: AtomicUsize::new(0),
        destination_hits: AtomicUsize::new(0),
        foreign_hits: AtomicUsize::new(0),
        signed_hits: AtomicUsize::new(0),
        authorization_or_cookie_hits: AtomicUsize::new(0),
        bad_method_or_body_hits: AtomicUsize::new(0),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .fallback(proxy)
        .with_state(proxy_state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let mut config = common::test_config();
    // This flag belongs only to this fixture/client. No production config,
    // scheme rule, DNS mapping or certificate trust is changed.
    config.allow_private_fetch = true;
    let signer = common::RemoteUser::new("plamenu.test", "transportfixture").signer();
    let client = FederationClient::new(
        "actor-reconciliation-owned-proxy-test",
        config.allow_private_fetch,
    )
    .unwrap()
    .with_proxies(&ProxyConfig {
        onion_proxy_url: Some(format!("http://{address}")),
        ..ProxyConfig::default()
    })
    .unwrap()
    .with_fetch_signer(signer);
    let keyring = Arc::new(FederationKeyring::from_config(&config).unwrap());
    let federation = Arc::new(HttpFederation::new(
        client,
        pool.clone(),
        config.domain,
        keyring,
    ));
    Fixture {
        federation,
        proxy: proxy_state,
        server,
    }
}

fn assert_no_authentication_material_or_body(fixture: &Fixture) {
    assert_eq!(
        fixture
            .proxy
            .authorization_or_cookie_hits
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        fixture.proxy.bad_method_or_body_hits.load(Ordering::SeqCst),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn real_http_federation_same_origin_redirect_succeeds_when_policy_allows(pool: PgPool) {
    let fixture = fixture(&pool, SAME_ORIGIN_DESTINATION, false).await;
    assert!(
        plamenu::instance_policy::can_federate_url(&pool, "plamenu.test", ACTOR)
            .await
            .unwrap()
    );
    let fetched = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.federation.fetch_activitypub_same_origin(ACTOR),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(fetched.final_url, SAME_ORIGIN_DESTINATION);
    assert_eq!(fetched.document.unwrap()["id"], ACTOR);
    assert_eq!(fixture.proxy.origin_hits.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.proxy.destination_hits.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.proxy.foreign_hits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.proxy.signed_hits.load(Ordering::SeqCst), 2);
    assert_no_authentication_material_or_body(&fixture);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn real_http_federation_redirect_rechecks_policy_changed_after_first_origin_get(
    pool: PgPool,
) {
    let fixture = fixture(&pool, SAME_ORIGIN_DESTINATION, true).await;
    assert!(
        plamenu::instance_policy::can_federate_url(&pool, "plamenu.test", ACTOR)
            .await
            .unwrap()
    );
    let federation = fixture.federation.clone();
    let request =
        tokio::spawn(async move { federation.fetch_activitypub_same_origin(ACTOR).await });
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.proxy.origin_received.notified(),
    )
    .await
    .unwrap();
    assert_eq!(fixture.proxy.origin_hits.load(Ordering::SeqCst), 1);
    // The production fetch is already in flight with initially allowed policy.
    // Commit the block before releasing its response, making the redirect
    // callback deterministic rather than racing response delivery.
    instance_policy::create_domain_block(
        &pool,
        instance_policy::NewDomainBlock {
            domain: ORIGIN_HOST,
            severity: "suspend",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    assert!(
        !plamenu::instance_policy::can_federate_url(&pool, "plamenu.test", SAME_ORIGIN_DESTINATION)
            .await
            .unwrap()
    );
    fixture.proxy.release_origin_response.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(FederationError::InvalidUrl(_))));
    assert_eq!(fixture.proxy.origin_hits.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.proxy.destination_hits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.proxy.foreign_hits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.proxy.signed_hits.load(Ordering::SeqCst), 1);
    assert_no_authentication_material_or_body(&fixture);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn real_http_federation_foreign_origin_redirect_never_receives_request(pool: PgPool) {
    let fixture = fixture(&pool, FOREIGN_DESTINATION, false).await;
    // Both destinations are allowed by domain policy. Origin equality itself
    // must reject the second one; a block cannot accidentally make this green.
    for uri in [ACTOR, FOREIGN_DESTINATION] {
        assert!(
            plamenu::instance_policy::can_federate_url(&pool, "plamenu.test", uri)
                .await
                .unwrap()
        );
    }
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.federation.fetch_activitypub_same_origin(ACTOR),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(FederationError::InvalidUrl(_))));
    assert_eq!(fixture.proxy.origin_hits.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.proxy.destination_hits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.proxy.foreign_hits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.proxy.signed_hits.load(Ordering::SeqCst), 1);
    assert_no_authentication_material_or_body(&fixture);
}
