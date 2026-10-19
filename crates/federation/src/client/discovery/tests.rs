use super::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Default)]
struct Budget {
    requests: Mutex<Vec<String>>,
    settled: Mutex<Vec<usize>>,
    remaining: AtomicUsize,
}
impl Budget {
    fn new(max: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(max),
            ..Self::default()
        }
    }
}
impl DiscoveryBudget for Budget {
    fn reserve<'a>(&'a self, uri: &'a str) -> BudgetFuture<'a, i64> {
        Box::pin(async move {
            if self
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_err()
            {
                return Err(FederationError::DiscoveryStale);
            }
            self.requests.lock().unwrap().push(uri.into());
            Ok(1)
        })
    }
    fn complete(&self, _receipt: i64, bytes: usize) -> BudgetFuture<'_, ()> {
        self.settled.lock().unwrap().push(bytes);
        Box::pin(async { Ok(()) })
    }
}
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    loop {
        let byte = socket.read_u8().await.unwrap();
        buffer.push(byte);
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
        assert!(buffer.len() < 16384);
    }
    String::from_utf8(buffer).unwrap().to_ascii_lowercase()
}
async fn serve(responses: Vec<Vec<u8>>) -> (String, String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let uri = "http://discovery-test.onion/source".to_owned();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            requests.push(read_request(&mut socket).await);
            let _ = socket.write_all(&response).await;
        }
        requests
    });
    (proxy, uri, task)
}
fn json(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).into_bytes()
}
fn client(proxy: &str) -> FederationClient {
    FederationClient::new("discovery-test", true)
        .unwrap()
        .with_proxies(&crate::ProxyConfig {
            onion_proxy_url: Some(proxy.into()),
            ..crate::ProxyConfig::default()
        })
        .unwrap()
}
fn signer() -> RequestSigner {
    let rsa = plamenu_ap::keys::generate_keypair().unwrap();
    let ed = plamenu_ap::keys::generate_ed25519_keypair();
    RequestSigner::from_pkcs8_pem(
        &rsa.private_pem,
        "https://local.example/actor#main-key".into(),
    )
    .unwrap()
    .with_ed25519(
        "https://local.example/actor#ed25519".into(),
        &ed.private_multibase,
    )
}

#[tokio::test]
async fn anonymous_json_never_uses_installed_signer_and_accounts_redirects() {
    let (proxy, uri, server) = serve(vec![
        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
        json("{\"name\":\"peer\"}"),
    ])
    .await;
    let budget = Budget::new(2);
    let result = client(&proxy)
        .with_fetch_signer(signer())
        .fetch_discovery(&uri, DiscoveryFormat::Json, &budget)
        .await
        .unwrap();
    assert!(result.final_url.ends_with("/final"));
    assert_eq!(budget.requests.lock().unwrap().len(), 2);
    assert_eq!(*budget.settled.lock().unwrap(), vec![0, result.body_bytes]);
    for request in server.await.unwrap() {
        assert!(request.contains("accept: application/json\r\n"));
        assert!(!request.contains("signature:"));
        assert!(!request.contains("authorization:"));
        assert!(!request.contains("cookie:"));
    }
}

#[tokio::test]
async fn blocked_redirect_is_rejected_before_second_contact() {
    let (proxy,uri,server)=serve(vec![b"HTTP/1.1 302 Found\r\nLocation: /blocked\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()]).await;
    let budget = Budget::new(1);
    assert!(matches!(
        client(&proxy)
            .fetch_discovery(&uri, DiscoveryFormat::Json, &budget)
            .await,
        Err(FederationError::DiscoveryStale)
    ));
    assert_eq!(server.await.unwrap().len(), 1);
    assert_eq!(budget.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn signatures_and_fallback_each_need_a_reservation() {
    for allowance in [1, 2] {
        let mut responses = vec![
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        ];
        if allowance == 2 {
            responses.push(b"HTTP/1.1 200 OK\r\nContent-Type: application/activity+json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec());
        }
        let (proxy, uri, server) = serve(responses).await;
        let budget = Budget::new(allowance);
        let result = client(&proxy)
            .with_fetch_signer(signer())
            .fetch_discovery(&uri, DiscoveryFormat::ActivityPub, &budget)
            .await;
        assert_eq!(result.is_ok(), allowance == 2);
        let requests = server.await.unwrap();
        assert!(requests[0].contains("signature:"));
        assert!(requests[0].contains("https://local.example/actor#main-key"));
        if allowance == 2 {
            assert!(requests[1].contains("signature-input:"));
        }
        assert_eq!(budget.requests.lock().unwrap().len(), allowance);
    }
}

#[tokio::test]
async fn oversized_declared_and_chunked_bodies_remain_fully_charged() {
    let large = "x".repeat(FEDERATION_JSON_LIMIT + 1);
    let chunked=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{large}\r\n0\r\n\r\n",large.len()).into_bytes();
    let declared=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",large.len()).into_bytes();
    for response in [declared, chunked] {
        let (proxy, uri, server) = serve(vec![response]).await;
        let budget = Budget::new(1);
        assert!(matches!(
            client(&proxy)
                .fetch_discovery(&uri, DiscoveryFormat::Json, &budget)
                .await,
            Err(FederationError::DocumentTooLarge(_))
        ));
        assert!(budget.settled.lock().unwrap().is_empty());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn malformed_json_and_excessive_nesting_are_rejected_after_byte_accounting() {
    for body in [
        "{".into(),
        format!("{}0{}", "[".repeat(160), "]".repeat(160)),
    ] {
        let (proxy, uri, server) = serve(vec![json(&body)]).await;
        let budget = Budget::new(1);
        assert!(matches!(
            client(&proxy)
                .fetch_discovery(&uri, DiscoveryFormat::Json, &budget)
                .await,
            Err(FederationError::InvalidActor(_))
        ));
        assert_eq!(*budget.settled.lock().unwrap(), vec![body.len()]);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn retry_after_and_wrong_content_type_are_not_json_documents() {
    for response in [b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7200\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec()] {
        let (proxy,uri,server)=serve(vec![response]).await;
        let budget=Budget::new(1);
        let error=client(&proxy).fetch_discovery(&uri,DiscoveryFormat::Json,&budget).await.unwrap_err();
        assert!(matches!(error,FederationError::RateLimited{retry_after_secs:Some(7200)}|FederationError::InvalidActor(_)));
        assert_eq!(*budget.settled.lock().unwrap(),vec![0]);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn cancellation_closes_the_body_and_keeps_the_reservation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let uri = "http://discovery-test.onion/slow".to_owned();
    let (started, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").await.unwrap();
        started.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let budget = Arc::new(Budget::new(1));
    let copy = budget.clone();
    let task = tokio::spawn(async move {
        client(&proxy)
            .fetch_discovery(&uri, DiscoveryFormat::Json, copy.as_ref())
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    server.await.unwrap();
    assert_eq!(budget.requests.lock().unwrap().len(), 1);
    assert!(budget.settled.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ssrf_and_missing_instance_signer_fail_before_admission() {
    let budget = Budget::new(3);
    let transport = FederationClient::new("discovery-test", false).unwrap();
    assert!(
        transport
            .fetch_discovery("https://127.0.0.1/source", DiscoveryFormat::Json, &budget)
            .await
            .is_err()
    );
    assert!(
        transport
            .fetch_discovery(
                "https://public.example/actor",
                DiscoveryFormat::ActivityPub,
                &budget
            )
            .await
            .is_err()
    );
    assert!(budget.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stalled_response_hits_transport_timeout_without_refunding_reservation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let uri = "http://discovery-test.onion/timeout".to_owned();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(35), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let budget = Budget::new(1);
    let result = tokio::time::timeout(
        Duration::from_secs(35),
        client(&proxy).fetch_discovery(&uri, DiscoveryFormat::Json, &budget),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(FederationError::Stalled(_))));
    server.await.unwrap();
    assert!(budget.settled.lock().unwrap().is_empty());
}
