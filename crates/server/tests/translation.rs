//! Status translation (M25): the `POST /statuses/{id}/translate` endpoint and
//! the `translation_languages` / `translation.enabled` instance surface,
//! against a stubbed `LibreTranslate` / `DeepL` backend.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{StubFederation, TEST_DOMAIN, create_local_account, test_app, test_state_translation};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::build_router;
use plamenu::config::{DeepLPlan, TranslationConfig};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, oauth, poll, status, user};
use serde_json::{Value, json};
use tower::ServiceExt;

const LIBRE_ENDPOINT: &str = "http://libretranslate.test";

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password("pw").unwrap();
    let row = user::create(
        pool,
        account.id,
        Some(&format!("{username}@plamenu.test")),
        &hash,
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "translation-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        pool,
        &hash_secret(&token),
        app.id,
        Some(row.id),
        "read write",
    )
    .await
    .unwrap();
    (account, token)
}

/// A local status with an explicit language (translation is source-language
/// keyed).
async fn post_es(
    pool: &PgPool,
    account_id: i64,
    content: &str,
    visibility: &str,
) -> status::Status {
    status::create_local(
        pool,
        status::NewLocalStatus {
            content,
            language: Some("es"),
            ..status::NewLocalStatus::new(account_id, content, visibility, None)
        },
    )
    .await
    .unwrap()
}

fn libre() -> TranslationConfig {
    TranslationConfig::LibreTranslate {
        endpoint: LIBRE_ENDPOINT.to_owned(),
        api_key: None,
    }
}

/// The standard `LibreTranslate` `/languages` reply: es↔en↔de.
fn serve_libre_languages(fed: &StubFederation) {
    fed.serve_service(
        &format!("{LIBRE_ENDPOINT}/languages"),
        200,
        &json!([
            { "code": "en", "name": "English", "targets": ["es", "de"] },
            { "code": "es", "name": "Spanish", "targets": ["en", "de"] },
            { "code": "de", "name": "German", "targets": ["en", "es"] },
        ])
        .to_string(),
    );
}

async fn api_on(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    accept_language: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(lang) = accept_language {
        builder = builder.header(header::ACCEPT_LANGUAGE, lang);
    }
    let request = builder.body(Body::empty()).unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn translate_public_status_libre(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = post_es(&pool, alice.id, "<p>Hola mundo</p>", "public").await;

    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);
    fed.serve_service(
        &format!("{LIBRE_ENDPOINT}/translate"),
        200,
        &json!({
            "translatedText": ["<p>Hello world</p>"],
            "detectedLanguage": [{ "confidence": 100, "language": "es" }],
        })
        .to_string(),
    );

    let app = build_router(test_state_translation(pool, fed.clone(), libre()));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;

    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["content"], "<p>Hello world</p>");
    assert_eq!(body["language"], "en");
    assert_eq!(body["detected_source_language"], "es");
    assert_eq!(body["provider"], "LibreTranslate");

    // The backend saw a languages fetch and a translate POST.
    let urls: Vec<String> = fed
        .service_requests()
        .iter()
        .map(|r| r.url.clone())
        .collect();
    assert!(
        urls.contains(&format!("{LIBRE_ENDPOINT}/translate")),
        "{urls:?}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn translate_spoiler_and_poll(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>Hola</p>",
            spoiler_text: "aviso",
            language: Some("es"),
            ..status::NewLocalStatus::new(alice.id, "<p>Hola</p>", "public", None)
        },
    )
    .await
    .unwrap();
    poll::create(
        &pool,
        poll::NewPoll {
            status_id: status.id,
            account_id: alice.id,
            options: &["uno".to_owned(), "dos".to_owned()],
            cached_tallies: &[0, 0],
            multiple: false,
            hide_totals: false,
            voters_count: None,
            expires_at: None,
        },
    )
    .await
    .unwrap();

    // The fragments (content, spoiler, 2 poll options) come back positionally
    // in the same order they are sent.
    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);
    fed.serve_service(
        &format!("{LIBRE_ENDPOINT}/translate"),
        200,
        &json!({
            "translatedText": ["<p>Hi</p>", "warning", "one", "two"],
            "detectedLanguage": [],
        })
        .to_string(),
    );

    let app = build_router(test_state_translation(pool.clone(), fed, libre()));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;

    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["content"], "<p>Hi</p>");
    assert_eq!(body["spoiler_text"], "warning");
    assert_eq!(body["poll"]["id"], status.id.to_string());
    assert_eq!(body["poll"]["options"][0]["title"], "one");
    assert_eq!(body["poll"]["options"][1]["title"], "two");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn own_private_status_is_forbidden(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = post_es(&pool, alice.id, "<p>secreto</p>", "private").await;

    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);

    let app = build_router(test_state_translation(pool, fed, libre()));
    let (code, _) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;
    // Viewable to its author, but not distributable → 403, not 404.
    assert_eq!(code, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn someone_elses_private_status_is_not_found(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let status = post_es(&pool, bob.id, "<p>secreto</p>", "private").await;

    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);

    let app = build_router(test_state_translation(pool, fed, libre()));
    let (code, _) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn no_backend_translate_is_not_found(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = post_es(&pool, alice.id, "<p>Hola</p>", "public").await;

    // Default app: no translation backend configured.
    let (code, _) = api_on(
        test_app(pool),
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn quota_exceeded_is_service_unavailable(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = post_es(&pool, alice.id, "<p>Hola</p>", "public").await;

    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);
    fed.serve_service(&format!("{LIBRE_ENDPOINT}/translate"), 403, "quota");

    let app = build_router(test_state_translation(pool, fed, libre()));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body["error"].as_str().unwrap().contains("quota"), "{body}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn translation_languages_and_instance_flag(pool: PgPool) {
    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);
    let app = build_router(test_state_translation(pool.clone(), fed, libre()));

    let (code, langs) = api_on(
        app.clone(),
        "GET",
        "/api/v1/instance/translation_languages",
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{langs}");
    assert_eq!(langs["es"], json!(["en", "de"]));
    // The auto/nil source is exposed as `und`.
    assert!(langs.get("und").is_some(), "{langs}");

    let (code, instance) = api_on(app, "GET", "/api/v2/instance", None, None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(instance["configuration"]["translation"]["enabled"], true);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_flag_off_without_backend(pool: PgPool) {
    let (code, instance) = api_on(
        test_app(pool.clone()),
        "GET",
        "/api/v2/instance",
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(instance["configuration"]["translation"]["enabled"], false);

    let (code, langs) = api_on(
        test_app(pool),
        "GET",
        "/api/v1/instance/translation_languages",
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(langs, json!({}));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn translate_with_deepl_backend(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = post_es(&pool, alice.id, "<p>Hola</p>", "public").await;

    let fed: Arc<StubFederation> = Arc::default();
    // DeepL fetches source and target language lists separately.
    fed.serve_service(
        "https://api-free.deepl.com/v2/languages?type=source",
        200,
        &json!([{ "language": "ES" }, { "language": "EN" }]).to_string(),
    );
    fed.serve_service(
        "https://api-free.deepl.com/v2/languages?type=target",
        200,
        &json!([{ "language": "EN-US" }, { "language": "DE" }]).to_string(),
    );
    fed.serve_service(
        "https://api-free.deepl.com/v2/translate",
        200,
        &json!({
            "translations": [
                { "detected_source_language": "ES", "text": "<p>Hello</p>" }
            ]
        })
        .to_string(),
    );

    let backend = TranslationConfig::DeepL {
        plan: DeepLPlan::Free,
        api_key: "secret".to_owned(),
    };
    let app = build_router(test_state_translation(pool, fed.clone(), backend));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        // DeepL lists EN-US but not bare EN; the base-subtag narrowing makes
        // `en` resolve.
        Some("en"),
    )
    .await;

    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["content"], "<p>Hello</p>");
    assert_eq!(body["provider"], "DeepL.com");
    assert_eq!(body["detected_source_language"], "es");

    // The translate POST carried the DeepL auth header and a form body.
    let translate = fed
        .service_requests()
        .into_iter()
        .find(|r| r.url.ends_with("/v2/translate"))
        .unwrap();
    assert!(
        translate
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "DeepL-Auth-Key secret"),
        "{:?}",
        translate.headers
    );
    let body = translate.body.unwrap();
    assert!(body.contains("tag_handling=html"), "{body}");
    assert!(body.contains("source_lang=ES"), "{body}");
    let _ = TEST_DOMAIN;
}

// ---------------------------------------------------------------------------
// Persistent cache

/// A second server instance (fresh in-memory caches, same database) serves a
/// stored translation without touching the backend.
#[sqlx::test(migrations = "../db/migrations")]
async fn translation_cached_across_restarts(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = post_es(&pool, alice.id, "<p>Hola mundo</p>", "public").await;
    let path = format!("/api/v1/statuses/{}/translate", status.id);

    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);
    fed.serve_service(
        &format!("{LIBRE_ENDPOINT}/translate"),
        200,
        &json!({
            "translatedText": ["<p>Hello world</p>"],
            "detectedLanguage": [{ "confidence": 100, "language": "es" }],
        })
        .to_string(),
    );
    let app = build_router(test_state_translation(pool.clone(), fed, libre()));
    let (code, _) = api_on(app, "POST", &path, Some(&token), Some("en")).await;
    assert_eq!(code, StatusCode::OK);

    // "Restart": new state over the same pool; the stub only knows the
    // language list, so any backend call would fail the request.
    let restarted: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&restarted);
    let app = build_router(test_state_translation(
        pool.clone(),
        restarted.clone(),
        libre(),
    ));
    let (code, body) = api_on(app, "POST", &path, Some(&token), Some("en")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["content"], "<p>Hello world</p>");
    assert_eq!(body["detected_source_language"], "es");
    assert_eq!(body["provider"], "LibreTranslate");
    let translate_calls = restarted
        .service_requests()
        .iter()
        .filter(|r| r.url.ends_with("/translate"))
        .count();
    assert_eq!(translate_calls, 0, "cached row must satisfy the request");
}

/// Editing a status changes the fragment hash, so the stored row is a miss
/// and the backend is consulted again.
#[sqlx::test(migrations = "../db/migrations")]
async fn edited_status_invalidates_cached_translation(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = post_es(&pool, alice.id, "<p>Hola mundo</p>", "public").await;
    let path = format!("/api/v1/statuses/{}/translate", status.id);

    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);
    fed.serve_service(
        &format!("{LIBRE_ENDPOINT}/translate"),
        200,
        &json!({ "translatedText": ["<p>Hello world</p>"] }).to_string(),
    );
    let app = build_router(test_state_translation(pool.clone(), fed, libre()));
    let (code, _) = api_on(app, "POST", &path, Some(&token), Some("en")).await;
    assert_eq!(code, StatusCode::OK);

    sqlx::query!(
        "UPDATE statuses SET content = '<p>Adiós mundo</p>' WHERE id = $1",
        status.id,
    )
    .execute(&pool)
    .await
    .unwrap();

    let after_edit: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&after_edit);
    after_edit.serve_service(
        &format!("{LIBRE_ENDPOINT}/translate"),
        200,
        &json!({ "translatedText": ["<p>Goodbye world</p>"] }).to_string(),
    );
    let app = build_router(test_state_translation(
        pool.clone(),
        after_edit.clone(),
        libre(),
    ));
    let (code, body) = api_on(app, "POST", &path, Some(&token), Some("en")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["content"], "<p>Goodbye world</p>");
    let translate_calls = after_edit
        .service_requests()
        .iter()
        .filter(|r| r.url.ends_with("/translate"))
        .count();
    assert_eq!(translate_calls, 1, "edit must force a backend call");
}

/// The per-account budget counts backend calls only: a cache hit stays free
/// after the limit is exhausted.
#[sqlx::test(migrations = "../db/migrations")]
async fn backend_rate_limit_spares_cache_hits(pool: PgPool) {
    sqlx::query!("UPDATE instance_settings SET translation_user_rate_limit_per_hour = 1",)
        .execute(&pool)
        .await
        .unwrap();
    let (alice, token) = user_with_token(&pool, "alice").await;
    let first = post_es(&pool, alice.id, "<p>Hola</p>", "public").await;
    let second = post_es(&pool, alice.id, "<p>Mundo</p>", "public").await;

    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed);
    fed.serve_service(
        &format!("{LIBRE_ENDPOINT}/translate"),
        200,
        &json!({ "translatedText": ["<p>Hi</p>"] }).to_string(),
    );
    let app = build_router(test_state_translation(pool.clone(), fed, libre()));

    let (code, _) = api_on(
        app.clone(),
        "POST",
        &format!("/api/v1/statuses/{}/translate", first.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // Second *backend* translation: over budget.
    let (code, _) = api_on(
        app.clone(),
        "POST",
        &format!("/api/v1/statuses/{}/translate", second.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);

    // Repeating the first is a cache hit — still allowed.
    let (code, _) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", first.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// OpenAI-compatible backend (llama.cpp / Hy-MT2)

const OPENAI_ENDPOINT: &str = "http://mt.test";

fn openai() -> TranslationConfig {
    TranslationConfig::OpenAi {
        endpoint: OPENAI_ENDPOINT.to_owned(),
        model: "hy-mt2-1.8b".to_owned(),
        api_key: None,
        languages: vec!["en".to_owned(), "es".to_owned(), "de".to_owned()],
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn translate_with_openai_backend(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = post_es(&pool, alice.id, "<p>Hola mundo</p>", "public").await;

    let fed: Arc<StubFederation> = Arc::default();
    fed.serve_service(
        &format!("{OPENAI_ENDPOINT}/v1/chat/completions"),
        200,
        &json!({
            "choices": [{ "message": { "role": "assistant", "content": "<p>Hello world</p>" } }],
        })
        .to_string(),
    );

    let app = build_router(test_state_translation(pool, fed.clone(), openai()));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;

    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["content"], "<p>Hello world</p>");
    assert_eq!(body["provider"], "hy-mt2-1.8b");
    // No detection support: falls back to the status' declared language.
    assert_eq!(body["detected_source_language"], "es");

    // The prompt named the target language in English and carried the text;
    // the request asked for the configured model.
    let requests = fed.service_requests();
    let translate = requests
        .iter()
        .find(|r| r.url.ends_with("/v1/chat/completions"))
        .expect("chat completion requested");
    let sent: Value = serde_json::from_str(translate.body.as_deref().unwrap()).unwrap();
    assert_eq!(sent["model"], "hy-mt2-1.8b");
    let prompt = sent["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("into English"), "{prompt}");
    assert!(prompt.contains("Hola mundo"), "{prompt}");
}

/// The `languages` list drives the instance surface without a backend call.
#[sqlx::test(migrations = "../db/migrations")]
async fn openai_language_map_is_static(pool: PgPool) {
    let fed: Arc<StubFederation> = Arc::default();
    let app = build_router(test_state_translation(pool, fed.clone(), openai()));
    let (code, body) = api_on(
        app,
        "GET",
        "/api/v1/instance/translation_languages",
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["und"], json!(["en", "es", "de"]));
    assert_eq!(body["es"], json!(["en", "de"]));
    assert!(fed.service_requests().is_empty(), "no backend calls");
}

/// A post whose language the backend doesn't list is refused with a specific
/// message, not a generic failure — the fediverse speaks more languages than
/// any backend inventory.
#[sqlx::test(migrations = "../db/migrations")]
async fn unsupported_source_language_is_a_specific_403(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>Sää on tänään kamala</p>",
            language: Some("fi"),
            ..status::NewLocalStatus::new(alice.id, "<p>Sää on tänään kamala</p>", "public", None)
        },
    )
    .await
    .unwrap();

    let fed: Arc<StubFederation> = Arc::default();
    serve_libre_languages(&fed); // en/es/de only
    let app = build_router(test_state_translation(pool, fed, libre()));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert_eq!(
        body["error"],
        "Translating from Finnish is not supported by the translation service"
    );
}

/// A multi-fragment post goes to the `OpenAI` backend as ONE structured-JSON
/// request; the title fragment rides along and lands in the entity.
#[sqlx::test(migrations = "../db/migrations")]
async fn openai_translates_title_and_fragments_in_one_request(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>Hola mundo</p>",
            language: Some("es"),
            spoiler_text: "aviso",
            title: Some("Un título importante"),
            ..status::NewLocalStatus::new(alice.id, "<p>Hola mundo</p>", "public", None)
        },
    )
    .await
    .unwrap();

    let fed: Arc<StubFederation> = Arc::default();
    fed.serve_service(
        &format!("{OPENAI_ENDPOINT}/v1/chat/completions"),
        200,
        &json!({
            "choices": [{ "message": { "role": "assistant", "content":
                "{\"f1\": \"An important title\", \"f2\": \"<p>Hello world</p>\", \"f3\": \"warning\"}" } }],
        })
        .to_string(),
    );
    let app = build_router(test_state_translation(pool, fed.clone(), openai()));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "An important title");
    assert_eq!(body["content"], "<p>Hello world</p>");
    assert_eq!(body["spoiler_text"], "warning");

    // One structured request carried all three fragments, with the widened
    // translate timeout.
    let requests = fed.service_requests();
    let chats: Vec<_> = requests
        .iter()
        .filter(|r| r.url.ends_with("/v1/chat/completions"))
        .collect();
    assert_eq!(chats.len(), 1, "one structured request");
    assert_eq!(chats[0].timeout_secs, Some(180));
    let sent: Value = serde_json::from_str(chats[0].body.as_deref().unwrap()).unwrap();
    let prompt = sent["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("\"f1\""), "{prompt}");
    assert!(prompt.contains("Un título importante"), "{prompt}");
}

/// When the structured reply isn't valid JSON, the backend falls back to one
/// plain request per fragment instead of failing the translation.
#[sqlx::test(migrations = "../db/migrations")]
async fn openai_structured_failure_falls_back_per_fragment(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>Hola</p>",
            language: Some("es"),
            spoiler_text: "aviso",
            ..status::NewLocalStatus::new(alice.id, "<p>Hola</p>", "public", None)
        },
    )
    .await
    .unwrap();

    let fed: Arc<StubFederation> = Arc::default();
    // No braces at all: the structured parse gives up, the per-fragment
    // path then treats the same canned reply as the translated text.
    fed.serve_service(
        &format!("{OPENAI_ENDPOINT}/v1/chat/completions"),
        200,
        &json!({
            "choices": [{ "message": { "role": "assistant", "content": "not json at all" } }],
        })
        .to_string(),
    );
    let app = build_router(test_state_translation(pool, fed.clone(), openai()));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let chats = fed
        .service_requests()
        .iter()
        .filter(|r| r.url.ends_with("/v1/chat/completions"))
        .count();
    assert_eq!(chats, 3, "structured attempt + one per fragment");
}

/// A title-only Page (Lemmy's common shape) translates its title.
#[sqlx::test(migrations = "../db/migrations")]
async fn title_only_page_translates(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "",
            language: Some("es"),
            title: Some("Solo un título"),
            ..status::NewLocalStatus::new(alice.id, "", "public", None)
        },
    )
    .await
    .unwrap();

    let fed: Arc<StubFederation> = Arc::default();
    fed.serve_service(
        &format!("{OPENAI_ENDPOINT}/v1/chat/completions"),
        200,
        &json!({
            "choices": [{ "message": { "role": "assistant", "content": "Just a title" } }],
        })
        .to_string(),
    );
    let app = build_router(test_state_translation(pool, fed, openai()));
    let (code, body) = api_on(
        app,
        "POST",
        &format!("/api/v1/statuses/{}/translate", status.id),
        Some(&token),
        Some("en"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "Just a title");
    assert_eq!(body["content"], "");
}
