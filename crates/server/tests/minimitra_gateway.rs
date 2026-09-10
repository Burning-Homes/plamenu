//! Minimitra/FEP-ae97 interoperability at the HTTP boundary.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, test_app_split_domain, test_state_with};
use http_body_util::BodyExt;
use plamenu::build_router;
use plamenu_ap::keys::{Ed25519KeyPairMultibase, KeyPairPem};
use plamenu_db::user::TimelineOrder;
use plamenu_db::{
    PgPool, account, account_domain_block, admin_account, favourite, follow, gateway, group,
    instance_policy, instance_settings, report, role, status,
};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

const AP_JSON: &str = "application/activity+json";

struct PortableUser {
    did_keys: Ed25519KeyPairMultibase,
    client_keys: KeyPairPem,
    did: String,
    host_domain: String,
    canonical_actor_id: String,
    actor_id: String,
    inbox_path: String,
    outbox_path: String,
}

impl PortableUser {
    fn new(label: &str) -> Self {
        Self::new_on(label, TEST_DOMAIN)
    }

    fn new_on(label: &str, host_domain: &str) -> Self {
        let did_keys = plamenu_ap::keys::generate_ed25519_keypair();
        let client_keys = plamenu_ap::keys::generate_keypair().unwrap();
        let did = format!("did:key:{}", did_keys.public_multibase);
        let actor_id = format!("https://{host_domain}/.well-known/apgateway/{did}/actors/{label}");
        let canonical_actor_id = format!("ap://{did}/actors/{label}");
        let prefix = format!("/.well-known/apgateway/{did}/actors/{label}");
        Self {
            did_keys,
            client_keys,
            did,
            host_domain: host_domain.to_owned(),
            canonical_actor_id,
            actor_id,
            inbox_path: format!("{prefix}/inbox"),
            outbox_path: format!("{prefix}/outbox"),
        }
    }

    fn timestamp() -> String {
        time::OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .unwrap()
            .format(&Rfc3339)
            .unwrap()
    }

    fn sign(&self, value: &Value) -> Value {
        plamenu_ap::proof::sign_document(
            value,
            &self.did_keys.private_multibase,
            &self.did,
            &Self::timestamp(),
        )
        .unwrap()
    }

    fn actor(&self, username: &str) -> Value {
        let client_multikey =
            plamenu_ap::multikey::encode_rsa_public(&self.client_keys.public_pem).unwrap();
        self.sign(&json!({
            "@context": [
                plamenu_ap::AS_CONTEXT,
                "https://w3id.org/security/data-integrity/v1",
                "https://www.w3.org/ns/cid/v1",
                { "gateways": "https://json-ld.org/contexts/person.jsonld#gateways" }
            ],
            "id": self.actor_id,
            "type": "Person",
            "preferredUsername": username,
            "name": username,
            "summary": "Minimitra portable actor",
            "discoverable": true,
            "inbox": format!("{}/inbox", self.actor_id),
            "outbox": format!("{}/outbox", self.actor_id),
            "followers": format!("{}/followers", self.actor_id),
            "following": format!("{}/following", self.actor_id),
            "gateways": [format!("https://{}", self.host_domain)],
            "assertionMethod": [{
                "id": format!("{}#main-key", self.canonical_actor_id),
                "type": "Multikey",
                "controller": self.actor_id,
                "publicKeyMultibase": client_multikey,
            }],
        }))
    }

    fn request_signer(&self) -> RequestSigner {
        RequestSigner::from_pkcs8_pem(
            &self.client_keys.private_pem,
            format!("{}#main-key", self.canonical_actor_id),
        )
        .unwrap()
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn portable_handles_use_the_account_domain_without_moving_identity(pool: PgPool) {
    const HOST_DOMAIN: &str = "social.example.test";
    const ACCOUNT_DOMAIN: &str = "example.test";

    open_registrations(&pool).await;
    common::open_previews(&pool).await;
    let app = test_app_split_domain(pool.clone(), HOST_DOMAIN, ACCOUNT_DOMAIN);
    let portable = PortableUser::new_on("split-domain-user", HOST_DOMAIN);
    assert_eq!(
        post_json(&app, "/.well-known/apgateway", &portable.actor("alice"))
            .await
            .0,
        StatusCode::CREATED
    );

    let stored = account::find_local_account_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert!(stored.is_portable_on(HOST_DOMAIN));
    assert_eq!(stored.uri.as_deref(), Some(portable.actor_id.as_str()));

    let lookup = Request::builder()
        .uri("/api/v1/accounts/lookup?acct=alice%40example.test")
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(&app, lookup).await;
    assert_eq!(status, StatusCode::OK);
    let lookup: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(lookup["acct"], "alice");
    assert_eq!(lookup["uri"], portable.actor_id);
    assert_eq!(lookup["url"], format!("https://{HOST_DOMAIN}/@alice"));

    let webfinger = Request::builder()
        .uri("/.well-known/webfinger?resource=acct:alice%40example.test")
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(&app, webfinger).await;
    assert_eq!(status, StatusCode::OK);
    let webfinger: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(webfinger["subject"], "acct:alice@example.test");
    assert!(
        webfinger["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["href"] == portable.actor_id)
    );
}

async fn open_registrations(pool: &PgPool) {
    let current = instance_settings::get(pool).await.unwrap();
    instance_settings::save(
        pool,
        instance_settings::SettingsUpdate {
            registrations_mode: instance_settings::RegistrationsMode::Open,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

async fn post_json(app: &Router, path: &str, value: &Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, AP_JSON)
        .body(Body::from(serde_json::to_vec(value).unwrap()))
        .unwrap();
    let (status, bytes) = send(app, request).await;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn post_remote_signed(
    app: &Router,
    path: &str,
    value: &Value,
    signer: &RequestSigner,
) -> StatusCode {
    let body = serde_json::to_vec(value).unwrap();
    let request_headers = signer.sign_post(TEST_DOMAIN, path, &body, SystemTime::now());
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::HOST, request_headers.host)
        .header(header::DATE, request_headers.date)
        .header("digest", request_headers.digest)
        .header("signature", request_headers.signature)
        .header(header::CONTENT_TYPE, AP_JSON)
        .body(Body::from(body))
        .unwrap();
    send(app, request).await.0
}

async fn get_client_signed(
    app: &Router,
    request_path: &str,
    signed_path: &str,
    signer: &RequestSigner,
) -> (StatusCode, Value) {
    let request_headers = signer.sign_get(TEST_DOMAIN, signed_path, AP_JSON, SystemTime::now());
    let request = Request::builder()
        .method(Method::GET)
        .uri(request_path)
        .header(header::HOST, TEST_DOMAIN)
        .header(header::DATE, request_headers.date)
        .header("signature", request_headers.signature)
        .header(header::ACCEPT, AP_JSON)
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(app, request).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_reply_delivered_to_portable_inbox_is_not_reingested(pool: PgPool) {
    open_registrations(&pool).await;
    common::open_previews(&pool).await;
    let local = common::create_immutable_local_account(&pool, "localbob", "Local Bob").await;
    let local_actor = local.uri.as_deref().unwrap().to_owned();
    let state = test_state_with(pool.clone(), StubFederation::default().into());
    let app = build_router(state.clone());
    let portable = PortableUser::new("thread-owner");
    assert_eq!(
        post_json(&app, "/.well-known/apgateway", &portable.actor("alice"))
            .await
            .0,
        StatusCode::CREATED
    );
    let portable_account = account::find_local_account_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();

    let root = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/objects/thread-root", portable.actor_id),
        "type": "Note",
        "attributedTo": portable.actor_id,
        "content": "portable thread root",
        "published": PortableUser::timestamp(),
        "to": plamenu_ap::activity::PUBLIC,
    }));
    let root_create = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/thread-root", portable.actor_id),
        "type": "Create",
        "actor": portable.actor_id,
        "object": root,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &root_create).await.0,
        StatusCode::ACCEPTED
    );
    let stored_root = status::find_by_uri(&pool, root["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();

    // Plamenu already materialized this local reply. Its outbound Create must
    // be retained for client polling without being ingested back as a second
    // status merely because the recipient uses a gateway inbox on this host.
    let local_reply = status::create_local(
        &pool,
        status::NewLocalStatus::new(
            local.id,
            "<p>local reply</p>",
            "public",
            Some(stored_root.id),
        ),
    )
    .await
    .unwrap();
    let local_reply_uri =
        plamenu::entities::status_uri_for_account(TEST_DOMAIN, &local_reply, &local);
    let local_create = json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": format!("{local_reply_uri}/activity"),
        "type": "Create",
        "actor": local_actor,
        "object": {
            "id": local_reply_uri,
            "type": "Note",
            "attributedTo": local_actor,
            "content": local_reply.content,
            "inReplyTo": root["id"],
            "published": local_reply.created_at.format(&Rfc3339).unwrap(),
            "to": [plamenu_ap::activity::PUBLIC, portable.actor_id],
        },
    });
    let local_key = plamenu::key_store::account_signing_key(&state, local.id, "rsa")
        .await
        .unwrap();
    let local_signer = RequestSigner::from_pkcs8_pem(
        local_key.private.expose_str().unwrap(),
        local_key.record.key_uri.clone(),
    )
    .unwrap();
    assert_eq!(
        post_remote_signed(&app, &portable.inbox_path, &local_create, &local_signer).await,
        StatusCode::ACCEPTED
    );
    let (inbox_status, inbox) = get_client_signed(
        &app,
        &portable.inbox_path,
        &portable.inbox_path,
        &portable.request_signer(),
    )
    .await;
    assert_eq!(inbox_status, StatusCode::OK);
    assert_eq!(inbox["orderedItems"], json!([local_create]));

    let same_timestamp_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND created_at = $2",
    )
    .bind(local.id)
    .bind(local_reply.created_at)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(same_timestamp_count, 1, "local reply was re-ingested");
    assert!(
        status::find_by_uri(&pool, &local_reply_uri)
            .await
            .unwrap()
            .is_none(),
        "a local status must retain its null stored URI"
    );
    assert_eq!(
        plamenu::ingest::resolve_status_ref(&state, &local_reply_uri)
            .await
            .unwrap()
            .unwrap()
            .id,
        local_reply.id
    );

    // A later Minimitra reply names the canonical local URI. It must resolve
    // to the original row, so both thread topology and the replies counter are
    // attached to the status the local author actually created.
    let portable_reply = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/objects/portable-reply", portable.actor_id),
        "type": "Note",
        "attributedTo": portable.actor_id,
        "content": "portable reply to local reply",
        "inReplyTo": local_reply_uri,
        "published": PortableUser::timestamp(),
        "to": [plamenu_ap::activity::PUBLIC, local_actor],
    }));
    let portable_create = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/portable-reply", portable.actor_id),
        "type": "Create",
        "actor": portable.actor_id,
        "object": portable_reply,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &portable_create)
            .await
            .0,
        StatusCode::ACCEPTED
    );
    let stored_portable_reply = status::find_by_uri(&pool, portable_reply["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored_portable_reply.account_id, portable_account.id);
    assert_eq!(stored_portable_reply.in_reply_to_id, Some(local_reply.id));

    let status_request = Request::builder()
        .uri(format!("/api/v1/statuses/{}", local_reply.id))
        .body(Body::empty())
        .unwrap();
    let (status_code, status_bytes) = send(&app, status_request).await;
    assert_eq!(status_code, StatusCode::OK);
    let rendered: Value = serde_json::from_slice(&status_bytes).unwrap();
    assert_eq!(rendered["replies_count"], 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn minimitra_gateway_lifecycle_is_end_to_end(pool: PgPool) {
    open_registrations(&pool).await;
    common::open_previews(&pool).await;
    let local = common::create_immutable_local_account(&pool, "localbob", "Local Bob").await;
    let local_actor = local.uri.as_deref().unwrap().to_owned();
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let portable = PortableUser::new("user-one");
    let initial_actor = portable.actor("alice");

    // Discovery and registration use Minimitra's exact endpoint/response
    // contract. Re-registration returns the same gateway key with 200.
    let request = Request::builder()
        .uri("/.well-known/apgateway")
        .body(Body::empty())
        .unwrap();
    let (status, metadata) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&metadata).unwrap()["uploadMedia"],
        format!("https://{TEST_DOMAIN}/.well-known/apgateway-media")
    );

    let (status, keys) = post_json(&app, "/.well-known/apgateway", &initial_actor).await;
    assert_eq!(status, StatusCode::CREATED, "{keys}");
    let gateway_method = keys["assertionMethod"][0].clone();
    let gateway_multikey = gateway_method["publicKeyMultibase"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(gateway_multikey.starts_with("z4MX"));
    let (status, same_keys) = post_json(&app, "/.well-known/apgateway", &initial_actor).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(same_keys, keys);

    // Registration creates a real local-namespace account while preserving
    // the client-owned portable actor URI. It is neither an implementation
    // row nor a Plamenu-owned signing identity.
    let portable_account = account::find_local_account_by_username(&pool, "alice")
        .await
        .unwrap()
        .expect("portable account is in the local handle namespace");
    let portable_account_id = portable_account.id.to_string();
    assert!(!portable_account.is_local());
    assert!(portable_account.is_portable_on(TEST_DOMAIN));
    assert!(
        !account::is_internal(&pool, portable_account.id)
            .await
            .unwrap()
    );
    assert!(
        account::local_username_reserved(&pool, "ALICE")
            .await
            .unwrap()
    );
    let collision = PortableUser::new("user-two").actor("alice");
    assert_eq!(
        post_json(&app, "/.well-known/apgateway", &collision)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let second_portable = PortableUser::new("user-three");
    assert_eq!(
        post_json(
            &app,
            "/.well-known/apgateway",
            &second_portable.actor("carol")
        )
        .await
        .0,
        StatusCode::CREATED
    );

    let lookup = Request::builder()
        .uri("/api/v1/accounts/lookup?acct=alice")
        .body(Body::empty())
        .unwrap();
    let (lookup_status, lookup_bytes) = send(&app, lookup).await;
    assert_eq!(lookup_status, StatusCode::OK);
    let lookup: Value = serde_json::from_slice(&lookup_bytes).unwrap();
    assert_eq!(lookup["acct"], "alice");
    assert_eq!(lookup["uri"], portable.actor_id);
    assert_eq!(lookup["url"], format!("https://{TEST_DOMAIN}/@alice"));

    let search = Request::builder()
        .uri("/api/v2/search?q=alice&type=accounts&resolve=false")
        .body(Body::empty())
        .unwrap();
    let (search_status, search_bytes) = send(&app, search).await;
    assert_eq!(search_status, StatusCode::OK);
    let search: Value = serde_json::from_slice(&search_bytes).unwrap();
    assert!(
        search["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"].as_str() == Some(portable_account_id.as_str())),
        "portable account is missing from normal account search: {search}"
    );

    // Minimitra adds the returned RSA method to the actor, publishes it as
    // classic `publicKey`, and submits a did:key-signed Update to its outbox.
    let mut current_actor = initial_actor.clone();
    current_actor.as_object_mut().unwrap().remove("proof");
    current_actor["assertionMethod"]
        .as_array_mut()
        .unwrap()
        .push(gateway_method);
    current_actor["publicKey"] = json!({
        "id": format!("{}#gateway-rsa", portable.actor_id),
        "owner": portable.actor_id,
        "publicKeyPem": plamenu_ap::multikey::decode_rsa_public(&gateway_multikey).unwrap(),
    });
    current_actor = portable.sign(&current_actor);
    let update = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/update-1", portable.actor_id),
        "type": "Update",
        "actor": portable.actor_id,
        "object": current_actor,
    }));
    let (status, update_result) = post_json(&app, &portable.outbox_path, &update).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{update_result}");

    let actor_path = portable
        .actor_id
        .strip_prefix(&format!("https://{TEST_DOMAIN}"))
        .unwrap();
    let request = Request::builder()
        .uri(actor_path)
        .header(header::ACCEPT, AP_JSON)
        .body(Body::empty())
        .unwrap();
    let (status, actor_bytes) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    let served_actor: Value = serde_json::from_slice(&actor_bytes).unwrap();
    assert_eq!(served_actor, update["object"]);

    let profile = Request::builder()
        .uri("/@alice")
        .body(Body::empty())
        .unwrap();
    let (profile_status, profile_bytes) = send(&app, profile).await;
    assert_eq!(profile_status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&profile_bytes).contains("Minimitra portable actor"),
        "portable profile is rendered by the ordinary Plamenu UI"
    );

    let directory = Request::builder()
        .uri("/api/v1/directory?local=true")
        .body(Body::empty())
        .unwrap();
    let (directory_status, directory_bytes) = send(&app, directory).await;
    assert_eq!(directory_status, StatusCode::OK);
    let directory: Value = serde_json::from_slice(&directory_bytes).unwrap();
    assert!(
        directory
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"].as_str() == Some(portable_account_id.as_str())),
        "portable account is part of the local directory: {directory}"
    );

    let local_admin = admin_account::list(
        &pool,
        &admin_account::AdminAccountFilter {
            origin: Some("local".into()),
            limit: 50,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let portable_admin = local_admin
        .iter()
        .find(|view| view.account.id == portable_account.id)
        .expect("portable account is visible to local-origin admin filters");
    assert!(portable_admin.portable);
    assert!(!portable_admin.has_user);
    assert!(
        instance_policy::known_instances(
            &pool,
            &instance_policy::KnownInstanceFilter {
                limit: 50,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .is_empty(),
        "the gateway host is not a remote instance merely because portable actors use it as their acct domain"
    );

    // The gateway-hosted acct remains WebFinger-discoverable as the same
    // client-owned actor exposed through Plamenu's ordinary account surfaces.
    let request = Request::builder()
        .uri("/.well-known/webfinger?resource=acct:alice@plamenu.test")
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    let webfinger: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        webfinger["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["href"] == portable.actor_id),
        "{webfinger}"
    );

    // Its federated acct domain is the local gateway host, but neither
    // instance-level nor user-level domain blocks may reclassify the portable
    // account as remote and hide it. Account-level moderation remains active.
    let block = instance_policy::create_domain_block(
        &pool,
        instance_policy::NewDomainBlock {
            domain: TEST_DOMAIN,
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
        instance_policy::account_domain_allows_federation(&pool, portable_account.id)
            .await
            .unwrap()
    );
    instance_policy::delete_domain_block(&pool, block.id)
        .await
        .unwrap();
    account_domain_block::create(&pool, local.id, TEST_DOMAIN)
        .await
        .unwrap();
    assert!(
        !account_domain_block::blocks_account_domain(&pool, local.id, portable_account.id)
            .await
            .unwrap()
    );
    account_domain_block::delete(&pool, local.id, TEST_DOMAIN)
        .await
        .unwrap();

    // A normally HTTP-signed remote Follow is stored for polling and projected
    // as a pending gateway follower.
    let follow = json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": format!("{}#follows/portable", bob.actor.id),
        "type": "Follow",
        "actor": bob.actor.id,
        "object": portable.actor_id,
    });
    assert_eq!(
        post_remote_signed(&app, &portable.inbox_path, &follow, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let client_signer = portable.request_signer();
    let unsigned = Request::builder()
        .uri(&portable.inbox_path)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, unsigned).await.0, StatusCode::UNAUTHORIZED);
    let (status, inbox) = get_client_signed(
        &app,
        &portable.inbox_path,
        &portable.inbox_path,
        &client_signer,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{inbox}");
    assert_eq!(inbox["orderedItems"], json!([follow]));
    let bob_account_id = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap()
        .id;
    assert!(
        follow::find(&pool, bob_account_id, portable_account.id)
            .await
            .unwrap()
            .is_some_and(|edge| edge.pending),
        "a remote follow is projected into the normal pending relationship graph"
    );

    // Minimitra signs only the URL path while putting `after` in the actual
    // request URI. The gateway's opt-in compatibility verification accepts it.
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("after", follow["id"].as_str().unwrap())
        .finish();
    let poll_path = format!("{}?{query}", portable.inbox_path);
    let (status, next) =
        get_client_signed(&app, &poll_path, &portable.inbox_path, &client_signer).await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["orderedItems"], json!([]));

    // Minimitra refers to the prior Follow by URI when accepting it. Resolving
    // that durable inbox item activates followers-collection fanout. Both the
    // Accept and subsequent Create are queued with their original IDs/proofs.
    let accept = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/accept-1", portable.actor_id),
        "type": "Accept",
        "actor": portable.actor_id,
        "object": follow["id"],
        "to": bob.actor.id,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &accept).await.0,
        StatusCode::ACCEPTED
    );
    assert!(
        follow::find(&pool, bob_account_id, portable_account.id)
            .await
            .unwrap()
            .is_some_and(|edge| !edge.pending),
        "Minimitra's Accept activates the ordinary relationship"
    );

    // The opposite direction uses the same graph: Minimitra initiates a
    // pending remote follow, and a normally HTTP-signed Accept arriving in its
    // client inbox settles it even though the follower actor is portable.
    let follow_bob = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/follow-bob", portable.actor_id),
        "type": "Follow",
        "actor": portable.actor_id,
        "object": bob.actor.id,
        "to": bob.actor.id,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &follow_bob).await.0,
        StatusCode::ACCEPTED
    );
    assert!(
        follow::find(&pool, portable_account.id, bob_account_id)
            .await
            .unwrap()
            .is_some_and(|edge| edge.pending)
    );
    let bob_accept = json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": format!("{}#accept/portable", bob.actor.id),
        "type": "Accept",
        "actor": bob.actor.id,
        "object": follow_bob,
    });
    assert_eq!(
        post_remote_signed(&app, &portable.inbox_path, &bob_accept, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(
        follow::find(&pool, portable_account.id, bob_account_id)
            .await
            .unwrap()
            .is_some_and(|edge| !edge.pending)
    );

    // An ordinary Plamenu user can follow the portable account too. The
    // Follow is delivered to the client-owned inbox for its verdict, while
    // the accepted edge feeds the normal home timeline without looping raw
    // portable posts back through Plamenu's own HTTP inbox.
    plamenu::actions::follow_account(&state, &local, &portable_account)
        .await
        .unwrap();
    let local_edge = follow::find(&pool, local.id, portable_account.id)
        .await
        .unwrap()
        .expect("local-to-portable follow request");
    let local_follow = json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": local_edge.uri.as_deref().unwrap(),
        "type": "Follow",
        "actor": local_actor,
        "object": portable.actor_id,
    });
    let local_key = plamenu::key_store::account_signing_key(&state, local.id, "rsa")
        .await
        .unwrap();
    let local_signer = RequestSigner::from_pkcs8_pem(
        local_key.private.expose_str().unwrap(),
        local_key.record.key_uri.clone(),
    )
    .unwrap();
    assert_eq!(
        post_remote_signed(&app, &portable.inbox_path, &local_follow, &local_signer).await,
        StatusCode::ACCEPTED
    );
    let accept_local = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/accept-local", portable.actor_id),
        "type": "Accept",
        "actor": portable.actor_id,
        "object": local_follow["id"],
        "to": local_actor,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &accept_local)
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert!(
        follow::find(&pool, local.id, portable_account.id)
            .await
            .unwrap()
            .is_some_and(|edge| !edge.pending)
    );
    let note = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/objects/note-1", portable.actor_id),
        "type": "Note",
        "attributedTo": portable.actor_id,
        "content": "hello from Minimitra",
        "to": [plamenu_ap::activity::PUBLIC, local_actor],
        "cc": format!("{}/followers", portable.actor_id),
    }));
    let create = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/create-1", portable.actor_id),
        "type": "Create",
        "actor": portable.actor_id,
        "object": note,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &create).await.0,
        StatusCode::ACCEPTED
    );
    let stored_portable_post = status::find_by_uri(&pool, note["id"].as_str().unwrap())
        .await
        .unwrap()
        .expect("portable Create is materialized as a normal status");
    assert_eq!(stored_portable_post.account_id, portable_account.id);
    assert_eq!(status::count_local(&pool).await.unwrap(), 1);
    assert!(
        status::home_timeline(&pool, local.id, TimelineOrder::default(), None, 20)
            .await
            .unwrap()
            .iter()
            .any(|item| item.id == stored_portable_post.id),
        "accepted local followers receive portable posts on the normal home timeline"
    );

    // Optimized community timelines inline their moderation predicates. Keep
    // them aligned with the shared portable-account rules too: blocking the
    // gateway's own domain must not hide a portable author's attributed post.
    let host_block = instance_policy::create_domain_block(
        &pool,
        instance_policy::NewDomainBlock {
            domain: TEST_DOMAIN,
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
    account_domain_block::create(&pool, local.id, TEST_DOMAIN)
        .await
        .unwrap();
    assert!(
        group::attributed_timeline(
            &pool,
            local.id,
            Some(local.id),
            group::TimelineSort::New,
            20,
            0,
        )
        .await
        .unwrap()
        .iter()
        .any(|item| item.id == stored_portable_post.id),
        "portable post was treated as remote from the gateway host in an attributed timeline"
    );
    account_domain_block::delete(&pool, local.id, TEST_DOMAIN)
        .await
        .unwrap();
    instance_policy::delete_domain_block(&pool, host_block.id)
        .await
        .unwrap();

    let local_timeline = Request::builder()
        .uri("/api/v1/timelines/public?local=true")
        .body(Body::empty())
        .unwrap();
    let (timeline_code, timeline_bytes) = send(&app, local_timeline).await;
    assert_eq!(timeline_code, StatusCode::OK);
    let timeline: Value = serde_json::from_slice(&timeline_bytes).unwrap();
    assert!(
        timeline
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["uri"] == note["id"]),
        "portable status is missing from the local timeline: {timeline}"
    );

    let statuses_path = format!("/api/v1/accounts/{}/statuses", portable_account.id);
    let statuses_request = Request::builder()
        .uri(statuses_path)
        .body(Body::empty())
        .unwrap();
    let (statuses_code, statuses_bytes) = send(&app, statuses_request).await;
    assert_eq!(statuses_code, StatusCode::OK);
    let statuses: Value = serde_json::from_slice(&statuses_bytes).unwrap();
    assert_eq!(statuses[0]["content"], "hello from Minimitra");

    let atom = Request::builder()
        .uri("/@alice.atom")
        .body(Body::empty())
        .unwrap();
    let (atom_status, atom_bytes) = send(&app, atom).await;
    assert_eq!(atom_status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&atom_bytes).contains("hello from Minimitra"),
        "portable projected posts are present in local account syndication"
    );

    // Same-instance interactions are projected directly rather than posted
    // back through Plamenu's own HTTP inbox.
    let local_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(local.id, "a local post", "public", None),
    )
    .await
    .unwrap();
    let local_post_uri =
        plamenu::entities::status_uri_for_account(TEST_DOMAIN, &local_post, &local);
    let like_local = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/like-local", portable.actor_id),
        "type": "Like",
        "actor": portable.actor_id,
        "object": local_post_uri,
        "to": local_actor,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &like_local).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        favourite::favourited_of(&pool, portable_account.id, &[local_post.id])
            .await
            .unwrap(),
        [local_post.id]
    );

    // Activity addressed to the portable actor is both retained for client
    // polling and projected normally, so threads/interactions are usable from
    // either interface.
    let bob_note_id = format!("{}/statuses/to-portable", bob.actor.id);
    let bob_create = json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": format!("{}#create/portable", bob.actor.id),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": bob_note_id,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "hello to the portable account",
            "to": [portable.actor_id, second_portable.actor_id],
            "published": PortableUser::timestamp(),
        }
    });
    assert_eq!(
        post_remote_signed(&app, &portable.inbox_path, &bob_create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &bob_note_id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        post_remote_signed(
            &app,
            &second_portable.inbox_path,
            &bob_create,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let second_signer = second_portable.request_signer();
    let (second_inbox_status, second_inbox) = get_client_signed(
        &app,
        &second_portable.inbox_path,
        &second_portable.inbox_path,
        &second_signer,
    )
    .await;
    assert_eq!(second_inbox_status, StatusCode::OK);
    assert_eq!(second_inbox["orderedItems"], json!([bob_create]));

    // A portable follow of an ordinary local account uses Plamenu's normal
    // unlocked-account acceptance logic. Only the resulting Accept needs a
    // delivery job; the original Follow is not looped through our own inbox.
    let follow_local = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/follow-local", portable.actor_id),
        "type": "Follow",
        "actor": portable.actor_id,
        "object": local_actor,
        "to": local_actor,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &follow_local)
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert!(
        follow::find(&pool, portable_account.id, local.id)
            .await
            .unwrap()
            .is_some_and(|edge| !edge.pending)
    );

    // Remote moderation reports resolve the gateway actor as a local account
    // target and retain projected portable statuses as evidence. This is the
    // same staff queue used for reports about Plamenu-owned accounts.
    let portable_flag = json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": format!("{}/reports/portable", bob.actor.id),
        "type": "Flag",
        "actor": bob.actor.id,
        "content": "portable account moderation test",
        "object": [portable.actor_id, note["id"]],
    });
    assert_eq!(
        post_remote_signed(&app, "/inbox", &portable_flag, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let bob_account = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    let reports = report::list_by_reporter(&pool, bob_account.id)
        .await
        .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].target_account_id, portable_account.id);
    assert_eq!(reports[0].status_ids, vec![stored_portable_post.id]);
    assert_eq!(reports[0].comment, "portable account moderation test");

    assert_eq!(plamenu_db::job::pending_count(&pool).await.unwrap(), 5);

    common::deliver_all_due(&state).await;
    let deliveries = stub.deliveries();
    assert_eq!(deliveries.len(), 5);
    let portable_deliveries: Vec<_> = deliveries
        .iter()
        .filter(|delivery| delivery.activity["actor"] == portable.actor_id)
        .collect();
    assert_eq!(portable_deliveries.len(), 3);
    assert!(
        portable_deliveries
            .iter()
            .all(|delivery| { delivery.key_id == format!("{}#gateway-rsa", portable.actor_id) })
    );
    assert!(deliveries.iter().any(|delivery| {
        delivery.inbox_url == format!("{}/inbox", portable.actor_id)
            && delivery.activity["type"] == "Accept"
            && delivery.activity["actor"] == local_actor
    }));
    let delivered_creates: Vec<_> = deliveries
        .iter()
        .filter(|delivery| delivery.activity["type"] == "Create")
        .collect();
    assert_eq!(delivered_creates.len(), 1);
    assert!(
        delivered_creates
            .iter()
            .all(|delivery| delivery.activity == create),
        "portable JSON was rewritten"
    );

    // Raw follower fan-out observes the same account moderation and block
    // state as the ordinary relationship graph. A suspended peer receives no
    // portable posts, and blocking it removes its client-delivery edge.
    assert_eq!(
        gateway::follower_inboxes(&pool, portable_account.id)
            .await
            .unwrap(),
        vec![bob_account.preferred_inbox().to_owned()]
    );
    account::suspend(&pool, bob_account.id, "local")
        .await
        .unwrap();
    assert!(
        gateway::follower_inboxes(&pool, portable_account.id)
            .await
            .unwrap()
            .is_empty()
    );
    account::unsuspend(&pool, bob_account.id).await.unwrap();
    let block_bob = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/block-bob", portable.actor_id),
        "type": "Block",
        "actor": portable.actor_id,
        "object": bob.actor.id,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &block_bob).await.0,
        StatusCode::ACCEPTED
    );
    assert!(
        gateway::follower_inboxes(&pool, portable_account.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        follow::find(&pool, bob_account.id, portable_account.id)
            .await
            .unwrap()
            .is_none()
    );

    // IDs are never overwritten and cannot be reused; an actor mismatch is a
    // distinct 403 as required by FEP-ae97.
    assert_eq!(
        post_json(&app, &portable.outbox_path, &create).await.0,
        StatusCode::BAD_REQUEST
    );
    let reused_note = portable.sign(&json!({
        "id": format!("{}/objects/reused-side-effect", portable.actor_id),
        "type": "Note",
        "attributedTo": portable.actor_id,
        "content": "must never be projected",
        "to": plamenu_ap::activity::PUBLIC,
    }));
    let reused_create = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": create["id"],
        "type": "Create",
        "actor": portable.actor_id,
        "object": reused_note,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &reused_create)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert!(
        status::find_by_uri(&pool, reused_note["id"].as_str().unwrap())
            .await
            .unwrap()
            .is_none(),
        "a rejected reused activity ID produced a status side effect"
    );
    let wrong_actor = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/wrong-actor", portable.actor_id),
        "type": "Delete",
        "actor": format!("{}/other", portable.actor_id),
        "object": format!("{}/other", portable.actor_id),
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &wrong_actor).await.0,
        StatusCode::FORBIDDEN
    );
    let claimed_key = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/objects/key-claim", portable.actor_id),
        "type": "Note",
        "attributedTo": portable.actor_id,
        "content": "not actually a note",
        "publicKeyMultibase": plamenu_ap::multikey::encode_ed25519_public(&[4; 32]),
    }));
    let claimed_key_create = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/key-claim", portable.actor_id),
        "type": "Create",
        "actor": portable.actor_id,
        "object": claimed_key,
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &claimed_key_create)
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    // Binary media uses the client's HTTP key, a SHA-256 hashlink, direct
    // serving at the advertised path, and a signed bodyless DELETE.
    let media_path = "/.well-known/apgateway-media";
    let media_bytes = b"not-a-decoded-image-but-valid-gateway-bytes";
    let request_headers =
        client_signer.sign_post(TEST_DOMAIN, media_path, media_bytes, SystemTime::now());
    let upload = Request::builder()
        .method(Method::POST)
        .uri(media_path)
        .header(header::HOST, request_headers.host)
        .header(header::DATE, request_headers.date)
        .header("digest", request_headers.digest)
        .header("signature", request_headers.signature)
        .header(header::CONTENT_TYPE, "image/png")
        .body(Body::from(media_bytes.as_slice()))
        .unwrap();
    let (status, bytes) = send(&app, upload).await;
    assert_eq!(status, StatusCode::CREATED);
    let media: Value = serde_json::from_slice(&bytes).unwrap();
    let hashlink = media["url"].as_str().unwrap();
    assert!(hashlink.starts_with("hl:zQm"), "{hashlink}");
    let stored_path = format!("{media_path}/{hashlink}");
    let download = Request::builder()
        .uri(&stored_path)
        .body(Body::empty())
        .unwrap();
    let (status, downloaded) = send(&app, download).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, media_bytes);

    let request_headers = client_signer.sign_delete(TEST_DOMAIN, &stored_path, SystemTime::now());
    let delete = Request::builder()
        .method(Method::DELETE)
        .uri(&stored_path)
        .header(header::HOST, TEST_DOMAIN)
        .header(header::DATE, request_headers.date)
        .header("signature", request_headers.signature)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, delete).await.0, StatusCode::NO_CONTENT);
    let missing = Request::builder()
        .uri(&stored_path)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, missing).await.0, StatusCode::NOT_FOUND);

    // Plamenu moderation acts on the portable account without claiming its
    // identity: the profile/status graph is hidden locally and the gateway
    // refuses further client writes/polling. The raw client-owned actor was
    // never rewritten as a Plamenu actor.
    let moderator = common::create_immutable_local_account(&pool, "moderator", "Moderator").await;
    let admin_role = role::find_by_name(&pool, "Admin").await.unwrap().unwrap();
    plamenu::moderation::apply_account_action(
        &state,
        &admin_role,
        moderator.id,
        &portable_account,
        "suspend",
        "portable interoperability test",
        None,
    )
    .await
    .unwrap();
    let suspended = account::find_by_id(&pool, portable_account.id)
        .await
        .unwrap()
        .unwrap();
    assert!(suspended.suspended());
    let blocked_create = portable.sign(&json!({
        "@context": [plamenu_ap::AS_CONTEXT, "https://w3id.org/security/data-integrity/v1"],
        "id": format!("{}/activities/after-suspend", portable.actor_id),
        "type": "Create",
        "actor": portable.actor_id,
        "object": portable.sign(&json!({
            "id": format!("{}/objects/after-suspend", portable.actor_id),
            "type": "Note",
            "attributedTo": portable.actor_id,
            "content": "must not be accepted",
            "to": plamenu_ap::activity::PUBLIC,
        })),
    }));
    assert_eq!(
        post_json(&app, &portable.outbox_path, &blocked_create)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let restored = plamenu::moderation::unsuspend_account(&state, &suspended)
        .await
        .expect("portable unsuspension must not fetch or rewrite its actor");
    assert!(!restored.suspended());
    assert_eq!(
        get_client_signed(
            &app,
            &portable.inbox_path,
            &portable.inbox_path,
            &client_signer,
        )
        .await
        .0,
        StatusCode::OK
    );
    plamenu::moderation::apply_account_action(
        &state,
        &admin_role,
        moderator.id,
        &restored,
        "suspend",
        "portable interoperability test (second suspension)",
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        get_client_signed(
            &app,
            &portable.inbox_path,
            &portable.inbox_path,
            &client_signer,
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ephemeral_webxdc_packets_never_enter_portable_collections(pool: PgPool) {
    open_registrations(&pool).await;
    let state = test_state_with(pool.clone(), StubFederation::default().into());
    let app = build_router(state);
    let portable = PortableUser::new("ephemeral-owner");
    assert_eq!(
        post_json(&app, "/.well-known/apgateway", &portable.actor("alice"))
            .await
            .0,
        StatusCode::CREATED
    );
    let packet = plamenu::webxdc_realtime::packet(
        "https://coordinator.example/webxdc/1",
        &portable.actor_id,
        &[0, 255],
    );
    assert_eq!(
        post_json(&app, &portable.outbox_path, &portable.sign(&packet))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let remote = RemoteUser::new("remote.example", "packet-sender");
    let packet = plamenu::webxdc_realtime::packet(
        "https://coordinator.example/webxdc/1",
        &remote.actor.id,
        &[0, 255],
    );
    assert_eq!(
        post_remote_signed(&app, &portable.inbox_path, &packet, &remote.signer()).await,
        StatusCode::BAD_REQUEST
    );
    let owner = account::find_local_account_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert!(
        gateway::find_collection_item(&pool, owner.id, "inbox", packet["id"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
}
