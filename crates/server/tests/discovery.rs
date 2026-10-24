//! Real guarded transport plus durable worker lifecycle, with loopback fixtures.
use plamenu::{AppState, discovery::worker, federation::HttpFederation};
use plamenu_db::{PgPool, discovery_store as db};
use plamenu_federation::FederationClient;
use plamenu_test_support as common;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn state(pool: PgPool, proxy: Option<String>) -> AppState {
    let config = common::test_config();
    let keyring = Arc::new(plamenu::crypto::FederationKeyring::from_config(&config).unwrap());
    let federation = Arc::new(HttpFederation::new(
        FederationClient::new("discovery-test", true)
            .unwrap()
            .with_proxies(&plamenu_federation::ProxyConfig {
                onion_proxy_url: proxy,
                ..plamenu_federation::ProxyConfig::default()
            })
            .unwrap(),
        pool.clone(),
        config.domain.clone(),
        keyring,
    ));
    AppState::new(
        pool,
        config,
        federation,
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap()
}
async fn admitted(pool: &PgPool, uri: &str) -> i64 {
    let mut settings = db::settings(pool).await.unwrap();
    settings.enabled = true;
    settings.disabled_mechanisms.push("peer_exploration".into());
    db::save_settings(pool, &settings).await.unwrap();
    let db::Admission::Added(source) = db::add_source(pool, uri, None).await.unwrap() else {
        panic!("new source")
    };
    db::enqueue(pool, source, uri, db::Operation::InspectSource, None)
        .await
        .unwrap();
    source
}
async fn request(socket: &mut TcpStream) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(socket.read_u8().await.unwrap());
        assert!(bytes.len() < 16384);
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn inspection_only_records_observed_capability_and_has_no_import_side_effects(pool: PgPool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let uri = "http://discovery-test.onion/metadata".to_owned();
    let source = admitted(&pool, &uri).await;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        request(&mut socket).await;
        let body = r#"{"software":"mastodon","directory":true}"#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        body.len()
    });
    assert_eq!(
        worker::run_due(&state(pool.clone(), Some(proxy)))
            .await
            .unwrap(),
        1
    );
    let bytes = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let source = db::source(&pool, source).await.unwrap().unwrap();
    assert_eq!(source.capabilities, vec!["source_inspection"]);
    assert_eq!(source.canonical_uri.as_deref(), Some(uri.as_str()));
    let status = db::diagnostics(&pool).await.unwrap();
    assert!(status.last_success_at.is_some());
    assert_eq!(status.queued, 0);
    assert_eq!(status.requests_24h, 1);
    assert_eq!(status.charged_bytes_24h, i64::try_from(bytes).unwrap());
    let counts=sqlx::query!(r#"SELECT (SELECT count(*) FROM accounts) AS "accounts!", (SELECT count(*) FROM statuses) AS "statuses!", (SELECT count(*) FROM follows) AS "follows!", (SELECT count(*) FROM discovery_entries) AS "entries!""#).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (
            counts.accounts,
            counts.statuses,
            counts.follows,
            counts.entries
        ),
        (0, 0, 0, 0)
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn pause_cancels_stream_and_resume_does_not_refund_or_overlap_requests(pool: PgPool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let uri = "http://discovery-test.onion/slow".to_owned();
    let source = admitted(&pool, &uri).await;
    let (ready, started) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").await.unwrap();
        ready.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let state = state(pool.clone(), Some(proxy));
    let copy = state.clone();
    let work = tokio::spawn(async move { worker::run_due(&copy).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), started)
        .await
        .unwrap()
        .unwrap();
    let mut settings = db::settings(&pool).await.unwrap();
    settings.paused = true;
    db::save_settings(&pool, &settings).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), work)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        db::source(&pool, source)
            .await
            .unwrap()
            .unwrap()
            .verified_at
            .is_none()
    );
    assert_eq!(
        db::diagnostics(&pool).await.unwrap().charged_bytes_24h,
        db::RESPONSE_BYTES
    );
    assert_eq!(worker::run_due(&state).await.unwrap(), 0);
    let mut settings = db::settings(&pool).await.unwrap();
    settings.paused = false;
    db::save_settings(&pool, &settings).await.unwrap();
    assert_eq!(worker::run_due(&state).await.unwrap(), 1);
    let status = db::diagnostics(&pool).await.unwrap();
    assert_eq!(status.requests_24h, 1);
    assert_eq!(status.queued, 1);
    assert_eq!(status.leased, 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn shutdown_leaves_recoverable_lease_and_conservative_charge(pool: PgPool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let uri = "http://discovery-test.onion/slow".to_owned();
    admitted(&pool, &uri).await;
    let (ready, started) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        request(&mut socket).await;
        ready.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let state = state(pool.clone(), Some(proxy));
    let copy = state.clone();
    let work = tokio::spawn(async move { worker::run_due(&copy).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), started)
        .await
        .unwrap()
        .unwrap();
    state.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(3), work)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let status = db::diagnostics(&pool).await.unwrap();
    assert_eq!(status.leased, 1);
    assert_eq!(status.charged_bytes_24h, db::RESPONSE_BYTES);
    sqlx::query!("UPDATE discovery_jobs SET leased_until=clock_timestamp()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let recovered = db::claim_due(&pool, "restarted").await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].attempts, 2);
    assert_eq!(
        db::reserve_request(&pool, &recovered[0], &uri)
            .await
            .unwrap(),
        db::Reservation::Capacity
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn blocked_instance_is_not_contacted(pool: PgPool) {
    let uri = "https://blocked.example/source";
    admitted(&pool, uri).await;
    plamenu_db::instance_policy::create_domain_block(
        &pool,
        plamenu_db::instance_policy::NewDomainBlock {
            domain: "blocked.example",
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
    assert_eq!(
        worker::run_due(&state(pool.clone(), None)).await.unwrap(),
        1
    );
    let status = db::diagnostics(&pool).await.unwrap();
    assert_eq!(status.requests_24h, 0);
    assert_eq!(status.leased, 0);
    assert!(status.last_success_at.is_none());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn outbox_sampling_requires_a_verified_author_before_any_request(pool: PgPool) {
    let source = admitted(&pool, "https://peer.example/source").await;
    let lease = db::claim_due(&pool, "verify").await.unwrap().remove(0);
    let now = time::OffsetDateTime::now_utc();
    db::finish_source(
        &pool,
        &lease,
        &db::VerifiedSource {
            canonical_uri: &lease.target_uri,
            capabilities: &["directory".into()],
            verified_at: now,
            valid_until: now + time::Duration::hours(24),
        },
    )
    .await
    .unwrap();
    db::enqueue(
        &pool,
        source,
        "https://peer.example/directory",
        db::Operation::SampleOutbox,
        None,
    )
    .await
    .unwrap();
    // Presentation now has its own scheduled job. Exercise this outbox lease
    // explicitly so unrelated source branding cannot select the fixture's work.
    let leases = db::claim_due_for(&pool, "unverified-author", &[db::Operation::SampleOutbox])
        .await
        .unwrap();
    assert_eq!(leases.len(), 1);
    plamenu::discovery::outbox::fetch(&state(pool.clone(), None), &leases[0])
        .await
        .unwrap();
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 0);
    let attempts = sqlx::query_scalar!(
        "SELECT attempts FROM discovery_jobs WHERE source_id=$1",
        source
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(attempts.is_none());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn instance_block_learned_during_redirect_prevents_the_next_contact(pool: PgPool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let authority = "discovery-test.onion".to_owned();
    let uri = format!("http://{authority}/source");
    admitted(&pool, &uri).await;
    let copy = pool.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        request(&mut socket).await;
        plamenu_db::instance_policy::create_domain_block(
            &copy,
            plamenu_db::instance_policy::NewDomainBlock {
                domain: &authority,
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
        let _=socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: /blocked\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        drop(socket);
        assert!(
            tokio::time::timeout(Duration::from_millis(500), listener.accept())
                .await
                .is_err()
        );
    });
    assert_eq!(
        worker::run_due(&state(pool.clone(), Some(proxy)))
            .await
            .unwrap(),
        1
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let status = db::diagnostics(&pool).await.unwrap();
    assert_eq!(status.requests_24h, 1);
    assert!(status.last_success_at.is_none());
}
