//! Integration tests for the first-party web UI: the session cookie flow and
//! the page shell, driven through tower without binding a socket.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, StubFederation, create_local_account};
use http_body_util::BodyExt;
use plamenu::auth::verify_password;
use plamenu::{build_router, remote};
use plamenu_db::{
    PgPool, account, account_domain_block, account_moderation_note, account_warning,
    admin_action_log, announcement, appeal, block, custom_emoji, custom_filter, email, endorsement,
    featured_tag, follow, id, instance_policy, invite, media, mute, notification,
    notification_policy, notification_request, oauth, preview_card, preview_card_trend,
    reachability, reaction, remote_history as history_db, report, report_note, role,
    rule as db_rule, scheduled_status, software_update, status, status_trend, tag, tag_trend,
    terms_of_service, user, username_block, warning_preset, webhook,
};
use tower::ServiceExt;

const EMAIL: &str = "alice@example.com";
const PASSWORD: &str = "correct horse battery";

/// Creates a local account with login credentials; returns the account.
async fn seed_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    password: &str,
) -> plamenu_db::account::Account {
    let account = create_local_account(pool, username, username).await;
    let hash = plamenu::auth::hash_password(password).unwrap();
    plamenu_db::user::create(pool, account.id, Some(email), &hash)
        .await
        .unwrap();
    account
}

/// Creates `@alice` with login credentials, leaving the pool open for more
/// fixtures; returns the account.
async fn seed_alice(pool: &PgPool) -> plamenu_db::account::Account {
    seed_user(pool, "alice", EMAIL, PASSWORD).await
}

/// Creates `@alice` with login credentials and returns the ready router.
async fn app_with_alice(pool: PgPool) -> Router {
    seed_alice(&pool).await;
    // The historic open posture: most web tests predate the private default
    // and exercise anonymous reads.
    common::open_previews(&pool).await;
    common::test_app(pool)
}

struct Resp {
    status: StatusCode,
    location: Option<String>,
    set_cookie: Option<String>,
    content_type: String,
    cache_control: String,
    body: String,
}

async fn send(app: &Router, request: Request<Body>) -> Resp {
    let response = app.clone().oneshot(request).await.unwrap();
    let header = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    };
    let status = response.status();
    let location = header(header::LOCATION);
    let set_cookie = header(header::SET_COOKIE);
    let content_type = header(header::CONTENT_TYPE).unwrap_or_default();
    let cache_control = header(header::CACHE_CONTROL).unwrap_or_default();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    // Fluent wraps every interpolated `{ $var }` placeable in bidi isolation
    // marks (U+2068 FSI / U+2069 PDI). They are invisible control characters
    // that these tests never assert on, so strip them here to match responses
    // against the text as a reader sees it. JSON API bodies never carry them.
    let body = String::from_utf8(bytes.to_vec())
        .unwrap()
        .replace(['\u{2068}', '\u{2069}'], "");
    Resp {
        status,
        location,
        set_cookie,
        content_type,
        cache_control,
        body,
    }
}

async fn get(app: &Router, uri: &str, cookie: Option<&str>) -> Resp {
    let mut request = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

/// GETs `uri` with an `ActivityPub` `Accept`, exactly as a federating peer that
/// dereferences a shareable link (searching by URL) would. Anonymous, like a
/// server-to-server object fetch.
async fn get_ap(app: &Router, uri: &str) -> Resp {
    let request = Request::builder()
        .uri(uri)
        .header(header::ACCEPT, "application/activity+json");
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn post_login(app: &Router, identifier: &str, password: &str) -> Resp {
    let body =
        serde_urlencoded::to_string([("identifier", identifier), ("password", password)]).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

/// The `name=value` head of a `Set-Cookie`, ready to send back as `Cookie`.
fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

/// Signs in with the given credentials and returns the session cookie pair.
async fn login_as(app: &Router, email: &str, password: &str) -> String {
    cookie_pair(
        &post_login(app, email, password)
            .await
            .set_cookie
            .expect("session cookie"),
    )
    .to_owned()
}

/// Signs in as `@alice` and returns the session cookie pair.
async fn login(app: &Router) -> String {
    login_as(app, EMAIL, PASSWORD).await
}

/// Pulls the first embedded CSRF token out of a rendered page's forms.
fn csrf_of(body: &str) -> String {
    let marker = r#"name="csrf" value=""#;
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

/// POSTs a urlencoded form with the session cookie attached.
async fn post_form(app: &Router, uri: &str, cookie: &str, fields: &[(&str, &str)]) -> Resp {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

/// POSTs the URL-encoded edit preview request used by the JavaScript enhancer.
async fn edit_preview_fragment(
    app: &Router,
    uri: &str,
    cookie: &str,
    fields: &[(&str, &str)],
) -> Resp {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("X-Compose-Preview", "1")
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

/// Signs in, composes a status, and returns its thread path (`/@alice/{id}`).
/// The compose endpoint now speaks `multipart/form-data` (so attachments can
/// ride along), hence the multipart post even with no files.
async fn compose(app: &Router, cookie: &str, text: &str) -> String {
    let csrf = csrf_of(&get(app, "/", Some(cookie)).await.body);
    let posted = post_multipart(
        app,
        "/web/compose",
        cookie,
        &[("csrf", &csrf), ("status", text), ("visibility", "public")],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    posted.location.expect("redirect to the new post")
}

/// Builds a router where `@alice` (with login creds) owns the open local group
/// `!hiking`; returns the router and the group account id.
async fn app_with_alice_owning_group(pool: PgPool) -> (Router, i64) {
    let state = common::test_state_with(pool, StubFederation::with_actors([]));
    let alice = seed_alice(&state.pool).await;
    let (group, _) = plamenu::groups::create_group(
        &state,
        plamenu::groups::CreateGroupParams {
            name: "hiking",
            display_name: "Hiking & trails",
            membership_policy: plamenu_db::group::MembershipPolicy::Open,
            posting_policy: plamenu_db::group::PostingPolicy::Members,
            created_by: alice.id,
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    .unwrap();
    (build_router(state), group.id)
}

/// The scoped group composer (`/compose?group=id`) is the full composer plus
/// the Title / Link fields, with visibility locked public and a "Posting to"
/// note — not the old cramped three-field box.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_compose_page_is_the_full_composer_with_title_and_link(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool).await;
    let cookie = login(&app).await;
    let page = get(&app, &format!("/compose?group={group_id}"), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("data-compose-title"), "Title field");
    assert!(page.body.contains("data-compose-link"), "Link field");
    assert!(page.body.contains("Posting to"), "group context note");
    assert!(
        page.body
            .contains(&format!(r#"name="group_id" value="{group_id}""#)),
        "hidden group id"
    );
    assert!(
        page.body.contains(r#"name="visibility" value="public""#),
        "visibility locked public"
    );
    assert!(page.body.contains("Post to group"), "submit label");
    // The full composer's sections are present (media / poll toolbar).
    assert!(
        page.body.contains(r#"data-compose-section="media""#),
        "media"
    );
    assert!(page.body.contains(r#"data-compose-section="poll""#), "poll");
}

/// A link with no title re-renders the group composer inline with the rule in
/// the banner and every field preserved — never the old bare error page.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_link_without_a_title_reprompts_gracefully(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool).await;
    let cookie = login(&app).await;
    let gid = group_id.to_string();
    let csrf = csrf_of(
        &get(&app, &format!("/compose?group={group_id}"), Some(&cookie))
            .await
            .body,
    );
    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("group_id", &gid),
            ("visibility", "public"),
            ("title", ""),
            ("external_url", "https://example.com/article"),
            ("status", "look at this"),
            ("op", "post"),
        ],
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "the composer re-renders inline, not a redirect or bare error"
    );
    assert!(resp.body.contains("compose__error"), "inline banner");
    assert!(
        resp.body.contains("Link posts need a title"),
        "the rule is explained: {}",
        resp.body
    );
    assert!(resp.body.contains("Posting to"), "still the group composer");
    assert!(
        resp.body.contains(r#"value="https://example.com/article""#),
        "the link the user entered is preserved"
    );
    assert!(
        resp.body
            .contains(&format!(r#"name="group_id" value="{group_id}""#)),
        "group context preserved"
    );
}

/// A title-only thread (no body, no media) is a valid group post — the blank
/// check treats the title as content — and its thread shows the title.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_title_only_post_is_accepted(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool).await;
    let cookie = login(&app).await;
    let gid = group_id.to_string();
    let csrf = csrf_of(
        &get(&app, &format!("/compose?group={group_id}"), Some(&cookie))
            .await
            .body,
    );
    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("group_id", &gid),
            ("visibility", "public"),
            ("title", "Trailhead meetup Saturday"),
            ("external_url", ""),
            ("status", ""),
            ("op", "post"),
        ],
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::SEE_OTHER,
        "a title-only thread posts"
    );
    let thread = get(&app, &resp.location.unwrap(), Some(&cookie)).await;
    assert!(
        thread.body.contains("Trailhead meetup Saturday"),
        "title shows"
    );
    assert!(thread.body.contains("status__title"), "as the title line");
}

/// A bare group post keeps its "posted in [group]" context on every first-party
/// surface, not only where the group's Announce wrapper happens to survive.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_post_context_survives_public_profile_and_thread_surfaces(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool.clone()).await;
    let cookie = login(&app).await;
    let gid = group_id.to_string();
    let csrf = csrf_of(
        &get(&app, &format!("/compose?group={group_id}"), Some(&cookie))
            .await
            .body,
    );
    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("group_id", &gid),
            ("visibility", "public"),
            ("title", ""),
            ("external_url", ""),
            ("status", "meet at the trailhead"),
            ("op", "post"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let root_path = resp.location.unwrap();

    let assert_context = |surface: &str, body: &str| {
        assert!(body.contains("meet at the trailhead"), "{surface}: post");
        assert!(body.contains("posted in"), "{surface}: context label");
        assert!(
            body.contains("Hiking &amp; trails"),
            "{surface}: community name"
        );
        assert!(
            body.contains(r#"href="/@hiking""#),
            "{surface}: community link"
        );
    };

    let thread = get(&app, &root_path, Some(&cookie)).await;
    assert_context("post detail", &thread.body);
    assert_context(
        "federated timeline",
        &get(&app, "/public", Some(&cookie)).await.body,
    );
    assert_context(
        "local timeline",
        &get(&app, "/public?local=true", Some(&cookie)).await.body,
    );
    assert_context(
        "author profile",
        &get(&app, "/@alice", Some(&cookie)).await.body,
    );

    // Opening a reply makes the original an ancestor card above the focus.
    // That card is another bare rendering, and must keep the same context.
    let root_id: i64 = root_path.rsplit('/').next().unwrap().parse().unwrap();
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let reply = status::create_local(
        &pool,
        status::NewLocalStatus::new(
            alice.id,
            "<p>the reply we opened</p>",
            "public",
            Some(root_id),
        ),
    )
    .await
    .unwrap();
    let reply_thread = get(&app, &format!("/@alice/{}", reply.id), Some(&cookie)).await;
    assert!(
        reply_thread.body.contains("the reply we opened"),
        "focus reply"
    );
    assert_context("ancestor above an opened reply", &reply_thread.body);
}

/// The group page's "New post" button links to the scoped composer.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_page_links_to_the_scoped_composer(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool).await;
    let cookie = login(&app).await;
    let page = get(&app, "/@hiking", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains(&format!("/compose?group={group_id}")),
        "the New post button targets the scoped composer"
    );
    assert!(
        page.body.contains("group-post-link"),
        "styled as the button"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_publishes_and_renders_everywhere(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "hello timeline").await;
    assert!(permalink.starts_with("/@alice/"));

    // The new post shows in its thread, on the profile, on the home timeline
    // and on the anonymous public timeline.
    for (uri, cookie) in [
        (permalink.as_str(), Some(cookie.as_str())),
        ("/@alice", Some(cookie.as_str())),
        ("/", Some(cookie.as_str())),
        ("/public", None),
    ] {
        let resp = get(&app, uri, cookie).await;
        assert_eq!(resp.status, StatusCode::OK, "GET {uri}");
        assert!(resp.body.contains("hello timeline"), "GET {uri}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_local_visibility_renders_badge_and_stays_local(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    let compose_page = get(&app, "/compose", Some(&cookie)).await;
    assert!(compose_page.body.contains(r#"value="local""#));
    assert!(compose_page.body.contains("Local only"));

    let preferences = get(&app, "/settings/preferences", Some(&cookie)).await;
    assert!(preferences.body.contains(r#"value="local""#));

    let csrf = csrf_of(&compose_page.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "local web post"),
            ("visibility", "local"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let permalink = posted.location.expect("redirect to the new post");
    let status_id = permalink
        .rsplit('/')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let stored = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert_eq!(stored.visibility, "local");

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert_eq!(thread.status, StatusCode::OK);
    assert!(thread.body.contains("local web post"));
    // Both the detail row and timeline cards carry the visibility glyph with
    // the label as its tooltip.
    assert!(thread.body.contains(
        r#"class="status__vis status__detail-icon" data-detail-icon="vis-local" title="Local only""#
    ));
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(
        home.body
            .contains(r#"class="status__vis" title="Local only""#)
    );

    let public = get(&app, "/public", None).await;
    assert!(!public.body.contains("local web post"));
    let local_anon = get(&app, "/public?local=true", None).await;
    assert!(!local_anon.body.contains("local web post"));
    let local_auth = get(&app, "/public?local=true", Some(&cookie)).await;
    assert!(local_auth.body.contains("local web post"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_rejects_a_bad_csrf_token(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", "forged"),
            ("status", "nope"),
            ("visibility", "public"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    // And nothing was published.
    let public = get(&app, "/public", None).await;
    assert!(!public.body.contains("nope"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_attaches_uploaded_media(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let png = sample_png_bytes();
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "with a picture"),
            ("visibility", "public"),
            ("media_alt[]", "a warm square"),
        ],
        ("media[]", "square.png", "image/png", &png),
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let permalink = posted.location.expect("redirect to the new post");

    // The thread renders the attachment, carrying the alt text we supplied.
    let resp = get(&app, &permalink, Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("figure"), "media figure: {}", resp.body);
    assert!(
        resp.body.contains(r#"alt="a warm square""#),
        "alt text missing: {}",
        resp.body
    );
    // The image carries intrinsic dimensions so its box is reserved (no CLS).
    assert!(
        resp.body.contains("width=\"32\"") && resp.body.contains("height=\"32\""),
        "image not sized: {}",
        resp.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_accepts_media_over_the_default_body_limit(pool: PgPool) {
    // A single realistic photo already exceeds axum's 2 MB default body limit.
    // The compose route disables that default and sizes its own limit from the
    // configurable `max_media_attachments`, so the post must go through.
    // Regression for the "invalid multipart body" failure on larger uploads.
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let png = noise_png_bytes(1200);
    assert!(
        png.len() > 2 * 1024 * 1024,
        "fixture must exceed the 2 MB default to exercise the limit: {}",
        png.len()
    );
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "big picture"),
            ("visibility", "public"),
            ("media_alt[]", "noise"),
        ],
        ("media[]", "big.png", "image/png", &png),
    )
    .await;
    assert_eq!(
        posted.status,
        StatusCode::SEE_OTHER,
        "large upload rejected — body limit too small"
    );
}

/// Reads the attribute value that immediately follows `marker` in `body`.
fn value_after<'a>(body: &'a str, marker: &str) -> &'a str {
    let start = body
        .find(marker)
        .unwrap_or_else(|| panic!("missing {marker}"))
        + marker.len();
    body[start..].split('"').next().unwrap()
}

/// A multipart compose POST carrying the JS preview header, so the handler
/// returns just the rendered card fragment instead of the whole page.
async fn preview_fragment(app: &Router, cookie: &str, fields: &[(&str, &str)]) -> Resp {
    use std::fmt::Write as _;
    const BOUNDARY: &str = "PLAMENUPREVIEWBOUNDARY";
    let mut body = String::new();
    for (name, value) in fields {
        write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .unwrap();
    }
    write!(body, "--{BOUNDARY}--\r\n").unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/web/compose")
        .header(header::COOKIE, cookie)
        .header("X-Compose-Preview", "1")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_preview_renders_the_draft_without_posting(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "preview"),
            ("status", "**bold** unposted marker"),
            ("content_type", "text/markdown"),
            ("visibility", "public"),
        ],
    )
    .await;
    // A preview re-renders the page (not a redirect) with the rendered card.
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.body.contains("compose__preview-heading"),
        "no preview pane"
    );
    assert!(
        resp.body.contains("<strong>bold</strong>"),
        "markdown not rendered in preview: {}",
        resp.body
    );
    // The composer comes back with the text intact (echoed into the textarea).
    assert!(resp.body.contains("unposted marker"), "text not echoed");

    // Nothing was posted: the marker never reaches the author's profile.
    let profile = get(&app, "/@alice", Some(&cookie)).await;
    assert!(
        !profile.body.contains("unposted marker"),
        "preview leaked into a real post"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_preview_carries_media_as_a_removable_card(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let png = sample_png_bytes();

    // Preview with an attached file: it uploads and comes back as a kept card.
    let previewed = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "preview"),
            ("status", "look at this"),
            ("visibility", "public"),
            ("media_alt[]", "a warm square"),
        ],
        ("media[]", "square.png", "image/png", &png),
    )
    .await;
    assert_eq!(previewed.status, StatusCode::OK);
    assert!(
        previewed.body.contains(r#"name="media_keep[]""#),
        "attachment not carried as a removable card: {}",
        previewed.body
    );
    let media_id = value_after(&previewed.body, r#"name="media_keep[]" value=""#).to_owned();
    assert!(
        media_id.parse::<i64>().is_ok(),
        "no media id in card: {media_id}"
    );

    // Posting with that id (the card kept) attaches the already-uploaded media —
    // it survived the preview without re-uploading.
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "post"),
            ("status", "look at this"),
            ("visibility", "public"),
            ("media_keep[]", &media_id),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let permalink = posted.location.expect("redirect to the new post");
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(
        thread.body.contains("figure"),
        "media not attached: {}",
        thread.body
    );
    assert!(
        thread.body.contains(r#"alt="a warm square""#),
        "alt text lost"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_preview_fragment_returns_only_the_card(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let resp = preview_fragment(
        &app,
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "preview"),
            ("status", "fragment marker"),
            ("visibility", "public"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("fragment marker"), "card missing");
    // A fragment is just the card — no page shell, no composer form.
    assert!(
        !resp.body.contains("data-compose"),
        "fragment leaked the form"
    );
    assert!(
        !resp.body.contains("<title"),
        "fragment leaked the page shell"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_preview_fragment_allows_a_media_only_draft(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    // The JS path composites its media client-side and doesn't upload on
    // preview, so a media-only draft arrives with empty text and no ids but
    // flags `preview_has_media`. It must render, not 422 as blank.
    let resp = preview_fragment(
        &app,
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "preview"),
            ("status", ""),
            ("visibility", "public"),
            ("preview_has_media", "1"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        !resp.body.contains("compose__error"),
        "media-only draft wrongly rejected as blank: {}",
        resp.body
    );
    assert!(resp.body.contains("class=\"status\""), "no card rendered");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_preview_reports_a_validation_error(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    // A blank draft with no media: the composer re-renders with the banner
    // rather than a bare error page, keeping the user in the composer.
    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "preview"),
            ("status", ""),
            ("visibility", "public"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("compose__error"), "no error banner");
    assert!(
        resp.body.contains("can't be blank"),
        "wrong error: {}",
        resp.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn favourite_toggles_through_the_action_form(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "favourite me").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    let thread = get(&app, &permalink, Some(&cookie)).await;
    let csrf = csrf_of(&thread.body);
    // The thread offers the "favourite" action (not yet "unfavourite").
    assert!(
        thread
            .body
            .contains(&format!("/web/statuses/{status_id}/favourite"))
    );

    let fav = post_form(
        &app,
        &format!("/web/statuses/{status_id}/favourite"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &permalink)],
    )
    .await;
    assert_eq!(fav.status, StatusCode::SEE_OTHER);
    assert_eq!(fav.location.as_deref(), Some(permalink.as_str()));

    // Now the form flips to the un-favourite endpoint.
    let after = get(&app, &permalink, Some(&cookie)).await;
    assert!(
        after
            .body
            .contains(&format!("/web/statuses/{status_id}/unfavourite"))
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bookmarks_and_favourites_pages_list_the_marked_posts(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "keep this around").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();
    let csrf = csrf_of(&get(&app, &permalink, Some(&cookie)).await.body);

    // Empty state before anything is marked.
    let empty_bookmarks = get(&app, "/bookmarks", Some(&cookie)).await;
    assert_eq!(empty_bookmarks.status, StatusCode::OK);
    assert!(!empty_bookmarks.body.contains("keep this around"));
    let empty_favourites = get(&app, "/favourites", Some(&cookie)).await;
    assert!(!empty_favourites.body.contains("keep this around"));

    for verb in ["bookmark", "favourite"] {
        let resp = post_form(
            &app,
            &format!("/web/statuses/{status_id}/{verb}"),
            &cookie,
            &[("csrf", &csrf), ("return_to", &permalink)],
        )
        .await;
        assert_eq!(resp.status, StatusCode::SEE_OTHER, "{verb}");
    }

    let bookmarks = get(&app, "/bookmarks", Some(&cookie)).await;
    assert_eq!(bookmarks.status, StatusCode::OK);
    assert!(bookmarks.body.contains("keep this around"));

    let favourites = get(&app, "/favourites", Some(&cookie)).await;
    assert_eq!(favourites.status, StatusCode::OK);
    assert!(favourites.body.contains("keep this around"));

    // The chrome links to both listings.
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(home.body.contains("href=\"/bookmarks\""));
    assert!(home.body.contains("href=\"/favourites\""));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mobile_tabbar_is_five_slots_with_a_burger_drawer(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    let body = &home.body;

    // The tab bar's own slots are the four direct links before the burger's
    // <details>; everything else moved into the drawer. Slice the region
    // between the tab bar and the drawer to inspect just those direct tabs.
    let (tabbar_start, rest) = body
        .split_once("class=\"tabbar\"")
        .expect("tab bar present");
    assert!(
        tabbar_start.is_empty() || rest.contains("data-drawer"),
        "tab bar hosts the drawer"
    );
    let direct_tabs = rest.split_once("data-drawer").expect("burger present").0;
    for link in ["/", "/public", "/compose", "/notifications"] {
        assert!(
            direct_tabs.contains(&format!("href=\"{link}\"")),
            "direct tab {link}"
        );
    }
    assert!(
        !direct_tabs.contains("href=\"/settings\""),
        "settings is not a direct tab"
    );
    assert!(
        !direct_tabs.contains("href=\"/bookmarks\""),
        "bookmarks is not a direct tab"
    );

    // The drawer carries the full menu — every nav destination plus the
    // sign-out form — behind a dismissable scrim. Stop at the `</details>`
    // that closes the drawer: past it lies the sidebar, which links to the
    // same destinations and would satisfy these assertions for free.
    let drawer = body
        .split_once("data-drawer-scrim")
        .expect("drawer scrim present")
        .1
        .split_once("</details>")
        .expect("drawer closes")
        .0;
    for link in [
        "/",
        "/explore",
        "/public",
        "/notifications",
        "/bookmarks",
        "/favourites",
        "/search",
        "/settings",
    ] {
        assert!(
            drawer.contains(&format!("href=\"{link}\"")),
            "drawer links to {link}"
        );
    }
    assert!(
        drawer.contains("action=\"/logout\""),
        "drawer offers sign-out"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mobile_tabbar_is_emitted_before_the_page_content(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    let body = &home.body;

    // The bar is `position: fixed`, so source order costs it nothing visually
    // — but browsers paint as they parse, and a bar emitted after a long
    // timeline only shows up once that timeline has been parsed, reading as
    // the chrome flickering in late. Keep it ahead of the content.
    let tabbar = body.find("class=\"tabbar\"").expect("tab bar present");
    let main = body.find("class=\"app-main\"").expect("main present");
    assert!(
        tabbar < main,
        "tab bar must precede the page content so it lands in the first paint"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn nav_account_row_wears_the_viewers_avatar(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    // Never set one: the row falls back to the same placeholder the API entity
    // hands out, so it can't render a broken image.
    let body = get(&app, "/", Some(&cookie)).await.body;
    assert!(
        body.contains(r#"<img class="account__avatar" src="/static/missing.png""#),
        "the avatar-less account row falls back to the placeholder"
    );

    sqlx::query!("UPDATE accounts SET avatar_file_name = 'ava-9.jpg' WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();

    // A session is always a local account, so its upload is served straight
    // from `/media/` — no domain, no proxy hop.
    let body = get(&app, "/", Some(&cookie)).await.body;
    assert_eq!(
        body.matches(r#"<img class="account__avatar" src="/media/ava-9.jpg""#)
            .count(),
        2,
        "both account rows — the sidebar's and the drawer's — carry the avatar"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn profile_renders_banner_and_profile_media_descriptions(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    sqlx::query(
        "UPDATE accounts
         SET avatar_file_name = 'avatar.png', header_file_name = 'banner.png',
             avatar_description = 'Alice smiling',
             header_description = 'A sunrise over green hills'
         WHERE id = $1",
    )
    .bind(alice.id)
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool);

    let page = get(&app, "/@alice", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(r#"<header class="profile has-header">"#));
    assert!(
        page.body.contains(
            r#"<img class="profile__header" src="https://plamenu.test/media/banner.png" alt="A sunrise over green hills">"#
        ),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(
            r#"<img class="profile__avatar" src="https://plamenu.test/media/avatar.png" alt="Alice smiling""#
        ),
        "{}",
        page.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_mobile_tabbar_is_home_plus_burger_drawer(pool: PgPool) {
    common::open_previews(&pool).await;
    let app = common::test_app(pool);
    let home = get(&app, "/", None).await;
    let body = &home.body;

    // Signed out, the bar keeps to two slots: Home and the burger. All other
    // destinations live in the drawer, like the signed-in layout.
    let rest = body
        .split_once("class=\"tabbar\"")
        .expect("tab bar present")
        .1;
    let direct_tabs = rest.split_once("data-drawer").expect("burger present").0;
    assert!(direct_tabs.contains("href=\"/\""), "home tab");
    for link in ["/explore", "/search", "/login", "/rules", "/staff"] {
        assert!(
            !direct_tabs.contains(&format!("href=\"{link}\"")),
            "{link} is not a direct tab"
        );
    }

    // The drawer carries the anonymous menu: browsing links (previews are on
    // in the test config), the always-public pages, and sign-in. Bounded at
    // the drawer's own `</details>` so the sidebar past it can't stand in.
    let drawer = body
        .split_once("data-drawer-scrim")
        .expect("drawer scrim present")
        .1
        .split_once("</details>")
        .expect("drawer closes")
        .0;
    for link in [
        "/explore", "/public", "/people", "/groups", "/search", "/rules", "/staff", "/login",
    ] {
        assert!(
            drawer.contains(&format!("href=\"{link}\"")),
            "drawer links to {link}"
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn profile_follow_button_drives_the_relationship(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let profile = get(&app, "/@bob", Some(&cookie)).await;
    assert_eq!(profile.status, StatusCode::OK);
    assert!(profile.body.contains("Follow"));
    assert!(
        !profile.body.contains("Follow settings"),
        "no settings before following"
    );
    let csrf = csrf_of(&profile.body);

    let follow = post_form(
        &app,
        &format!("/web/accounts/{}/follow", bob.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", "/@bob")],
    )
    .await;
    assert_eq!(follow.status, StatusCode::SEE_OTHER);

    let after = get(&app, "/@bob", Some(&cookie)).await;
    assert!(after.body.contains("Unfollow"));

    // Following exposes the per-follow settings form (M32); saving it
    // stores notify / hide-boosts / the language filter.
    assert!(after.body.contains("Follow settings"));
    let settings = post_form(
        &app,
        &format!("/web/accounts/{}/follow_settings", bob.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", "/@bob"),
            ("notify", "on"),
            ("languages", "en"),
            ("languages", "de"),
        ],
    )
    .await;
    assert_eq!(settings.status, StatusCode::SEE_OTHER);
    let edge = follow::find(&pool, alice.id, bob.id)
        .await
        .unwrap()
        .unwrap();
    assert!(edge.notify);
    assert!(!edge.show_reblogs, "unchecked box turns boosts off");
    assert!(!edge.with_replies, "so does the replies box");
    assert_eq!(
        edge.languages.as_deref(),
        Some(["en".to_owned(), "de".to_owned()].as_slice())
    );

    // The form renders the stored state back.
    let page = get(&app, "/@bob", Some(&cookie)).await;
    assert!(page.body.contains(r#"name="notify" checked"#));
    assert!(!page.body.contains(r#"name="show_reblogs" checked"#));
    assert!(!page.body.contains(r#"name="with_replies" checked"#));

    // Re-submitting with boosts on and no languages clears the filter.
    let settings = post_form(
        &app,
        &format!("/web/accounts/{}/follow_settings", bob.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", "/@bob"),
            ("show_reblogs", "on"),
            ("with_replies", "on"),
        ],
    )
    .await;
    assert_eq!(settings.status, StatusCode::SEE_OTHER);
    let edge = follow::find(&pool, alice.id, bob.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!edge.notify);
    assert!(edge.show_reblogs);
    assert!(edge.with_replies);
    assert_eq!(edge.languages, None);
    let page = get(&app, "/@bob", Some(&cookie)).await;
    assert!(page.body.contains(r#"name="with_replies" checked"#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unknown_profile_is_a_styled_404(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let resp = get(&app, "/@nobody", None).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert!(resp.content_type.starts_with("text/html"));
    assert!(resp.body.contains("Not found"));
}

/// A remote Lemmy handle served as both a Person (`/u/collision`) and a Group
/// (`/c/collision`): `/@collision@host` renders the person, `/!collision@host`
/// the group. They are different pages.
#[sqlx::test(migrations = "../db/migrations")]
async fn person_and_group_handles_route_separately(pool: PgPool) {
    let mut person = RemoteUser::new("lemmy.test", "collision");
    person.actor.name = Some("Collision Person".to_owned());
    person.actor.id = "https://lemmy.test/u/collision".to_owned();
    person.actor.inbox = "https://lemmy.test/u/collision/inbox".to_owned();
    person.actor.public_key.id = "https://lemmy.test/u/collision#main-key".to_owned();
    person.actor.public_key.owner = "https://lemmy.test/u/collision".to_owned();

    let mut group = RemoteUser::new("lemmy.test", "collision");
    group.actor.kind = "Group".to_owned();
    group.actor.name = Some("Collision Community".to_owned());
    group.actor.id = "https://lemmy.test/c/collision".to_owned();
    group.actor.inbox = "https://lemmy.test/c/collision/inbox".to_owned();
    group.actor.public_key.id = "https://lemmy.test/c/collision#main-key".to_owned();
    group.actor.public_key.owner = "https://lemmy.test/c/collision".to_owned();

    remote::store_remote_actor(&pool, &person.actor)
        .await
        .unwrap();
    remote::store_remote_actor(&pool, &group.actor)
        .await
        .unwrap();

    let app = common::test_app(pool.clone());

    let person_page = get(&app, "/@collision@lemmy.test", None).await;
    assert_eq!(person_page.status, StatusCode::OK);
    assert!(person_page.body.contains("Collision Person"));
    assert!(!person_page.body.contains("Collision Community"));
    // A person keeps the `@` sigil and "Joined".
    assert!(person_page.body.contains("@collision@lemmy.test"));
    assert!(person_page.body.contains("Joined "), "{}", person_page.body);

    let group_page = get(&app, "/!collision@lemmy.test", None).await;
    assert_eq!(group_page.status, StatusCode::OK);
    assert!(group_page.body.contains("Collision Community"));
    assert!(!group_page.body.contains("Collision Person"));
    // A group wears the `!` community sigil and reads "Created", not "Joined".
    assert!(
        group_page.body.contains("!collision@lemmy.test"),
        "{}",
        group_page.body
    );
    assert!(group_page.body.contains("Created "), "{}", group_page.body);
    assert!(!group_page.body.contains("Joined "), "{}", group_page.body);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn public_timeline_is_open_to_anonymous_visitors(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let resp = get(&app, "/public", None).await;
    assert_eq!(resp.status, StatusCode::OK);
    // The scope tabs are present even without a session.
    assert!(resp.body.contains("Federated"));
    assert!(resp.body.contains("Local"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_page_renders(pool: PgPool) {
    let app = common::test_app(pool);
    let resp = get(&app, "/login", None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.content_type.starts_with("text/html"));
    assert!(resp.body.contains("Sign in"));
    assert!(resp.body.contains(r#"action="/login""#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_page_negotiates_russian_from_accept_language(pool: PgPool) {
    let app = common::test_app(pool);
    let request = Request::builder()
        .uri("/login")
        .header(header::ACCEPT_LANGUAGE, "en;q=0.4, ru-RU;q=0.9")
        .body(Body::empty())
        .unwrap();
    let resp = send(&app, request).await;

    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(resp.body.contains("Войти"));
    assert!(resp.body.contains("Электронная почта или имя пользователя"));
    assert!(resp.body.contains("Правила"));
    assert!(!resp.body.contains(">Sign in<"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stored_interface_locale_overrides_accept_language(pool: PgPool) {
    let account = seed_alice(&pool).await;
    let user = user::find_by_account_id(&pool, account.id)
        .await
        .unwrap()
        .unwrap();
    user::update_locale(&pool, user.id, Some("ru"))
        .await
        .unwrap();
    common::open_previews(&pool).await;
    let app = common::test_app(pool);

    let login = post_login(&app, EMAIL, PASSWORD).await;
    let set_cookie = login.set_cookie.unwrap();
    let cookie = set_cookie.split(';').next().unwrap();
    let request = Request::builder()
        .uri("/")
        .header(header::COOKIE, cookie)
        .header(header::ACCEPT_LANGUAGE, "en-US")
        .body(Body::empty())
        .unwrap();
    let resp = send(&app, request).await;

    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(resp.body.contains("<title>Главная — Plamenu</title>"));
    assert!(resp.body.contains("Уведомления"));
    assert!(resp.body.contains("Настройки"));

    let bookmarks = get(&app, "/bookmarks", Some(cookie)).await;
    assert_eq!(bookmarks.status, StatusCode::OK);
    assert!(bookmarks.body.contains("<h1>Закладки</h1>"));
    assert!(
        bookmarks
            .body
            .contains("У вас пока нет публикаций в закладках.")
    );

    let notifications = get(&app, "/notifications", Some(cookie)).await;
    assert_eq!(notifications.status, StatusCode::OK);
    assert!(notifications.body.contains("<h1>Уведомления</h1>"));
    assert!(notifications.body.contains("Уведомлений пока нет."));

    let conversations = get(&app, "/conversations", Some(cookie)).await;
    assert_eq!(conversations.status, StatusCode::OK);
    assert!(conversations.body.contains("<h1>Личные упоминания</h1>"));
    assert!(conversations.body.contains("Новое личное упоминание"));

    let compose = get(&app, "/compose", Some(cookie)).await;
    assert_eq!(compose.status, StatusCode::OK);
    assert!(compose.body.contains("<h1>Новая публикация</h1>"));
    assert!(compose.body.contains("О чём вы думаете?"));
    assert!(compose.body.contains("Предупреждение о содержимом"));
    assert!(compose.body.contains("Отметить медиафайлы как деликатные"));
    assert!(compose.body.contains("Добавить опрос"));
    assert!(compose.body.contains("Опубликовать"));
    assert!(!compose.body.contains("What&#x27;s on your mind?"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_search_negotiates_russian_interface(pool: PgPool) {
    common::open_previews(&pool).await;
    let app = common::test_app(pool);
    let request = Request::builder()
        .uri("/search?q=missing")
        .header(header::ACCEPT_LANGUAGE, "ru")
        .body(Body::empty())
        .unwrap();
    let resp = send(&app, request).await;

    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(resp.body.contains("Поиск публикаций, людей и хэштегов"));
    assert!(resp.body.contains("ничего не найдено"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_profile_negotiates_russian_interface(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let request = Request::builder()
        .uri("/@alice")
        .header(header::ACCEPT_LANGUAGE, "ru-RU")
        .body(Body::empty())
        .unwrap();
    let resp = send(&app, request).await;

    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(resp.body.contains("Публикации"));
    assert!(resp.body.contains("Подписки"));
    assert!(resp.body.contains("Подписчики"));
    assert!(resp.body.contains("Разделы профиля"));
    assert!(!resp.body.contains(">Following<"));
    assert!(!resp.body.contains(">Followers<"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_home_serves_the_landing_page(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let resp = get(&app, "/", None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("landing__hero"));
    // The compact stats block: a local and a known-network line, but no
    // relay line while no relay subscription is accepted.
    assert!(resp.body.contains("This server"));
    assert!(resp.body.contains("Fediverse"));
    assert!(!resp.body.contains("Relays"));
    // Rules, the profile directory and the staff list moved to their own
    // pages, linked from the left menu; the groups sample is gone entirely.
    assert!(!resp.body.contains("Server rules"));
    assert!(!resp.body.contains("Discoverable profiles"));
    assert!(!resp.body.contains("Administered by"));
    // (the left menu still links the /groups directory, so match the
    // section heading, not the word)
    assert!(!resp.body.contains("<h2>Groups</h2>"));
    assert!(resp.body.contains(r#"href="/rules""#));
    assert!(resp.body.contains(r#"href="/staff""#));
    // The footer is the version, linking to the repository.
    assert!(resp.body.contains(&format!("Plamenu {}", plamenu::VERSION)));
    assert!(resp.body.contains(plamenu::SOURCE_URL));

    // With the landing page switched off, the old redirect returns. A fresh
    // app instance sidesteps the settings cache.
    sqlx::query!("UPDATE instance_settings SET landing_page = false")
        .execute(&pool)
        .await
        .unwrap();
    let app = common::test_app(pool);
    let resp = get(&app, "/", None).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(resp.location.as_deref(), Some("/login"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_discovery_pages_negotiate_russian(pool: PgPool) {
    common::open_previews(&pool).await;
    // The discovery surfaces sit behind their own anonymous switches.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            anon_trends: true,
            anon_directory: true,
            anon_groups: true,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool);
    let ru = |uri: &'static str| {
        Request::builder()
            .uri(uri)
            .header(header::ACCEPT_LANGUAGE, "ru")
            .body(Body::empty())
            .unwrap()
    };

    // The welcome page: chrome, CTA and the stats block.
    let landing = send(&app, ru("/")).await;
    assert_eq!(landing.status, StatusCode::OK);
    assert!(landing.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(landing.body.contains("Войти"));
    assert!(landing.body.contains("Этот сервер"));
    assert!(landing.body.contains("Федиверс"));
    assert!(!landing.body.contains("Fediverse"));

    // The always-public rules and staff pages.
    let rules = send(&app, ru("/rules")).await;
    assert!(rules.body.contains("<h1>Правила сервера</h1>"));
    assert!(rules.body.contains("Этот сервер не опубликовал правил."));
    let staff = send(&app, ru("/staff")).await;
    assert!(staff.body.contains("<h1>Команда</h1>"));
    assert!(staff.body.contains("Этот сервер не указал команду."));

    // Trending: the tab strip and the posts tab's empty state.
    let explore = send(&app, ru("/explore")).await;
    assert_eq!(explore.status, StatusCode::OK);
    assert!(explore.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(explore.body.contains("Хэштеги"));
    assert!(explore.body.contains("Новости"));
    assert!(
        explore.body.contains("Сейчас ничего не в тренде.")
            || explore.body.contains("Тренды отключены на этом сервере.")
    );

    // The People directory.
    let people = send(&app, ru("/people")).await;
    assert_eq!(people.status, StatusCode::OK);
    assert!(people.body.contains("<h1>"));
    assert!(people.body.contains("Люди"));
    assert!(
        people.body.contains("В каталоге пока никого нет.")
            || people
                .body
                .contains("Каталог профилей отключён на этом сервере.")
    );

    // The public timelines: both scope tabs under the negotiated chrome.
    let public = send(&app, ru("/public")).await;
    assert_eq!(public.status, StatusCode::OK);
    assert!(public.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(public.body.contains("Федеративная"));
    assert!(public.body.contains("Локальная"));

    // The remote-interaction interstitial, and a refusal from its POST leg.
    let interact = send(&app, ru("/interact?uri=https://remote.example/notes/1")).await;
    assert_eq!(interact.status, StatusCode::OK);
    assert!(interact.body.contains("Продолжите на своём сервере"));
    assert!(
        interact
            .body
            .contains("Вы собираетесь взаимодействовать с содержимым этого сервера.")
    );
    assert!(interact.body.contains("Отправиться домой"));
    let refused = Request::builder()
        .method("POST")
        .uri("/interact")
        .header(header::ACCEPT_LANGUAGE, "ru")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            "uri=https%3A%2F%2Fremote.example%2Fnotes%2F1&handle=not-a-handle",
        ))
        .unwrap();
    let refused = send(&app, refused).await;
    assert_eq!(refused.status, StatusCode::OK);
    assert!(refused.body.contains("не похоже на полный адрес"));

    // The groups directory, behind its own anonymous switch.
    let groups = send(&app, ru("/groups")).await;
    assert_eq!(groups.status, StatusCode::OK);
    assert!(groups.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(groups.body.contains("Локальные группы"));
    assert!(
        groups
            .body
            .contains("Других групп на этом сервере пока нет.")
    );

    // The styled 404 negotiates for anonymous readers too (it used to fall
    // back to English chrome).
    let missing = send(&app, ru("/@no-such-user")).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(missing.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(missing.body.contains("Страница не найдена"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn landing_stats_show_connected_relays(pool: PgPool) {
    // An accepted relay with some inbound activity this week gets its own
    // stats line (a pending one would not — see the landing-page test above).
    let relay = sqlx::query_scalar!(
        r#"INSERT INTO relays (id, inbox_url, state)
           VALUES ($1, 'https://relay.example/inbox', 'accepted') RETURNING id"#,
        plamenu_db::id::next(),
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO relay_daily_activities (relay_id, day, count)
         VALUES ($1, (now() AT TIME ZONE 'UTC')::date, 42)",
        relay,
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = common::test_app(pool);
    let resp = get(&app, "/", None).await;
    assert!(resp.body.contains("Relays"));
    assert!(resp.body.contains("connected"));
    assert!(resp.body.contains("activities this week"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn staff_page_lists_privileged_accounts(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    // Alice holds the default User role: no staff to show yet.
    let resp = get(&app, "/staff", None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("has not listed any staff"));

    // A privileged role puts her on the page under the role's own heading
    // (no settings cache involved).
    sqlx::query!("UPDATE users SET role_id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let resp = get(&app, "/staff", None).await;
    assert!(resp.body.contains("Moderator"));
    assert!(resp.body.contains("/@alice"));

    // Unchecking "Suggest account to others" takes her off the roster.
    sqlx::query!("UPDATE accounts SET discoverable = false")
        .execute(&pool)
        .await
        .unwrap();
    let resp = get(&app, "/staff", None).await;
    assert!(resp.body.contains("has not listed any staff"));
    assert!(!resp.body.contains("/@alice"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn good_credentials_set_session_cookie(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let resp = post_login(&app, EMAIL, PASSWORD).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(resp.location.as_deref(), Some("/"));
    let cookie = resp.set_cookie.expect("session cookie set");
    assert!(cookie.starts_with("__Host-plamenu_session="));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("Secure"));
    assert!(cookie.contains("SameSite=Lax"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_accepts_username_and_handle_forms(pool: PgPool) {
    let app = app_with_alice(pool).await;
    // The same field takes the username, with or without a leading `@`, the
    // full local handle, or (case-insensitively) the e-mail address.
    for identifier in [
        "alice",
        "@alice",
        "ALICE",
        "alice@plamenu.test",
        "ALICE@EXAMPLE.COM",
    ] {
        let resp = post_login(&app, identifier, PASSWORD).await;
        assert_eq!(resp.status, StatusCode::SEE_OTHER, "{identifier}");
        assert!(resp.set_cookie.is_some(), "{identifier}");
    }
    // A handle on a foreign domain is not a local login.
    let resp = post_login(&app, "alice@elsewhere.example", PASSWORD).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_form_email_field_alias_still_works(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let body = serde_urlencoded::to_string([("email", EMAIL), ("password", PASSWORD)]).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let resp = send(&app, request).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(resp.set_cookie.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn account_without_email_signs_in_by_username(pool: PgPool) {
    let account = create_local_account(&pool, "noemail", "No Email").await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    plamenu_db::user::create(&pool, account.id, None, &hash)
        .await
        .unwrap();
    let app = common::test_app(pool);
    let resp = post_login(&app, "noemail", PASSWORD).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(resp.set_cookie.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn repeated_bad_credentials_temporarily_lock_login(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    for _ in 0..(user::LOGIN_LOCK_MAX_ATTEMPTS - 1) {
        let resp = post_login(&app, EMAIL, "wrong").await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
        assert!(resp.set_cookie.is_none());
        assert!(resp.body.contains("Wrong e-mail/username or password"));
    }

    let locked = post_login(&app, EMAIL, "wrong").await;
    assert_eq!(locked.status, StatusCode::UNAUTHORIZED);
    assert!(locked.body.contains("temporarily locked"));

    let correct = post_login(&app, EMAIL, PASSWORD).await;
    assert_eq!(correct.status, StatusCode::UNAUTHORIZED);
    assert!(correct.set_cookie.is_none());
    assert!(correct.body.contains("temporarily locked"));

    sqlx::query!(
        "UPDATE users SET locked_at = now() - interval '2 hours' WHERE lower(email) = lower($1)",
        EMAIL,
    )
    .execute(&pool)
    .await
    .unwrap();

    let unlocked = post_login(&app, EMAIL, PASSWORD).await;
    assert_eq!(unlocked.status, StatusCode::SEE_OTHER);
    assert!(unlocked.set_cookie.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn session_cookie_unlocks_home(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = cookie_pair(
        &post_login(&app, EMAIL, PASSWORD)
            .await
            .set_cookie
            .expect("cookie"),
    )
    .to_owned();

    let resp = get(&app, "/", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    // The home feed no longer carries an inline composer — posting lives on
    // the dedicated /compose page, linked from the chrome.
    assert!(!resp.body.contains(r#"action="/web/compose""#));
    assert!(resp.body.contains(r#"href="/compose""#));
    // The signed-in nav exposes the handle and a sign-out control.
    assert!(resp.body.contains("/@alice"));
    assert!(resp.body.contains(r#"action="/logout""#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn logout_clears_and_revokes_the_session(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = cookie_pair(
        &post_login(&app, EMAIL, PASSWORD)
            .await
            .set_cookie
            .expect("cookie"),
    )
    .to_owned();

    // The shell's logout form carries the session-bound CSRF token; pull it
    // off a rendered page like a browser would.
    let home = get(&app, "/", Some(&cookie)).await;
    let logout = post_form(&app, "/logout", &cookie, &[("csrf", &csrf_of(&home.body))]).await;
    assert_eq!(logout.status, StatusCode::SEE_OTHER);
    assert_eq!(logout.location.as_deref(), Some("/login"));
    assert!(logout.set_cookie.unwrap().contains("Max-Age=0"));

    // The revoked token no longer authenticates ("/" now serves the public
    // landing page to everyone, so probe a signed-in-only page instead).
    let after = get(&app, "/notifications", Some(&cookie)).await;
    assert_eq!(after.status, StatusCode::SEE_OTHER);
    assert_eq!(after.location.as_deref(), Some("/login"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stylesheet_is_served(pool: PgPool) {
    let app = common::test_app(pool);
    let resp = get(&app, "/assets/app.css", None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.content_type.starts_with("text/css"));
    assert!(resp.body.contains(":root"));

    let media_badges = resp
        .body
        .split_once(".media__badges {")
        .and_then(|(_, css)| css.split_once('}'))
        .map(|(rule, _)| rule)
        .expect("stylesheet contains the media badge rule");
    assert!(media_badges.contains("inset-block-start: var(--space-2)"));
    assert!(!media_badges.contains("inset-block-end:"));

    // A notification embeds a full status. Its own grid track must be
    // shrinkable too; constraining only the outer notifications list lets a
    // preview card's max-content width push the nested status past the card.
    let notification = resp
        .body
        .split_once(".notification {")
        .and_then(|(_, css)| css.split_once('}'))
        .map(|(rule, _)| rule)
        .expect("stylesheet contains the notification rule");
    assert!(notification.contains("grid-template-columns: minmax(0, 1fr)"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pwa_install_surface_is_complete(pool: PgPool) {
    let app = common::test_app(pool);

    let response = get(&app, "/manifest.webmanifest", None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.content_type,
        "application/manifest+json; charset=utf-8"
    );
    let manifest: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(manifest["name"], "Plamenu");
    assert_eq!(manifest["id"], "/");
    assert_eq!(manifest["start_url"], "/");
    assert_eq!(manifest["scope"], "/");
    assert_eq!(manifest["display"], "standalone");
    assert_eq!(manifest["background_color"], "#0f1419");
    assert_eq!(manifest["theme_color"], "#0f1419");
    let icons = manifest["icons"].as_array().unwrap();
    assert!(
        icons
            .iter()
            .any(|icon| { icon["sizes"] == "512x512" && icon["purpose"] == "any" })
    );
    assert!(
        icons
            .iter()
            .any(|icon| { icon["sizes"] == "512x512" && icon["purpose"] == "maskable" })
    );
    assert_eq!(manifest["shortcuts"].as_array().unwrap().len(), 3);

    let icon = image::load_from_memory(&png_bytes(&app, "/pwa/icon-512.png").await).unwrap();
    assert_eq!((icon.width(), icon.height()), (512, 512));
    let touch = image::load_from_memory(&png_bytes(&app, "/apple-touch-icon.png").await).unwrap();
    assert_eq!((touch.width(), touch.height()), (180, 180));
    assert_eq!(
        get(&app, "/pwa/not-an-asset.png", None).await.status,
        StatusCode::NOT_FOUND
    );

    let offline = get(&app, "/offline", None).await;
    assert_eq!(offline.status, StatusCode::OK);
    assert!(offline.body.contains("You're offline"));
    assert!(
        offline
            .body
            .contains(r#"link rel="manifest" href="/manifest.webmanifest""#)
    );
    assert!(
        offline
            .body
            .contains(r#"rel="apple-touch-icon" sizes="180x180""#)
    );
    assert!(!offline.body.contains("apple-touch-startup-image"));

    let worker = get(&app, "/sw.js", None).await;
    assert_eq!(worker.status, StatusCode::OK);
    assert!(worker.content_type.starts_with("text/javascript"));
    assert!(worker.body.contains("const PLAMENU_ASSET_VERSION"));
    assert!(worker.body.contains("self.addEventListener(\"fetch\""));
    assert!(worker.body.contains("navigationPreload.enable()"));
    let script = get(&app, "/assets/app.js", None).await;
    assert!(script.body.contains("if (menu.open) opened()"));
    assert!(script.body.contains("(display-mode: standalone)"));
    assert!(script.body.contains("bindStandaloneExternalLinks"));
    assert!(script.body.contains("stream=user:notification"));
    assert!(script.body.contains("showLiveNotification"));
    assert!(
        script
            .body
            .contains(r#"window.addEventListener("pagehide", suspend)"#)
    );
    assert!(
        script
            .body
            .contains(r#"window.addEventListener("pageshow", resume)"#)
    );
    assert!(
        script
            .body
            .contains(r#"const pagerHistoryKey = "plamenuPager""#)
    );
    assert!(
        script
            .body
            .contains("await restorePagerPosition(container, saved)")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notifications_page_lists_activity(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    // Bob follows Alice — a "follow" notification lands for Alice.
    plamenu_db::notification::create(&pool, alice.id, bob.id, "follow", None)
        .await
        .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let resp = get(&app, "/notifications", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("Bob"));
    assert!(resp.body.contains("followed you"));
    assert!(resp.body.contains("/@bob"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notifications_page_coalesces_post_reasons_and_mentions_win(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let post = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "<p>one post, three reasons</p>", "public", None),
    )
    .await
    .unwrap();
    notification::create_post_notifications_many(
        &pool,
        &[notification::PostNotification {
            account_id: alice.id,
            mention: true,
            quote: true,
            status: true,
        }],
        bob.id,
        post.id,
    )
    .await
    .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/notifications", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.body
            .matches(r#"<article class="notification""#)
            .count(),
        1
    );
    assert_eq!(page.body.matches("one post, three reasons").count(), 1);
    assert!(page.body.contains("mentioned you"));
    assert!(!page.body.contains("quoted your post"));
    assert!(!page.body.contains(">posted<"));
    let row = sqlx::query!(
        "SELECT kind, reasons FROM notifications WHERE account_id = $1",
        alice.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.kind, "mention");
    assert_eq!(row.reasons, vec!["mention", "quote", "status"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reaction_notification_links_once_to_actor_and_shows_reacted_post(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    common::open_previews(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "the post Bob reacted to").await;
    let status_id: i64 = permalink.rsplit('/').next().unwrap().parse().unwrap();

    reaction::create(
        &pool,
        reaction::NewReaction {
            account_id: bob.id,
            status_id,
            name: "🔥",
            custom_emoji_url: None,
            uri: None,
        },
    )
    .await
    .unwrap();
    plamenu_db::notification::create_reaction(&pool, alice.id, bob.id, status_id, "🔥")
        .await
        .unwrap();

    let page = get(&app, "/notifications", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Bob"));
    assert!(page.body.contains("reacted to your post"));
    assert!(page.body.contains("the post Bob reacted to"));
    assert!(
        page.body
            .contains(&format!(r#"href="{permalink}#post-{status_id}""#)),
        "the reacted post has a link to its thread"
    );
    assert_eq!(
        page.body.matches(r#"href="/@bob""#).count(),
        1,
        "the reactor profile is linked once, without an account-card duplicate"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unread_dot_lights_the_bell_until_the_page_is_opened(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Nothing has happened yet: no dot anywhere in the chrome.
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(!home.body.contains("bell__dot"), "quiet bell with no news");

    plamenu_db::notification::create(&pool, alice.id, bob.id, "follow", None)
        .await
        .unwrap();

    // The dot shows on any page's chrome, alongside the hidden "(new)" label.
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(home.body.contains("bell__dot"), "bell lights up");
    assert!(home.body.contains("(new)"));

    // Opening the notifications page reads everything: the shared marker
    // advances to the newest notification and this very render clears the dot.
    let page = get(&app, "/notifications", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("bell__dot"), "reading clears the bell");

    let user = plamenu_db::user::find_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    let marker = plamenu_db::marker::find(&pool, user.id, "notifications")
        .await
        .unwrap()
        .expect("opening the page saved a marker");
    assert!(marker.last_read_id > 0);

    // And it stays cleared afterwards.
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(!home.body.contains("bell__dot"), "bell stays quiet");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_lists_render(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    // Bob follows Alice.
    plamenu_db::follow::create(&pool, bob.id, alice.id, None)
        .await
        .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    // Alice's followers list shows Bob.
    let followers = get(&app, "/@alice/followers", Some(&cookie)).await;
    assert_eq!(followers.status, StatusCode::OK);
    assert!(followers.body.contains("Followers"));
    assert!(followers.body.contains("/@bob"));

    // Bob's following list shows Alice.
    let following = get(&app, "/@bob/following", Some(&cookie)).await;
    assert_eq!(following.status, StatusCode::OK);
    assert!(following.body.contains("/@alice"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hidden_follow_lists_stay_private_on_web(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    // Follows both ways, so both of Alice's lists have an entry to leak.
    plamenu_db::follow::create(&pool, bob.id, alice.id, None)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, alice.id, bob.id, None)
        .await
        .unwrap();
    sqlx::query!("UPDATE accounts SET hide_collections = true WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query!("UPDATE accounts SET hide_collections = true WHERE username = 'bob'")
        .execute(&pool)
        .await
        .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    // Anonymous visitors get the notice, never the accounts — same rule as
    // `GET /api/v1/accounts/{id}/followers`.
    for path in ["/@alice/followers", "/@alice/following"] {
        let resp = get(&app, path, None).await;
        assert_eq!(resp.status, StatusCode::OK);
        assert!(!resp.body.contains("/@bob\""), "{path} leaked to anon");
        assert!(resp.body.contains("chosen to hide"));
    }

    // Other signed-in users are shut out the same way (Alice viewing Bob's).
    // Alice's own handle stays in the nav chrome, so assert on the card list.
    let other = get(&app, "/@bob/followers", Some(&cookie)).await;
    assert_eq!(other.status, StatusCode::OK);
    assert!(!other.body.contains("account-list"));
    assert!(other.body.contains("chosen to hide"));

    // The owner still sees their own lists.
    let own = get(&app, "/@alice/followers", Some(&cookie)).await;
    assert_eq!(own.status, StatusCode::OK);
    assert!(own.body.contains("/@bob"));
}

/// A member who hid their own social graph must also disappear from *other*
/// accounts' follower lists on the web — except for the member themselves and
/// the list's owner. Here carol (the list owner) hides nothing; dave does.
#[sqlx::test(migrations = "../db/migrations")]
async fn hidden_member_filtered_from_other_accounts_lists_on_web(pool: PgPool) {
    let carol = seed_user(&pool, "carol", "carol@example.com", "carol-password-1").await;
    let dave = seed_user(&pool, "dave", "dave@example.com", "dave-password-1").await;
    let erin = seed_user(&pool, "erin", "erin@example.com", "erin-password-1").await;
    // dave (hidden) and erin (visible) both follow carol.
    plamenu_db::follow::create(&pool, dave.id, carol.id, None)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, erin.id, carol.id, None)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE accounts SET hide_collections = true WHERE id = $1",
        dave.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool);

    let count_cards = |body: &str| body.matches(r#"class="account-card""#).count();

    // Anonymous: carol's list renders (she hides nothing) but drops dave.
    let anon = get(&app, "/@carol/followers", None).await;
    assert_eq!(anon.status, StatusCode::OK);
    assert!(anon.body.contains("account-list"), "carol's list renders");
    assert!(!anon.body.contains("chosen to hide"));
    assert!(anon.body.contains("/@erin\""), "visible follower shown");
    assert!(
        !anon.body.contains("/@dave\""),
        "hidden follower leaked to anon"
    );

    // A third party (erin) signed in gets the same filtered list.
    let erin_cookie = login_as(&app, "erin@example.com", "erin-password-1").await;
    let third = get(&app, "/@carol/followers", Some(&erin_cookie)).await;
    assert!(
        !third.body.contains("/@dave\""),
        "hidden follower leaked to a third party"
    );

    // Exception 1: dave viewing carol's followers still sees himself — exactly
    // one card more than the third party does (both signed in, so any nav
    // chrome is symmetric and cancels out of the comparison).
    let dave_cookie = login_as(&app, "dave@example.com", "dave-password-1").await;
    let self_view = get(&app, "/@carol/followers", Some(&dave_cookie)).await;
    assert_eq!(
        count_cards(&self_view.body),
        count_cards(&third.body) + 1,
        "a hidden member still sees themselves in another account's list"
    );

    // Exception 2: carol, the list owner, sees every follower, dave included.
    let carol_cookie = login_as(&app, "carol@example.com", "carol-password-1").await;
    let owner = get(&app, "/@carol/followers", Some(&carol_cookie)).await;
    assert!(
        owner.body.contains("/@dave\""),
        "the list owner sees the hidden follower"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_nav_hides_features_disabled_for_visitors(pool: PgPool) {
    seed_alice(&pool).await;

    // With public previews switched on, the logged-out chrome advertises
    // Explore and Search.
    common::open_previews(&pool).await;
    let open = common::test_app(pool.clone());
    let open_login = get(&open, "/login", None).await;
    assert!(open_login.body.contains(r#"href="/public""#));
    assert!(open_login.body.contains(r#"href="/search""#));

    // With them off (the private default), those links are gone — no dead
    // links that just bounce to the sign-in page.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            timeline_preview_federated: false,
            timeline_preview_local: false,
            timeline_preview_tag: false,
            public_search: false,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    let locked = common::test_app_private(pool);
    let locked_login = get(&locked, "/login", None).await;
    assert!(!locked_login.body.contains(r#"href="/public""#));
    assert!(!locked_login.body.contains(r#"href="/search""#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notifications_require_a_session(pool: PgPool) {
    let app = app_with_alice(pool).await;
    // Every session-gated route bounces an anonymous visitor to the login page.
    for uri in [
        "/notifications",
        "/web/compose/suggestions?type=accounts&q=bob",
        "/settings/profile",
    ] {
        let resp = get(&app, uri, None).await;
        assert_eq!(resp.status, StatusCode::SEE_OTHER, "GET {uri}");
        assert_eq!(resp.location.as_deref(), Some("/login"), "GET {uri}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn search_finds_people_and_hashtags(pool: PgPool) {
    seed_alice(&pool).await;
    create_local_account(&pool, "bob", "Bob").await;
    // A known remote account, to exercise the exact `user@domain` lookup.
    let carol = RemoteUser::new("remote.example", "carol");
    remote::store_remote_actor(&pool, &carol.actor)
        .await
        .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;
    // A post carrying a hashtag, so a tag exists to find.
    compose(&app, &cookie, "exploring #plamenutest today").await;

    let people = get(&app, "/search?q=bob", Some(&cookie)).await;
    assert_eq!(people.status, StatusCode::OK);
    assert!(people.body.contains("Bob"));
    assert!(people.body.contains("/@bob"));

    // Regression: a full `@user@domain` handle for a known remote account must
    // resolve on the web search page. Postgres lexes `carol@remote.example` as
    // a single email token, so the old `account::search` full-text path never
    // matched the separately-indexed username and domain lexemes — the exact
    // `user@domain` lookup (shared with the API) is what finds it.
    let handle = get(&app, "/search?q=@carol@remote.example", Some(&cookie)).await;
    assert_eq!(handle.status, StatusCode::OK);
    assert!(
        handle.body.contains("carol@remote.example"),
        "web search should find a remote account by its full handle"
    );

    let tags = get(&app, "/search?q=plamenutest", Some(&cookie)).await;
    assert!(tags.body.contains("#plamenutest"));
    assert!(tags.body.contains("/tags/plamenutest"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn search_resolves_a_url_to_its_post(pool: PgPool) {
    seed_alice(&pool).await;
    common::open_previews(&pool).await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let note_uri = "https://remote.example/notes/aohyqlglkiai009h";
    stub.objects.lock().unwrap().insert(
        note_uri.to_owned(),
        serde_json::json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>hello from afar</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-06-11T08:00:00Z",
        }),
    );
    let app = common::test_app_with(pool, stub.clone());
    let cookie = login(&app).await;

    // Regression: pasting a post URL into the web search box used to fall
    // into status full-text search (which never matches a URL) instead of
    // the API's `resolve_url` path. Fetch, ingest and show the post.
    let uri = format!("/search?q={note_uri}");
    let resp = get(&app, &uri, Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.body.contains("hello from afar"),
        "web search should resolve a post URL to the post itself"
    );
    assert!(resp.body.contains("bob@remote.example"));
    assert!(stub.fetches().contains(&note_uri.to_owned()));

    // Anonymous viewers never resolve URLs (matching the API, where
    // `resolve` requires authentication) — even now that the post is known.
    let anon = get(&app, &uri, None).await;
    assert_eq!(anon.status, StatusCode::OK);
    assert!(!anon.body.contains("hello from afar"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_suggestions_complete_mentions_and_hashtags(pool: PgPool) {
    seed_alice(&pool).await;
    create_local_account(&pool, "bob", "Bob the Builder").await;
    // A known remote account answers an exact `user@domain` query without any
    // webfinger round-trip (the endpoint never resolves).
    let carol = RemoteUser::new("remote.example", "carol");
    remote::store_remote_actor(&pool, &carol.actor)
        .await
        .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;
    compose(&app, &cookie, "exploring #plamenutest today").await;

    let people = get(
        &app,
        "/web/compose/suggestions?type=accounts&q=bob",
        Some(&cookie),
    )
    .await;
    assert_eq!(people.status, StatusCode::OK);
    let items: serde_json::Value = serde_json::from_str(&people.body).unwrap();
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["acct"], "bob");
    assert_eq!(items[0]["display_name"], "Bob the Builder");

    let exact = get(
        &app,
        "/web/compose/suggestions?type=accounts&q=carol@remote.example",
        Some(&cookie),
    )
    .await;
    let items: serde_json::Value = serde_json::from_str(&exact.body).unwrap();
    assert_eq!(items[0]["acct"], "carol@remote.example");

    // Hashtags prefix-match, sigil tolerated.
    let tags = get(
        &app,
        "/web/compose/suggestions?type=hashtags&q=%23plamenu",
        Some(&cookie),
    )
    .await;
    assert_eq!(tags.status, StatusCode::OK);
    let items: serde_json::Value = serde_json::from_str(&tags.body).unwrap();
    assert_eq!(items[0]["name"], "plamenutest");

    // A blank query and an unknown type are both empty results, not errors.
    for uri in [
        "/web/compose/suggestions?type=accounts&q=",
        "/web/compose/suggestions?type=bogus&q=bob",
    ] {
        let resp = get(&app, uri, Some(&cookie)).await;
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.body, "[]");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn search_page_is_open_to_anonymous_visitors(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let resp = get(&app, "/search", None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#"action="/search""#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn tag_timeline_shows_tagged_posts(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    compose(&app, &cookie, "first #plamenutest post").await;

    // Anonymous visitors can read a hashtag timeline.
    let resp = get(&app, "/tags/plamenutest", None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("#plamenutest"));
    assert!(resp.body.contains("first"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_page_offers_full_editor(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let resp = get(&app, "/compose", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("Content warning"));
    assert!(resp.body.contains("Add a poll"));
    assert!(resp.body.contains(r#"name="poll_options[]""#));
    // The layout uses collapsible groups behind toolbar toggle buttons, and the
    // textarea no longer forces text (media-only posts are allowed).
    assert!(resp.body.contains(r#"data-compose-toggle="poll""#));
    assert!(resp.body.contains(r#"data-compose-section="media""#));
    // The media section advertises the attachment cap to the JS manager
    // and wraps the no-JS slots so they can be swapped for the dynamic list.
    assert!(resp.body.contains("data-media-max="));
    assert!(resp.body.contains("data-media-static"));
    // The poll section does the same for the dynamic choice-row builder.
    assert!(resp.body.contains("data-poll-max="));
    assert!(resp.body.contains("data-poll-static"));
    // The custom-emoji picker (trigger + popup shell; JS fills it).
    assert!(resp.body.contains("data-emoji-trigger"));
    assert!(resp.body.contains("data-emoji-search"));
    assert!(resp.body.contains("data-emoji-list"));
    let textarea = resp
        .body
        .split_once(r#"name="status""#)
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(tag, _)| tag)
        .expect("status textarea present");
    assert!(
        !textarea.contains("required"),
        "textarea must not be required: {textarea}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compose_publishes_a_poll(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "pick a colour"),
            ("visibility", "public"),
            ("poll_options[]", "Red"),
            ("poll_options[]", "Blue"),
            ("poll_expires_in", "86400"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let permalink = posted.location.expect("new post");

    // The author sees the poll results view (you can't vote your own poll).
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("Red"));
    assert!(thread.body.contains("Blue"));
    assert!(thread.body.contains("votes"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn another_user_can_vote_in_a_poll(pool: PgPool) {
    seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", "bob password 1").await;
    let app = common::test_app(pool);

    // Alice posts the poll.
    let alice = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&alice)).await.body);
    let permalink = post_multipart(
        &app,
        "/web/compose",
        &alice,
        &[
            ("csrf", &csrf),
            ("status", "tabs or spaces"),
            ("visibility", "public"),
            ("poll_options[]", "Tabs"),
            ("poll_options[]", "Spaces"),
        ],
    )
    .await
    .location
    .expect("new poll");
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    // Bob sees a live vote form and casts a vote.
    let bob = login_as(&app, "bob@example.com", "bob password 1").await;
    let before = get(&app, &permalink, Some(&bob)).await;
    assert!(
        before
            .body
            .contains(&format!("/web/statuses/{status_id}/vote"))
    );
    let bob_csrf = csrf_of(&before.body);

    let voted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/vote"),
        &bob,
        &[
            ("csrf", &bob_csrf),
            ("return_to", &permalink),
            ("choices[]", "0"),
        ],
    )
    .await;
    assert_eq!(voted.status, StatusCode::SEE_OTHER);

    // Afterwards Bob sees the results (a percentage), not the form.
    let after = get(&app, &permalink, Some(&bob)).await;
    assert!(after.body.contains('%'));
    assert!(after.body.contains("1 vote"));

    // The JS enhancement refreshes only this server-rendered region after its
    // fetch POST. It carries the authoritative own-choice highlight and does
    // not leak a second page shell into the existing document.
    let fragment = get(
        &app,
        &format!("/web/statuses/{status_id}/poll?return_to={permalink}"),
        Some(&bob),
    )
    .await;
    assert_eq!(fragment.status, StatusCode::OK);
    assert!(fragment.body.contains("data-poll="));
    assert!(
        fragment
            .body
            .contains(r#"<div class="poll__result is-own">"#),
        "own-choice class must be the element's effective class: {}",
        fragment.body
    );
    assert!(fragment.body.contains("1 vote"));
    assert!(
        !fragment.body.contains("<title"),
        "fragment leaked page shell"
    );
    assert!(
        !fragment
            .body
            .contains(&format!("/web/statuses/{status_id}/vote")),
        "voted viewer was offered the form again"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn quote_post_embeds_the_original(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let png = sample_png_bytes();
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "the original thought"),
            ("visibility", "public"),
            ("media_alt[]", "quoted warm square"),
        ],
        ("media[]", "quoted-square.png", "image/png", &png),
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let original = posted.location.expect("original post");
    let original_id = original.rsplit('/').next().unwrap().to_owned();

    // The compose page pre-targets the quote.
    let page = get(
        &app,
        &format!("/compose?quote={original_id}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Quote post"));
    assert!(page.body.contains(r#"name="quoted_status_id""#));

    let csrf = csrf_of(&page.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "my hot take"),
            ("visibility", "public"),
            ("quoted_status_id", &original_id),
        ],
    )
    .await;
    let permalink = posted.location.expect("quoting post");
    let quote_id: i64 = permalink.rsplit('/').next().unwrap().parse().unwrap();
    let stored = status::find_by_id(&pool, quote_id).await.unwrap().unwrap();
    assert!(
        stored
            .content
            .starts_with(r#"<p class="quote-inline">RE: "#),
        "quote fallback must be stored in the authored body: {}",
        stored.content
    );
    let federated = get_ap(&app, &format!("/users/alice/statuses/{quote_id}")).await;
    assert_eq!(federated.status, StatusCode::OK);
    let federated: serde_json::Value = serde_json::from_str(&federated.body).unwrap();
    assert!(
        federated["content"]
            .as_str()
            .unwrap()
            .contains(r#"class="quote-inline""#),
        "served ActivityPub body must retain the fallback: {federated}"
    );
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("my hot take"));
    assert!(thread.body.contains("quote-card"));
    assert!(thread.body.contains("the original thought"));
    let quote_card = thread
        .body
        .split_once(r#"<div class="quote-card">"#)
        .expect("embedded quote card")
        .1;
    assert!(
        quote_card.contains(r#"class="status__media""#)
            && quote_card.contains(r#"alt="quoted warm square""#),
        "the quoted post's media must remain in its card: {quote_card}"
    );
    assert!(
        !thread.body.contains(r#"class="quote-inline""#),
        "accepted native rendering must hide the fallback"
    );

    // The quoted original now reports a quote count, shown on its quote action.
    let oid: i64 = original_id.parse().unwrap();
    let counts = status::engagement_for(&pool, &[oid]).await.unwrap();
    assert_eq!(counts[&oid].quotes, 1);
    let original_thread = get(&app, &original, Some(&cookie)).await;
    assert!(original_thread.body.contains("action--quote"));
    assert!(
        original_thread.body.contains("m228-240 92-160"),
        "the quote action should use Mastodon's format_quote glyph"
    );
    assert!(
        original_thread
            .body
            .contains(r#"transform="translate(-3.625 27) scale(.03125)""#),
        "the quote glyph should be centred and optically match its neighbouring icons"
    );
}

/// The boosts / favourites engagement lists show who engaged, linked
/// from the thread detail counters; an unknown list segment is a 404.
#[sqlx::test(migrations = "../db/migrations")]
async fn engagement_lists_show_boosters_and_favers(pool: PgPool) {
    seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", "bob-password-1").await;
    let app = common::test_app(pool);
    let alice_cookie = login(&app).await;
    let permalink = compose(&app, &alice_cookie, "engage with me").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    let bob_cookie = login_as(&app, "bob@example.com", "bob-password-1").await;
    let csrf = csrf_of(&get(&app, &permalink, Some(&bob_cookie)).await.body);
    for verb in ["favourite", "reblog"] {
        let acted = post_form(
            &app,
            &format!("/web/statuses/{status_id}/{verb}"),
            &bob_cookie,
            &[("csrf", &csrf), ("return_to", &permalink)],
        )
        .await;
        assert_eq!(acted.status, StatusCode::SEE_OTHER);
    }

    // The detail view's engagement counters link into the lists.
    let thread = get(&app, &permalink, Some(&alice_cookie)).await;
    assert!(thread.body.contains(&format!("{permalink}/reblogs")));
    assert!(thread.body.contains(&format!("{permalink}/favourites")));

    let favs = get(
        &app,
        &format!("{permalink}/favourites"),
        Some(&alice_cookie),
    )
    .await;
    assert_eq!(favs.status, StatusCode::OK);
    assert!(favs.body.contains("/@bob"));

    let boosts = get(&app, &format!("{permalink}/reblogs"), Some(&alice_cookie)).await;
    assert_eq!(boosts.status, StatusCode::OK);
    assert!(boosts.body.contains("/@bob"));

    // The lists are as public as the post (anonymous viewers see them too).
    let anon = get(&app, &format!("{permalink}/favourites"), None).await;
    assert_eq!(anon.status, StatusCode::OK);
    assert!(anon.body.contains("/@bob"));

    let bogus = get(&app, &format!("{permalink}/nonsense"), None).await;
    assert_eq!(bogus.status, StatusCode::NOT_FOUND);
}

/// The quotes list renders the quoting posts and offers the quoted
/// author a per-quote revoke control that withdraws the quote.
#[sqlx::test(migrations = "../db/migrations")]
async fn quotes_list_lets_the_author_revoke(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let original = compose(&app, &cookie, "the quotable original").await;
    let original_id = original.rsplit('/').next().unwrap().to_owned();

    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "quoting this"),
            ("visibility", "public"),
            ("quoted_status_id", &original_id),
        ],
    )
    .await;
    let quote_permalink = posted.location.expect("quoting post");
    let quote_id = quote_permalink.rsplit('/').next().unwrap().to_owned();

    // The list shows the quoting post and, to the quoted author, the revoke
    // control.
    let quotes_path = format!("{original}/quotes");
    let quotes = get(&app, &quotes_path, Some(&cookie)).await;
    assert_eq!(quotes.status, StatusCode::OK);
    assert!(quotes.body.contains("quoting this"));
    let revoke_path = format!("/web/statuses/{original_id}/quotes/{quote_id}/revoke");
    assert!(quotes.body.contains(&revoke_path));

    let revoked = post_form(
        &app,
        &revoke_path,
        &cookie,
        &[("csrf", &csrf), ("return_to", &quotes_path)],
    )
    .await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);
    assert_eq!(revoked.location.as_deref(), Some(quotes_path.as_str()));

    let after = get(&app, &quotes_path, Some(&cookie)).await;
    assert!(!after.body.contains("quoting this"));
    assert!(after.body.contains("Nothing here yet."));
}

/// A content warning is a single gate over the whole post: a CW post with
/// sensitive media shows one toggle, not a separate one for text and media.
#[sqlx::test(migrations = "../db/migrations")]
async fn content_warning_is_one_toggle_over_text_and_media(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let png = sample_png_bytes();
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "the hidden body"),
            ("visibility", "public"),
            ("spoiler_text", "spoilers ahead"),
            ("media_alt[]", "secret square"),
        ],
        ("media[]", "square.png", "image/png", &png),
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let permalink = posted.location.expect("new post");

    let thread = get(&app, &permalink, Some(&cookie)).await;
    // Exactly one content-warning toggle and no separate media spoiler.
    assert_eq!(
        thread.body.matches("status__cw").count(),
        1,
        "expected a single CW toggle: {}",
        thread.body
    );
    assert!(
        !thread.body.contains("status__media-sensitive"),
        "media should sit inside the CW, not behind its own toggle: {}",
        thread.body
    );
    // The media still renders (inside the toggle), alt text and all.
    assert!(thread.body.contains("secret square"));
}

/// A described image thumbnail carries the ALT badge and its blurhash
/// (the JS lightbox/placeholder hooks); an undescribed one carries neither.
#[sqlx::test(migrations = "../db/migrations")]
async fn image_renders_alt_badge_and_blurhash(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let png = sample_png_bytes();
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "described picture"),
            ("visibility", "public"),
            ("media_alt[]", "a purple square"),
        ],
        ("media[]", "square.png", "image/png", &png),
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let thread = get(&app, &posted.location.expect("new post"), Some(&cookie)).await;
    assert!(thread.body.contains("media__badge"), "{}", thread.body);
    assert!(thread.body.contains(">ALT<"), "{}", thread.body);
    assert!(thread.body.contains("data-blurhash=\""), "{}", thread.body);

    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "bare picture"),
            ("visibility", "public"),
        ],
        ("media[]", "square.png", "image/png", &png),
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let thread = get(&app, &posted.location.expect("new post"), Some(&cookie)).await;
    assert!(!thread.body.contains("media__badge"), "{}", thread.body);
}

/// An audio attachment renders a real `<audio>` player instead of the
/// old bare "Attachment" link.
#[sqlx::test(migrations = "../db/migrations")]
async fn audio_attachment_renders_a_player(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let wav = sample_wav_bytes();
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "a tune"),
            ("visibility", "public"),
        ],
        ("media[]", "tune.wav", "audio/wav", &wav),
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let thread = get(&app, &posted.location.expect("new post"), Some(&cookie)).await;
    assert!(thread.body.contains("media--audio"), "{}", thread.body);
    assert!(thread.body.contains("<audio"), "{}", thread.body);
    assert!(thread.body.contains("controls"), "{}", thread.body);
}

/// A gifv (soundless clip) renders a still poster link by default —
/// badged "GIF" — and only autoplays as an unchromed muted loop once the
/// viewer turns the "Auto-play animated GIFs" preference on.
#[sqlx::test(migrations = "../db/migrations")]
async fn gifv_honors_the_autoplay_preference(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let mp4 = soundless_mp4_bytes();
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "a loop"),
            ("visibility", "public"),
        ],
        ("media[]", "loop.mp4", "video/mp4", &mp4),
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let permalink = posted.location.expect("new post");

    // Autoplay off (the default): a poster image linking to the clip, no
    // <video> element at all — plus the GIF badge either way.
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(!thread.body.contains("<video"), "{}", thread.body);
    assert!(thread.body.contains("data-gifv"), "{}", thread.body);
    assert!(thread.body.contains(">GIF<"), "{}", thread.body);

    // Flip the preference on: the tile becomes an unchromed muted loop.
    let pcsrf = csrf_of(&get(&app, "/settings/preferences", Some(&cookie)).await.body);
    let saved = post_form(
        &app,
        "/web/settings/preferences",
        &cookie,
        &[("csrf", &pcsrf), ("reading_autoplay_gifs", "true")],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);

    let thread = get(&app, &permalink, Some(&cookie)).await;
    let tag_start = thread.body.find("<video").expect("a video tag");
    let tag = &thread.body[tag_start..thread.body[tag_start..].find('>').unwrap() + tag_start];
    assert!(tag.contains("media__gifv"), "{tag}");
    for attr in ["autoplay", "muted", "loop", "playsinline"] {
        assert!(tag.contains(attr), "missing {attr}: {tag}");
    }
    assert!(
        !tag.contains("controls"),
        "gifv must not show player chrome: {tag}"
    );
    assert!(thread.body.contains(">GIF<"), "{}", thread.body);
}

/// The "always expand content warnings" reading preference opens the CW toggle
/// in the rendered timeline instead of leaving it collapsed.
#[sqlx::test(migrations = "../db/migrations")]
async fn reading_expand_spoilers_opens_the_warning(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let pcsrf = csrf_of(&get(&app, "/settings/preferences", Some(&cookie)).await.body);
    let saved = post_form(
        &app,
        "/web/settings/preferences",
        &cookie,
        &[("csrf", &pcsrf), ("reading_expand_spoilers", "true")],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);

    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "behind a warning"),
            ("visibility", "public"),
            ("spoiler_text", "cw"),
        ],
    )
    .await;
    let permalink = posted.location.expect("new post");

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(
        thread.body.contains(r#"<details class="status__cw" open>"#),
        "CW should render open: {}",
        thread.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn author_can_delete_their_status(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "delete me please").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    // The owner's thread offers a delete control.
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(
        thread
            .body
            .contains(&format!("/web/statuses/{status_id}/delete"))
    );
    let csrf = csrf_of(&thread.body);

    let deleted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/delete"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &permalink)],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);

    // The status is gone.
    let after = get(&app, &permalink, Some(&cookie)).await;
    assert_eq!(after.status, StatusCode::NOT_FOUND);
}

/// The owner's overflow menu carries the secondary verbs — pin,
/// conversation mute, quote policy, delete and delete-and-redraft — while the
/// moderation verbs (against other accounts) stay out of one's own menu.
#[sqlx::test(migrations = "../db/migrations")]
async fn overflow_menu_offers_owner_verbs(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "menu check").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("data-status-menu"));
    for verb in ["pin", "mute", "quote_policy", "redraft", "delete"] {
        assert!(
            thread
                .body
                .contains(&format!("/web/statuses/{status_id}/{verb}")),
            "menu should offer {verb}"
        );
    }
    assert!(thread.body.contains("Pin to profile"));
    assert!(thread.body.contains("Mute conversation"));
    assert!(thread.body.contains("Delete and re-draft"));
    assert!(thread.body.contains("Copy link to post"));
    // Moderation verbs target other people's posts, never one's own.
    assert!(!thread.body.contains("/web/accounts/"));
    assert!(!thread.body.contains("Block @alice"));
}

/// Pin / unpin and conversation mute / unmute round-trip through the
/// overflow menu forms, the label flipping with the state.
#[sqlx::test(migrations = "../db/migrations")]
async fn pin_and_conversation_mute_toggle_through_the_menu(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "toggle me").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();
    let csrf = csrf_of(&get(&app, &permalink, Some(&cookie)).await.body);
    let fields: &[(&str, &str)] = &[("csrf", &csrf), ("return_to", &permalink)];

    let pinned = post_form(
        &app,
        &format!("/web/statuses/{status_id}/pin"),
        &cookie,
        fields,
    )
    .await;
    assert_eq!(pinned.status, StatusCode::SEE_OTHER);
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("Unpin from profile"));

    let muted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/mute"),
        &cookie,
        fields,
    )
    .await;
    assert_eq!(muted.status, StatusCode::SEE_OTHER);
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("Unmute conversation"));

    for verb in ["unpin", "unmute"] {
        let undone = post_form(
            &app,
            &format!("/web/statuses/{status_id}/{verb}"),
            &cookie,
            fields,
        )
        .await;
        assert_eq!(undone.status, StatusCode::SEE_OTHER);
    }
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("Pin to profile"));
    assert!(thread.body.contains("Mute conversation"));
}

/// Pinned posts lead the profile under a "Pinned" marker and don't
/// repeat in the chronological feed below.
#[sqlx::test(migrations = "../db/migrations")]
async fn pinned_posts_lead_the_profile(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let pinned_permalink = compose(&app, &cookie, "the pinned classic").await;
    compose(&app, &cookie, "a newer unpinned post").await;
    let status_id = pinned_permalink.rsplit('/').next().unwrap().to_owned();
    let csrf = csrf_of(&get(&app, &pinned_permalink, Some(&cookie)).await.body);

    post_form(
        &app,
        &format!("/web/statuses/{status_id}/pin"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &pinned_permalink)],
    )
    .await;

    let profile = get(&app, "/@alice", Some(&cookie)).await;
    assert!(profile.body.contains(r#"data-kind="pinned""#));
    assert!(profile.body.contains("the pinned classic"));
    // The pinned card leads the page — before the newer post.
    let pinned_at = profile.body.find("the pinned classic").unwrap();
    let newer_at = profile.body.find("a newer unpinned post").unwrap();
    assert!(pinned_at < newer_at);
    // And it isn't repeated in the feed below.
    assert_eq!(profile.body.matches("the pinned classic").count(), 1);
}

/// Delete-and-redraft removes the post and reopens the composer
/// prefilled with its source text, content warning and visibility.
#[sqlx::test(migrations = "../db/migrations")]
async fn redraft_deletes_and_prefills_the_composer(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/", Some(&cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "second draft coming"),
            ("spoiler_text", "spoilers ahead"),
            ("visibility", "unlisted"),
        ],
    )
    .await;
    let permalink = posted.location.unwrap();
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();
    let csrf = csrf_of(&get(&app, &permalink, Some(&cookie)).await.body);

    let redrafted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/redraft"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &permalink)],
    )
    .await;
    assert_eq!(redrafted.status, StatusCode::SEE_OTHER);
    let location = redrafted.location.unwrap();
    assert!(location.starts_with("/compose?"), "got {location}");

    // The composer comes back prefilled…
    let composer = get(&app, &location, Some(&cookie)).await;
    assert!(composer.body.contains("second draft coming"));
    assert!(composer.body.contains(r#"value="spoilers ahead""#));
    assert!(composer.body.contains(r#"value="unlisted" selected"#));
    // …and the original is gone.
    let after = get(&app, &permalink, Some(&cookie)).await;
    assert_eq!(after.status, StatusCode::NOT_FOUND);
}

/// The owner changes who can quote after posting; the menu's selector
/// tracks the stored policy and the detail metadata spells it out.
#[sqlx::test(migrations = "../db/migrations")]
async fn quote_policy_updates_through_the_menu(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "quote policy check").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains(r#"value="public" selected"#));
    let csrf = csrf_of(&thread.body);

    let changed = post_form(
        &app,
        &format!("/web/statuses/{status_id}/quote_policy"),
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", &permalink),
            ("policy", "nobody"),
        ],
    )
    .await;
    assert_eq!(changed.status, StatusCode::SEE_OTHER);
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains(r#"value="nobody" selected"#));
    assert!(thread.body.contains("Quotes disabled"));

    // An unknown policy value is rejected, not stored as something else.
    let bogus = post_form(
        &app,
        &format!("/web/statuses/{status_id}/quote_policy"),
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", &permalink),
            ("policy", "everyone"),
        ],
    )
    .await;
    assert_eq!(bogus.status, StatusCode::BAD_REQUEST);
}

/// Another user's post offers the account-moderation verbs in its
/// overflow menu, and the forms actually create the mute / block rows.
#[sqlx::test(migrations = "../db/migrations")]
async fn moderation_verbs_mute_and_block_from_the_menu(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let bob_cookie = login_as(&app, "bob@example.com", PASSWORD).await;
    let permalink = compose(&app, &bob_cookie, "moderate me").await;

    let cookie = login(&app).await;
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("Mute @bob"));
    assert!(thread.body.contains("Block @bob"));
    assert!(
        thread
            .body
            .contains(&format!("/web/accounts/{}/mute", bob.id))
    );
    // Owner-only verbs stay out of someone else's menu.
    assert!(!thread.body.contains("/redraft"));
    assert!(!thread.body.contains("Pin to profile"));
    // The in-place moderation hooks: the article names its author, and the
    // forms carry the kind, target and undo direction for the JS to flip.
    assert!(
        thread
            .body
            .contains(&format!(r#"data-author-id="{}""#, bob.id))
    );
    assert!(thread.body.contains(r#"data-mod="mute""#));
    assert!(thread.body.contains(r#"data-mod="block""#));
    assert!(
        thread
            .body
            .contains(&format!(r#"data-mod-account="{}""#, bob.id))
    );
    assert!(thread.body.contains(&format!(
        r#"data-mod-undo-action="/web/accounts/{}/unmute""#,
        bob.id
    )));
    assert!(thread.body.contains(r#"data-mod-undo-label="Unmute @bob""#));
    let csrf = csrf_of(&thread.body);
    let fields: &[(&str, &str)] = &[("csrf", &csrf), ("return_to", &permalink)];

    let muted = post_form(
        &app,
        &format!("/web/accounts/{}/mute", bob.id),
        &cookie,
        fields,
    )
    .await;
    assert_eq!(muted.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::mute::find_active(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_some()
    );

    let blocked = post_form(
        &app,
        &format!("/web/accounts/{}/block", bob.id),
        &cookie,
        fields,
    )
    .await;
    assert_eq!(blocked.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::block::exists(&pool, alice.id, bob.id)
            .await
            .unwrap()
    );
}

/// A remote post's menu links to the original page and offers a
/// user-level domain block for the author's server.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_posts_offer_original_page_and_domain_block(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let remote_bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let remote_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/1",
            account_id: stored_bob.id,
            content: "<p>hello from afar</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: Some("https://remote.example/@bob/1"),
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let resolved = get(
        &app,
        &format!("/web/statuses/{}", remote_status.id),
        Some(&cookie),
    )
    .await;
    let permalink = resolved.location.expect("thread redirect");
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("Open original page"));
    assert!(
        thread
            .body
            .contains(r#"href="https://remote.example/@bob/1""#)
    );
    assert!(thread.body.contains("Block domain remote.example"));
    // The in-place moderation hooks for the server-level verb.
    assert!(
        thread
            .body
            .contains(r#"data-author-domain="remote.example""#)
    );
    assert!(thread.body.contains(r#"data-mod="domain""#));
    assert!(
        thread
            .body
            .contains(r#"data-mod-undo-action="/web/domains/unblock""#)
    );
    let csrf = csrf_of(&thread.body);

    let blocked = post_form(
        &app,
        "/web/domains/block",
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", &permalink),
            ("domain", "remote.example"),
        ],
    )
    .await;
    assert_eq!(blocked.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::account_domain_block::exists(&pool, alice.id, "remote.example")
            .await
            .unwrap()
    );
}

/// Another account's profile carries an overflow menu with the whole-account
/// moderation verbs (the profile-level counterparts of the status menu), the
/// list-management and endorsement ("Feature on profile") entries, and the
/// join date — none of which appear on one's own profile.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_menu_offers_account_actions_and_join_date(pool: PgPool) {
    seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let profile = get(&app, "/@bob", Some(&cookie)).await;
    assert_eq!(profile.status, StatusCode::OK);
    assert!(profile.body.contains("Mute @bob"));
    assert!(profile.body.contains("Block @bob"));
    assert!(profile.body.contains("Report @bob"));
    assert!(
        profile
            .body
            .contains(&format!("/web/accounts/{}/mute", bob.id))
    );
    assert!(
        profile
            .body
            .contains(&format!("/web/accounts/{}/report?return_to=", bob.id))
    );
    // The list-management and endorsement verbs are wired to real endpoints.
    assert!(profile.body.contains("Add or remove from lists"));
    assert!(
        profile
            .body
            .contains(&format!("/web/accounts/{}/lists", bob.id))
    );
    assert!(profile.body.contains("Feature on profile"));
    assert!(
        profile
            .body
            .contains(&format!("/web/accounts/{}/endorse", bob.id))
    );
    // A local profile has no "original page" elsewhere to link to.
    assert!(!profile.body.contains("View original page"));
    // Registration date is now shown.
    assert!(profile.body.contains("Joined "));

    // None of the moderation verbs point at oneself.
    let own = get(&app, "/@alice", Some(&cookie)).await;
    assert!(!own.body.contains("Block @alice"));
    assert!(!own.body.contains("Report @alice"));
}

/// Mute/unmute and block/unblock round-trip through the profile menu, and the
/// button flips to its inverse once the relationship exists.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_menu_mute_and_block_round_trip(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let profile = get(&app, "/@bob", Some(&cookie)).await;
    let csrf = csrf_of(&profile.body);
    let fields: &[(&str, &str)] = &[("csrf", &csrf), ("return_to", "/@bob")];

    let muted = post_form(
        &app,
        &format!("/web/accounts/{}/mute", bob.id),
        &cookie,
        fields,
    )
    .await;
    assert_eq!(muted.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::mute::find_active(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_some()
    );
    let after_mute = get(&app, "/@bob", Some(&cookie)).await;
    assert!(after_mute.body.contains("Unmute @bob"));

    let unmuted = post_form(
        &app,
        &format!("/web/accounts/{}/unmute", bob.id),
        &cookie,
        fields,
    )
    .await;
    assert_eq!(unmuted.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::mute::find_active(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_none()
    );

    let blocked = post_form(
        &app,
        &format!("/web/accounts/{}/block", bob.id),
        &cookie,
        fields,
    )
    .await;
    assert_eq!(blocked.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::block::exists(&pool, alice.id, bob.id)
            .await
            .unwrap()
    );
    let after_block = get(&app, "/@bob", Some(&cookie)).await;
    assert!(after_block.body.contains("Unblock @bob"));

    let unblocked = post_form(
        &app,
        &format!("/web/accounts/{}/unblock", bob.id),
        &cookie,
        fields,
    )
    .await;
    assert_eq!(unblocked.status, StatusCode::SEE_OTHER);
    assert!(
        !plamenu_db::block::exists(&pool, alice.id, bob.id)
            .await
            .unwrap()
    );
}

/// The profile shows compact badges for the viewer's own sanctions on the
/// account — mute, block and the reverse "blocks you" — and no badge row at
/// all on a clean profile or one's own.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_shows_viewer_moderation_badges(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let clean = get(&app, "/@bob", Some(&cookie)).await;
    assert!(!clean.body.contains("profile__moderation"));

    mute::upsert(&pool, alice.id, bob.id, false, None)
        .await
        .unwrap();
    let muted = get(&app, "/@bob", Some(&cookie)).await;
    assert!(muted.body.contains("Muted by you"));
    assert!(!muted.body.contains("Blocked by you"));

    block::create(&pool, alice.id, bob.id, None).await.unwrap();
    let blocked = get(&app, "/@bob", Some(&cookie)).await;
    assert!(blocked.body.contains("Blocked by you"));

    // The reverse edge: bob blocking alice shows as "Blocks you".
    block::create(&pool, bob.id, alice.id, None).await.unwrap();
    let blocked_by = get(&app, "/@bob", Some(&cookie)).await;
    assert!(blocked_by.body.contains("Blocks you"));

    // One's own profile never carries a moderation readout.
    let own = get(&app, "/@alice", Some(&cookie)).await;
    assert!(!own.body.contains("profile__moderation"));
}

/// Limited accounts retain a visible sanction badge for logged-in viewers,
/// while suspended profiles are unavailable to every ordinary viewer.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_shows_admin_sanction_badges(pool: PgPool) {
    seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    account::silence(&pool, bob.id).await.unwrap();
    let limited = get(&app, "/@bob", Some(&cookie)).await;
    assert!(limited.body.contains(">Limited<"));

    account::unsilence(&pool, bob.id).await.unwrap();
    account::suspend(&pool, bob.id, "local").await.unwrap();
    let suspended = get(&app, "/@bob", Some(&cookie)).await;
    assert_eq!(suspended.status, StatusCode::FORBIDDEN);

    let anon = get(&app, "/@bob", None).await;
    assert_eq!(anon.status, StatusCode::FORBIDDEN);
}

/// A remote profile's badge row covers the server-level states: the viewer's
/// own domain block and the admin domain block's severity.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_profile_shows_server_badges(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let remote_bob = RemoteUser::new("remote.example", "bob");
    remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    account_domain_block::create(&pool, alice.id, "remote.example")
        .await
        .unwrap();
    let blocked = get(&app, "/@bob@remote.example", Some(&cookie)).await;
    assert!(blocked.body.contains("Server blocked by you"));
    assert!(!blocked.body.contains("Server limited"));

    instance_policy::create_domain_block(
        &pool,
        instance_policy::NewDomainBlock {
            domain: "remote.example",
            severity: "silence",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    let limited = get(&app, "/@bob@remote.example", Some(&cookie)).await;
    assert!(limited.body.contains("Server limited"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_history_profile_control_is_feature_gated_compact_and_viewer_local(pool: PgPool) {
    history_db::save_settings(&pool, false, 90, false)
        .await
        .unwrap();
    let alice = seed_alice(&pool).await;
    sqlx::query!(
        "UPDATE users SET time_zone = 'Asia/Tbilisi' WHERE account_id = $1",
        alice.id,
    )
    .execute(&pool)
    .await
    .unwrap();
    let remote_bob = RemoteUser::new("remote.example", "bob");
    let bob = remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let disabled = get(&app, "/@bob@remote.example", Some(&cookie)).await;
    assert_eq!(disabled.status, StatusCode::OK);
    assert!(!disabled.body.contains("data-remote-history="));
    assert!(!disabled.body.contains(">Remote history<"));

    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE remote_history_states
         SET state = 'complete', last_success_at = '2026-08-14 08:15:00+00',
             automatic_retry_at = now() + interval '6 hours'
         WHERE account_id = $1",
        bob.id,
    )
    .execute(&pool)
    .await
    .unwrap();

    let enabled = get(&app, "/@bob@remote.example", Some(&cookie)).await;
    assert!(enabled.body.contains("data-remote-history="));
    assert!(enabled.body.contains("data-remote-history-region"));
    assert!(enabled.body.contains(">Remote history<"));
    assert!(enabled.body.contains(">Aug 14, 2026, 12:15</time>"));
    assert!(enabled.body.contains("data-absolute"));
    assert_eq!(history_db::pending_count(&pool).await.unwrap(), 0);
}

/// The profile "Report" link opens the same report form addressed to the whole
/// account (no post pre-checked), and submitting it files a report with no
/// attached statuses through the shared `create_report`.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_report_files_an_account_report(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let bob_cookie = login_as(&app, "bob@example.com", PASSWORD).await;
    compose(&app, &bob_cookie, "a post that could back a report").await;

    let cookie = login(&app).await;
    let report_path = format!("/web/accounts/{}/report", bob.id);
    let page = get(&app, &report_path, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Report @bob"));
    assert!(page.body.contains("this account"));
    // The account's posts are offered as optional evidence, none pre-checked.
    assert!(page.body.contains("a post that could back a report"));
    assert!(!page.body.contains("checked"));

    let csrf = csrf_of(&page.body);
    let posted = post_form(
        &app,
        &report_path,
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", "/@bob"),
            ("category", "spam"),
            ("comment", "harassing strangers"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let done = posted.location.expect("confirmation redirect");
    assert!(done.starts_with(&format!("{report_path}?done=1")));

    let reports = report::list_by_reporter(&pool, alice.id).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].category, "spam");
    assert!(reports[0].status_ids.is_empty());

    let confirmation = get(&app, &done, Some(&cookie)).await;
    assert!(confirmation.body.contains("Thanks for reporting"));
}

/// A remote account's profile menu links to its original page and offers a
/// user-level domain block for its server, which flips to "Unblock" once set.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_profile_offers_original_page_and_domain_block(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let remote_bob = RemoteUser::new("remote.example", "bob");
    remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let profile = get(&app, "/@bob@remote.example", Some(&cookie)).await;
    assert_eq!(profile.status, StatusCode::OK);
    assert!(profile.body.contains("View original page"));
    assert!(profile.body.contains("Block server remote.example"));

    let csrf = csrf_of(&profile.body);
    let blocked = post_form(
        &app,
        "/web/domains/block",
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", "/@bob@remote.example"),
            ("domain", "remote.example"),
        ],
    )
    .await;
    assert_eq!(blocked.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::account_domain_block::exists(&pool, alice.id, "remote.example")
            .await
            .unwrap()
    );

    let after = get(&app, "/@bob@remote.example", Some(&cookie)).await;
    assert!(after.body.contains("Unblock server remote.example"));

    let unblocked = post_form(
        &app,
        "/web/domains/unblock",
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", "/@bob@remote.example"),
            ("domain", "remote.example"),
        ],
    )
    .await;
    assert_eq!(unblocked.status, StatusCode::SEE_OTHER);
    assert!(
        !plamenu_db::account_domain_block::exists(&pool, alice.id, "remote.example")
            .await
            .unwrap()
    );
}

/// A silenced local account's profile is stripped to the handle and a notice
/// for anonymous visitors — no posts — while a logged-in viewer still sees the
/// full profile, annotated with a notice.
#[sqlx::test(migrations = "../db/migrations")]
async fn silenced_profile_hidden_from_anonymous_but_full_for_members(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    compose(&app, &cookie, "a quiet secret post").await;
    plamenu_db::account::silence(&pool, alice.id).await.unwrap();

    // Anonymous: only the handle and the notice, never the posts.
    let anon = get(&app, "/@alice", None).await;
    assert_eq!(anon.status, StatusCode::OK);
    assert!(anon.body.contains("@alice"));
    assert!(anon.body.contains("has been silenced"));
    assert!(
        !anon.body.contains("a quiet secret post"),
        "anonymous visitors see none of a silenced account's posts"
    );

    // Logged-in: the full profile, plus the notice, still shows the posts.
    let member = get(&app, "/@alice", Some(&cookie)).await;
    assert_eq!(member.status, StatusCode::OK);
    assert!(member.body.contains("a quiet secret post"));
    assert!(member.body.contains("This account has been silenced."));
}

/// A `silence` domain block limits every account on that server: the remote
/// profile shows anonymous visitors only the notice and a link back to the
/// original page on the home server.
#[sqlx::test(migrations = "../db/migrations")]
async fn domain_silence_strips_remote_profile_for_anonymous(pool: PgPool) {
    seed_alice(&pool).await;
    let remote_bob = RemoteUser::new("remote.example", "bob");
    remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    plamenu_db::instance_policy::create_domain_block(
        &pool,
        plamenu_db::instance_policy::NewDomainBlock {
            domain: "remote.example",
            severity: "silence",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool.clone());

    let anon = get(&app, "/@bob@remote.example", None).await;
    assert_eq!(anon.status, StatusCode::OK);
    assert!(anon.body.contains("has been silenced"));
    assert!(anon.body.contains("Open original page"));
}

/// The overflow menu on another user's post links to the report form,
/// which carries Mastodon's stepper fields as one plain multi-field form —
/// category radios (violation only when rules exist), the rule picker, the
/// target's recent posts with the reported one pre-checked, the comment box —
/// and no forward toggle for a local target.
#[sqlx::test(migrations = "../db/migrations")]
async fn report_page_offers_the_stepper_form(pool: PgPool) {
    seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", "pw").await;
    db_rule::create(&pool, "No spamming", "Repetitive posts get removed", None)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let bob_cookie = login_as(&app, "bob@example.com", "pw").await;
    let reported = compose(&app, &bob_cookie, "an objectionable post").await;
    compose(&app, &bob_cookie, "an unrelated post").await;
    let reported_id = reported.rsplit('/').next().unwrap();

    let cookie = login(&app).await;
    let thread = get(&app, &reported, Some(&cookie)).await;
    assert!(thread.body.contains("Report @bob"));
    assert!(
        thread
            .body
            .contains(&format!("/web/statuses/{reported_id}/report?return_to="))
    );

    let page = get(
        &app,
        &format!("/web/statuses/{reported_id}/report"),
        Some(&cookie),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Report @bob"));
    for value in ["spam", "legal", "violation", "other"] {
        assert!(
            page.body.contains(&format!(r#"value="{value}" required"#)),
            "missing category {value}"
        );
    }
    assert!(page.body.contains("No spamming"));
    assert!(page.body.contains("Repetitive posts get removed"));
    // The reported post leads the evidence picker pre-checked; other recent
    // posts by the target are offered unchecked.
    assert!(
        page.body
            .contains(&format!(r#"value="{reported_id}" checked"#))
    );
    assert!(page.body.contains("an objectionable post"));
    assert!(page.body.contains("an unrelated post"));
    assert!(page.body.contains(r#"name="comment""#));
    // A local target has no origin server to forward to.
    assert!(!page.body.contains(r#"name="forward""#));
}

/// Submitting the form files the report through the same action as
/// `POST /api/v1/reports` and lands on the confirmation view with its
/// mute/block shortcuts.
#[sqlx::test(migrations = "../db/migrations")]
async fn report_form_files_a_report_and_confirms(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", "pw").await;
    let app = common::test_app(pool.clone());
    let bob_cookie = login_as(&app, "bob@example.com", "pw").await;
    let reported = compose(&app, &bob_cookie, "spam spam spam").await;
    let reported_id = reported.rsplit('/').next().unwrap();

    let cookie = login(&app).await;
    let report_path = format!("/web/statuses/{reported_id}/report");
    let page = get(&app, &report_path, Some(&cookie)).await;
    let csrf = csrf_of(&page.body);
    let posted = post_form(
        &app,
        &report_path,
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", &reported),
            ("category", "spam"),
            ("status_ids[]", reported_id),
            ("comment", "keeps posting the same links"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let done = posted.location.expect("confirmation redirect");
    assert!(done.starts_with(&format!("{report_path}?done=1")));

    let reports = report::list_by_reporter(&pool, alice.id).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].category, "spam");
    assert_eq!(reports[0].comment, "keeps posting the same links");
    assert_eq!(
        reports[0].status_ids,
        vec![reported_id.parse::<i64>().unwrap()]
    );
    assert_eq!(reports[0].forwarded, Some(false));

    let confirmation = get(&app, &done, Some(&cookie)).await;
    assert!(confirmation.body.contains("Thanks for reporting"));
    assert!(confirmation.body.contains("Mute @bob"));
    assert!(confirmation.body.contains("Block @bob"));
    assert!(confirmation.body.contains(&format!(r#"href="{reported}""#)));
}

/// Citing rules forces the `violation` category, exactly like the API
/// (`ReportService`), whatever the radio said.
#[sqlx::test(migrations = "../db/migrations")]
async fn citing_a_rule_forces_the_violation_category(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", "pw").await;
    let rule = db_rule::create(&pool, "Be kind", "", None).await.unwrap();
    let app = common::test_app(pool.clone());
    let bob_cookie = login_as(&app, "bob@example.com", "pw").await;
    let reported = compose(&app, &bob_cookie, "unkind words").await;
    let reported_id = reported.rsplit('/').next().unwrap();

    let cookie = login(&app).await;
    let report_path = format!("/web/statuses/{reported_id}/report");
    let csrf = csrf_of(&get(&app, &report_path, Some(&cookie)).await.body);
    let rule_id = rule.id.to_string();
    let posted = post_form(
        &app,
        &report_path,
        &cookie,
        &[
            ("csrf", &csrf),
            ("category", "spam"),
            ("rule_ids[]", &rule_id),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);

    let reports = report::list_by_reporter(&pool, alice.id).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].category, "violation");
    assert_eq!(reports[0].rule_ids.as_deref(), Some(&[rule.id][..]));
}

/// A remote target adds the forward toggle, and forwarding is recorded
/// on the stored report like the API's `forward` param.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_report_offers_the_forward_toggle(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let remote_bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let remote_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/1",
            account_id: stored_bob.id,
            content: "<p>rude from afar</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: Some("https://remote.example/@bob/1"),
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let report_path = format!("/web/statuses/{}/report", remote_status.id);
    let page = get(&app, &report_path, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Report @bob@remote.example"));
    assert!(page.body.contains(r#"name="forward""#));
    assert!(
        page.body
            .contains("Also forward this report to remote.example")
    );

    let csrf = csrf_of(&page.body);
    let posted = post_form(
        &app,
        &report_path,
        &cookie,
        &[
            ("csrf", &csrf),
            ("category", "other"),
            ("forward", "1"),
            ("comment", "rude"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let reports = report::list_by_reporter(&pool, alice.id).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].forwarded, Some(true));
}

/// The gates — own posts can't be reported (the menu never offers it
/// and the page 404s), and the POST rejects a bad CSRF token.
#[sqlx::test(migrations = "../db/migrations")]
async fn report_gates_own_posts_and_csrf(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", "pw").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let own = compose(&app, &cookie, "my own post").await;
    let own_id = own.rsplit('/').next().unwrap();

    let thread = get(&app, &own, Some(&cookie)).await;
    assert!(!thread.body.contains("Report @alice"));
    let page = get(
        &app,
        &format!("/web/statuses/{own_id}/report"),
        Some(&cookie),
    )
    .await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);

    let bob_cookie = login_as(&app, "bob@example.com", "pw").await;
    let reported = compose(&app, &bob_cookie, "bob's post").await;
    let reported_id = reported.rsplit('/').next().unwrap();
    let posted = post_form(
        &app,
        &format!("/web/statuses/{reported_id}/report"),
        &cookie,
        &[("csrf", "bogus"), ("category", "spam")],
    )
    .await;
    assert_eq!(posted.status, StatusCode::FORBIDDEN);
    assert!(
        report::list_by_reporter(&pool, alice.id)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The "all-JS menu ships hidden" rule was reworked: the overflow
/// menu now always carries the engagement-list links, so it is a working
/// no-JS disclosure even for an anonymous viewer of a local post.
#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_menu_offers_the_engagement_lists(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "public menu check").await;

    let public = get(&app, "/public", None).await;
    assert!(public.body.contains("data-copy-link"));
    assert!(public.body.contains(&format!("{permalink}/reblogs")));
    assert!(public.body.contains(&format!("{permalink}/quotes")));
    assert!(public.body.contains(&format!("{permalink}/favourites")));
    // No state-changing verbs are offered logged-out.
    assert!(!public.body.contains("Mute conversation"));
    assert!(!public.body.contains("/web/domains/block"));
}

/// Timeline cards carry the account flags (bot, locked) beside the
/// handle and the language / edited / visibility chips in the meta corner.
#[sqlx::test(migrations = "../db/migrations")]
async fn status_cards_surface_account_flags_and_metadata(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    let csrf = csrf_of(&get(&app, "/", Some(&cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "metadata check"),
            ("visibility", "public"),
            ("language", "en"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);

    sqlx::query!("UPDATE accounts SET is_bot = true, locked = true WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query!("UPDATE statuses SET edited_at = now()")
        .execute(&pool)
        .await
        .unwrap();

    let home = get(&app, "/", Some(&cookie)).await;
    assert!(home.body.contains(r#"title="Automated account""#));
    assert!(home.body.contains(r#"title="Follows require approval""#));
    assert!(home.body.contains(r#"class="status__vis" title="Public""#));
    // The language chip is detail-only; closed cards don't carry it.
    assert!(!home.body.contains("status__lang"));
    assert!(home.body.contains(r#"class="status__edited""#));
    // The relative time carries the full instant as its tooltip.
    assert!(home.body.contains("UTC\""));
}

/// A reply names its target ("Replying to @…") and links into the
/// parent thread through the `/web/statuses/{id}` resolver, which redirects
/// to the canonical permalink. Also pins down the `in_reply_to_account_id`
/// entity fix the line reads from.
#[sqlx::test(migrations = "../db/migrations")]
async fn replies_name_their_target_and_resolve_the_parent(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;

    let alice_cookie = login(&app).await;
    let parent_permalink = compose(&app, &alice_cookie, "parent post").await;
    let parent_id = parent_permalink.rsplit('/').next().unwrap().to_owned();

    let bob_cookie = login_as(&app, "bob@example.com", PASSWORD).await;
    let csrf = csrf_of(&get(&app, "/", Some(&bob_cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &bob_cookie,
        &[
            ("csrf", &csrf),
            ("status", "@alice hello there"),
            ("visibility", "public"),
            ("in_reply_to_id", &parent_id),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let reply_permalink = posted.location.expect("redirect to the reply");

    let reply_page = get(&app, &reply_permalink, Some(&bob_cookie)).await;
    assert!(reply_page.body.contains("Replying to @alice"));
    assert!(
        reply_page
            .body
            .contains(&format!(r#"href="/web/statuses/{parent_id}""#))
    );

    // The resolver bounces a bare status id to its canonical permalink,
    // anchored to the opened post (deep-link scroll target).
    let resolved = get(
        &app,
        &format!("/web/statuses/{parent_id}"),
        Some(&bob_cookie),
    )
    .await;
    assert_eq!(resolved.status, StatusCode::SEE_OTHER);
    let expected = format!("{parent_permalink}#post-{parent_id}");
    assert_eq!(resolved.location.as_deref(), Some(expected.as_str()));

    // A self-thread reply falls back to the author's own handle even though
    // the reply text mentions nobody.
    let csrf = csrf_of(&get(&app, "/", Some(&alice_cookie)).await.body);
    let threaded = post_multipart(
        &app,
        "/web/compose",
        &alice_cookie,
        &[
            ("csrf", &csrf),
            ("status", "continuing my own thread"),
            ("visibility", "public"),
            ("in_reply_to_id", &parent_id),
        ],
    )
    .await;
    let self_reply = get(
        &app,
        &threaded.location.expect("redirect to the reply"),
        Some(&alice_cookie),
    )
    .await;
    assert!(self_reply.body.contains("Replying to @alice"));
}

/// The focused post of a thread page carries the detail treatment —
/// full timestamp, visibility / language / quote-policy labels and the
/// engagement summary — while plain timeline cards do not.
#[sqlx::test(migrations = "../db/migrations")]
async fn thread_focus_shows_detail_metadata(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "look at my detail view").await;

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("status__detail-meta"));
    // Visibility and quote policy are icon chips with their labels in titles.
    assert!(
        thread
            .body
            .contains(r#"class="status__vis status__detail-icon""#)
    );
    assert!(thread.body.contains(r#"title="Public""#));
    assert!(thread.body.contains(r#"data-detail-icon="vis-public""#));
    assert!(thread.body.contains(r#"title="Anyone can quote""#));
    assert!(thread.body.contains(r#"data-detail-icon="quote-any""#));
    // The detail timestamp is a `<time>`: viewer wall-clock in the body, the
    // UTC reading in the tooltip, RFC 3339 UTC in `datetime`. The offset is no
    // longer suffixed inline — see `web::clock`.
    let detail_time = thread
        .body
        .split_once("status__detail-meta")
        .expect("detail meta")
        .1;
    let detail_time = &detail_time[..detail_time.find("</span>").expect("detail time cell")];
    assert!(detail_time.contains("data-absolute"), "{detail_time}");
    assert!(detail_time.contains("UTC\""), "{detail_time}");
    // The counts live on the action bar, the lists behind the overflow menu's
    // engagement row.
    assert!(thread.body.contains("status__menu-row"));

    let home = get(&app, "/", Some(&cookie)).await;
    assert!(!home.body.contains("status__detail-meta"));
}

/// Quote attachments that aren't an embedded accepted card render an
/// explanatory placeholder instead of disappearing.
#[sqlx::test(migrations = "../db/migrations")]
async fn non_embedded_quote_states_render_placeholders(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let original = compose(&app, &cookie, "the original thought").await;
    let original_id = original.rsplit('/').next().unwrap().to_owned();

    let csrf = csrf_of(&get(&app, "/", Some(&cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "my hot take"),
            ("visibility", "public"),
            ("quoted_status_id", &original_id),
        ],
    )
    .await;
    let permalink = posted.location.expect("quoting post");

    sqlx::query!("UPDATE quotes SET state = 'pending'")
        .execute(&pool)
        .await
        .unwrap();
    let pending = get(&app, &permalink, Some(&cookie)).await;
    assert!(pending.body.contains("Quote pending approval"));
    assert!(!pending.body.contains("the original thought"));
    assert!(pending.body.contains("RE:"));
    assert!(pending.body.contains(r#"class="quote-inline""#));

    sqlx::query("UPDATE quotes SET state = 'rejected'")
        .execute(&pool)
        .await
        .unwrap();
    let rejected = get(&app, &permalink, Some(&cookie)).await;
    assert!(
        rejected
            .body
            .contains("The quote was removed by its author")
    );
    assert!(rejected.body.contains("RE:"));
    assert!(rejected.body.contains(r#"class="quote-inline""#));
}

/// The first-party UI reflects the same quote policy enforced by the posting
/// service: denied remote posts show a disabled quote glyph, have no composer
/// link, and a hand-written composer URL is refused.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_posts_without_quote_permission_disable_the_web_action(pool: PgPool) {
    seed_alice(&pool).await;
    let remote_bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let remote_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/no-quotes",
            account_id: stored_bob.id,
            content: "<p>you may read but not quote this</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: Some("https://remote.example/@bob/no-quotes"),
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let resolved = get(
        &app,
        &format!("/web/statuses/{}", remote_status.id),
        Some(&cookie),
    )
    .await;
    let thread = get(
        &app,
        &resolved.location.expect("thread redirect"),
        Some(&cookie),
    )
    .await;
    assert!(
        thread
            .body
            .contains("You aren't allowed to quote this post")
    );
    assert!(
        !thread
            .body
            .contains(&format!(r#"href="/compose?quote={}""#, remote_status.id))
    );
    assert!(
        thread.body.contains("M791-56 425-422"),
        "denied quote action should use Mastodon's format_quote_off glyph"
    );
    assert!(
        thread
            .body
            .contains(r#"transform="translate(0 24) scale(.025)""#),
        "the disabled quote glyph must remain inside the 24px viewport"
    );

    let compose = get(
        &app,
        &format!("/compose?quote={}", remote_status.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(compose.status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Rendered content keeps mention and hashtag anchors in-app (relative
/// `/@acct` and `/tags/{name}` hrefs). A signed-in viewer's other links route
/// through the in-app resolver (`/web/go`) so a click can land on our copy of a
/// federated actor/post; an anonymous viewer, who can't resolve, gets the plain
/// external link. Either way the link opens in a new tab.
#[sqlx::test(migrations = "../db/migrations")]
async fn content_links_stay_in_app_and_external_links_open_new_tabs(pool: PgPool) {
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(
        &app,
        &cookie,
        "@bob about #rust see https://news.example/story",
    )
    .await;

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains(r#"href="/@bob""#), "{}", thread.body);
    assert!(thread.body.contains(r#"href="/tags/rust""#));
    // Signed in: the plain link routes through the resolver, still in a new tab.
    assert!(
        thread
            .body
            .contains(r#"href="/web/go?url=https%3A%2F%2Fnews.example%2Fstory""#),
        "{}",
        thread.body
    );
    assert!(
        thread
            .body
            .contains(r#"rel="nofollow noopener" translate="no" target="_blank""#),
        "{}",
        thread.body
    );

    // Anonymous: nothing to resolve, so the plain link stays external; mentions
    // and hashtags are still in-app.
    let anon = get(&app, &permalink, None).await;
    assert!(
        anon.body.contains(r#"href="https://news.example/story""#),
        "{}",
        anon.body
    );
    assert!(!anon.body.contains("/web/go"), "{}", anon.body);
    assert!(anon.body.contains(r#"href="/@bob""#));
    assert!(anon.body.contains(r#"href="/tags/rust""#));
}

/// Remote sanitised HTML carries `rel` (ammonia) but no `target`; the
/// renderer adds one so external links consistently open in a new tab. For a
/// signed-in viewer the link also routes through the in-app resolver.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_content_links_gain_a_new_tab_target(pool: PgPool) {
    seed_alice(&pool).await;
    let remote_bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let remote_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus { title: None, object_type: None, external_url: None,
            uri: "https://remote.example/users/bob/statuses/9",
            account_id: stored_bob.id,
            content: r#"<p><a href="https://elsewhere.example/read" rel="nofollow noopener noreferrer">read this</a></p>"#,
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: Some("https://remote.example/@bob/9"),
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let resolved = get(
        &app,
        &format!("/web/statuses/{}", remote_status.id),
        Some(&cookie),
    )
    .await;
    let thread = get(
        &app,
        &resolved.location.expect("thread redirect"),
        Some(&cookie),
    )
    .await;
    assert!(
        thread.body.contains(
            r#"href="/web/go?url=https%3A%2F%2Felsewhere.example%2Fread" rel="nofollow noopener noreferrer" target="_blank""#
        ),
        "{}",
        thread.body
    );
}

/// The in-app link resolver (`GET /web/go`): a signed-in viewer's click on a
/// federated URL lands on our local copy. An anonymous viewer gets the
/// storage-only lookup: a known object still opens locally, an unknown
/// one falls through to the original page without any fetch.
#[sqlx::test(migrations = "../db/migrations")]
async fn go_resolves_federated_urls_and_falls_through_otherwise(pool: PgPool) {
    seed_alice(&pool).await;
    let mut group = RemoteUser::new("lemmy.test", "memes");
    group.actor.kind = "Group".to_owned();
    group.actor.name = Some("Memes".to_owned());
    group.actor.id = "https://lemmy.test/c/memes".to_owned();
    group.actor.inbox = "https://lemmy.test/c/memes/inbox".to_owned();
    group.actor.public_key.id = "https://lemmy.test/c/memes#main-key".to_owned();
    group.actor.public_key.owner = "https://lemmy.test/c/memes".to_owned();
    remote::store_remote_actor(&pool, &group.actor)
        .await
        .unwrap();

    let app = common::test_app(pool);
    let cookie = login(&app).await;

    // The stored group's AP id resolves to our local `/!` community profile.
    let hit = get(
        &app,
        "/web/go?url=https%3A%2F%2Flemmy.test%2Fc%2Fmemes",
        Some(&cookie),
    )
    .await;
    assert_eq!(hit.status, StatusCode::SEE_OTHER);
    assert_eq!(hit.location.as_deref(), Some("/!memes@lemmy.test"));

    // A URL that names no federatable object falls through to the original.
    let miss = get(
        &app,
        "/web/go?url=https%3A%2F%2Fplamenu.test%2Fnope",
        Some(&cookie),
    )
    .await;
    assert_eq!(miss.status, StatusCode::SEE_OTHER);
    assert_eq!(miss.location.as_deref(), Some("https://plamenu.test/nope"));

    // An anonymous viewer's click on a KNOWN object opens the local view too
    // served from storage, no fetch on their behalf.
    let anon = get(
        &app,
        "/web/go?url=https%3A%2F%2Flemmy.test%2Fc%2Fmemes",
        None,
    )
    .await;
    assert_eq!(anon.status, StatusCode::SEE_OTHER);
    assert_eq!(anon.location.as_deref(), Some("/!memes@lemmy.test"));

    // An unknown URL falls through to the original for an anonymous viewer —
    // resolution fetches stay signed-in-only.
    let anon_miss = get(
        &app,
        "/web/go?url=https%3A%2F%2Flemmy.test%2Fc%2Funknown",
        None,
    )
    .await;
    assert_eq!(anon_miss.status, StatusCode::SEE_OTHER);
    assert_eq!(
        anon_miss.location.as_deref(),
        Some("https://lemmy.test/c/unknown")
    );

    // Non-http(s) schemes are rejected outright.
    let bad = get(&app, "/web/go?url=mailto%3Ax%40y.z", Some(&cookie)).await;
    assert_eq!(bad.status, StatusCode::NOT_FOUND);
}

/// A status' preview card renders under the content — provider, title,
/// description and thumbnail, the whole card one external link.
#[sqlx::test(migrations = "../db/migrations")]
async fn preview_card_renders_under_the_content(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "read https://news.example/story").await;
    let status_id: i64 = permalink.rsplit('/').next().unwrap().parse().unwrap();

    // Attach a crawled card directly; the crawler itself is covered by the
    // preview_cards suite.
    let card = preview_card::upsert(
        &pool,
        preview_card::NewPreviewCard {
            url: "https://news.example/story",
            title: "A big story",
            description: "Everything that happened, at length.",
            kind: "link",
            author_name: "",
            author_url: "",
            provider_name: "News Example",
            provider_url: "https://news.example",
            html: "",
            width: 640,
            height: 480,
            image_url: Some("https://news.example/thumb.jpg"),
            image_description: "the story's cover",
            embed_url: "",
            language: None,
            published_at: None,
            author_account_id: None,
        },
    )
    .await
    .unwrap();
    preview_card::attach(&pool, status_id, card.id, "https://news.example/story")
        .await
        .unwrap();

    for uri in [permalink.as_str(), "/"] {
        let page = get(&app, uri, Some(&cookie)).await;
        assert!(page.body.contains("preview-card"), "GET {uri}");
        assert!(page.body.contains("A big story"), "GET {uri}");
        assert!(page.body.contains("News Example"), "GET {uri}");
        assert!(
            page.body.contains("Everything that happened, at length."),
            "GET {uri}"
        );
        // The card image is proxied through the instance, never hot-linked.
        assert!(
            page.body
                .contains(&format!("/media/proxy/card/{}", card.id)),
            "GET {uri}"
        );
        assert!(
            !page.body.contains("news.example/thumb.jpg"),
            "card image leaks origin on GET {uri}"
        );
    }
}

// ---- Settings -----------------------------------------------------------

/// POSTs a `multipart/form-data` body of text fields (the encoding the profile
/// editor uses, since it also carries the avatar/header uploads).
async fn post_multipart(app: &Router, uri: &str, cookie: &str, fields: &[(&str, &str)]) -> Resp {
    use std::fmt::Write as _;
    const BOUNDARY: &str = "PLAMENUTESTBOUNDARY";
    let mut body = String::new();
    for (name, value) in fields {
        write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .unwrap();
    }
    write!(body, "--{BOUNDARY}--\r\n").unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

/// POSTs a multipart form with text fields plus one binary file field.
async fn post_multipart_file(
    app: &Router,
    uri: &str,
    cookie: &str,
    fields: &[(&str, &str)],
    file: (&str, &str, &str, &[u8]),
) -> Resp {
    use std::io::Write as _;
    const BOUNDARY: &str = "PLAMENUFILEBOUNDARY";
    let mut body = Vec::new();
    for (name, value) in fields {
        write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .unwrap();
    }
    let (name, filename, content_type, bytes) = file;
    write!(
        body,
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .unwrap();
    body.extend_from_slice(bytes);
    write!(body, "\r\n--{BOUNDARY}--\r\n").unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

fn sample_png_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        32,
        32,
        image::Rgb([160, 90, 220]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}

/// A minimal 8kHz mono 16-bit PCM WAV (~0.25s square wave), handcrafted so no
/// external tool is needed to build the fixture; the server still transcodes
/// it with ffmpeg on upload.
fn sample_wav_bytes() -> Vec<u8> {
    let sample_rate = 8000u32;
    let samples: Vec<i16> = (0..2000)
        .map(|i| if (i / 9) % 2 == 0 { 8000 } else { -8000 })
        .collect();
    let data_len = u32::try_from(samples.len() * 2).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// A one-second soundless 64x64 H.264 mp4 — classified as `gifv` on upload.
/// Generated with ffmpeg like the fixtures in `media.rs`; the media pipeline
/// requires ffmpeg anyway, so the tests may assume it too.
fn soundless_mp4_bytes() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("fixture.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-loglevel",
            "fatal",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=10",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-y",
        ])
        .arg(&out)
        .status()
        .expect("ffmpeg must be installed to run the media tests");
    assert!(status.success(), "ffmpeg fixture generation failed");
    std::fs::read(&out).unwrap()
}

/// A `side`×`side` PNG of pseudo-random pixels — noise won't PNG-compress away,
/// so the encoded bytes stay large (unlike a solid colour), which is what we
/// need to exceed a body-size limit in a test.
fn noise_png_bytes(side: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(side, side);
    let mut seed: u32 = 0x1234_5678;
    let mut next = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed.to_le_bytes()[2]
    };
    for px in img.pixels_mut() {
        *px = image::Rgb([next(), next(), next()]);
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_index_redirects_to_profile(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let resp = get(&app, "/settings", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(resp.location.as_deref(), Some("/settings/profile"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_profile_form_renders(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let resp = get(&app, "/settings/profile", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("Edit profile"));
    assert!(resp.body.contains("name=\"display_name\""));
    assert!(resp.body.contains("/settings/preferences"));
    assert!(resp.body.contains("name=\"bot\""));
    assert!(resp.body.contains("name=\"avatar_description\""));
    assert!(resp.body.contains("name=\"header_description\""));
    assert!(!resp.body.contains("WIP"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_profile_form_uses_stored_russian_locale(pool: PgPool) {
    let account = seed_alice(&pool).await;
    let stored_user = user::find_by_account_id(&pool, account.id)
        .await
        .unwrap()
        .unwrap();
    user::update_locale(&pool, stored_user.id, Some("ru"))
        .await
        .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let resp = get(&app, "/settings/profile?saved=1", Some(&cookie)).await;

    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(
        resp.body
            .contains("<title>Редактирование профиля — Plamenu</title>")
    );
    assert!(resp.body.contains("Разделы настроек"));
    assert!(resp.body.contains("Конфиденциальность и охват"));
    assert!(resp.body.contains("Отображаемое имя"));
    assert!(resp.body.contains("Метаданные профиля"));
    assert!(resp.body.contains("Подтверждать запросы на подписку"));
    assert!(resp.body.contains("Сохранить изменения"));
    assert!(
        resp.body
            .contains("Профиль сохранён, изменения отправлены вашим подписчикам.")
    );
    assert!(!resp.body.contains(">Edit profile<"));
    assert!(!resp.body.contains(">Save changes<"));
    assert!(!resp.body.contains(">Preferences<"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn core_settings_pages_use_stored_russian_locale(pool: PgPool) {
    let account = seed_alice(&pool).await;
    let stored_user = user::find_by_account_id(&pool, account.id)
        .await
        .unwrap()
        .unwrap();
    user::update_locale(&pool, stored_user.id, Some("ru"))
        .await
        .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    for (path, expected, absent) in [
        (
            "/settings/preferences?saved=1",
            "Настройки публикации",
            ">Posting defaults<",
        ),
        (
            "/settings/languages?saved=1",
            "Языки, на которых вы публикуете",
            ">Languages you post in<",
        ),
        (
            "/settings/privacy?saved=1",
            "Обнаружение аккаунта",
            ">Discoverability<",
        ),
        (
            "/settings/account?error=current_password",
            "Неверный текущий пароль",
            "The current password was incorrect",
        ),
        (
            "/settings/push",
            "Уведомлять меня о следующем",
            ">Notify me about<",
        ),
    ] {
        let response = get(&app, path, Some(&cookie)).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert!(
            response.body.contains(r#"<html lang="ru" dir="ltr""#),
            "{path}"
        );
        assert!(response.body.contains(expected), "{path}: {expected}");
        assert!(!response.body.contains(absent), "{path}: {absent}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_profile_update_persists(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/settings/profile", Some(&cookie)).await.body);

    let saved = post_multipart(
        &app,
        "/web/settings/profile",
        &cookie,
        &[
            ("csrf", &csrf),
            ("display_name", "Alice Liddell"),
            ("note", "down the rabbit hole"),
            // Hidden-false + checkbox-true: a checked "require follow requests".
            ("locked", "false"),
            ("locked", "true"),
            ("bot", "false"),
            ("bot", "true"),
            ("avatar_description", "me, smiling"),
            ("header_description", "a rabbit hole"),
            ("fields_attributes[0][name]", "Website"),
            ("fields_attributes[0][value]", "https://example.com/"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    assert_eq!(saved.location.as_deref(), Some("/settings/profile?saved=1"));

    // The editor now reflects the saved values and the profile shows the bio.
    let form = get(&app, "/settings/profile?saved=1", Some(&cookie)).await;
    assert!(form.body.contains("Alice Liddell"));
    assert!(form.body.contains("down the rabbit hole"));
    assert!(form.body.contains("Website"));
    let profile = get(&app, "/@alice", Some(&cookie)).await;
    assert!(profile.body.contains("Alice Liddell"));
    assert!(profile.body.contains("down the rabbit hole"));
    // The profile surfaces the bot/locked badges and the metadata field.
    assert!(
        profile.body.contains("profile__badge--bot"),
        "bot badge: {}",
        profile.body
    );
    assert!(
        profile.body.contains("profile__badge--locked"),
        "lock badge: {}",
        profile.body
    );
    assert!(
        profile.body.contains("profile__field"),
        "metadata field: {}",
        profile.body
    );
    assert!(profile.body.contains("Website"));
    assert!(profile.body.contains("example.com"));
    let stored = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert!(stored.locked);
    assert!(stored.is_bot);
    assert_eq!(stored.avatar_description, "me, smiling");
    assert_eq!(stored.header_description, "a rabbit hole");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_profile_update_rejects_bad_csrf(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let resp = post_multipart(
        &app,
        "/web/settings/profile",
        &cookie,
        &[("csrf", "not-the-real-token"), ("display_name", "Mallory")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_live_sections_render(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    for (uri, marker) in [
        ("/settings/preferences", "Save preferences"),
        ("/settings/languages", "Save languages"),
        ("/settings/privacy", "Save privacy"),
        ("/settings/account", "Change email"),
        ("/settings/security", "Change password"),
    ] {
        let resp = get(&app, uri, Some(&cookie)).await;
        assert_eq!(resp.status, StatusCode::OK, "{uri} should render");
        assert!(resp.body.contains(marker), "{uri} should be live");
        assert!(resp.body.contains(r#"data-live-notifications="false""#));
        if uri == "/settings/privacy" {
            assert!(resp.body.contains("Unsolicited private mentions"));
            assert!(resp.body.contains(r#"name="private_mentions_policy""#));
            assert!(resp.body.contains(r#"value="accept" selected"#));
        }
        assert!(!resp.body.contains("WIP"), "{uri} should not be marked WIP");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_preferences_save_and_compose_defaults(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let alice_user = user::find_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    plamenu_db::web_setting::upsert(
        &pool,
        alice_user.id,
        &serde_json::json!({ "theme": "mastodon-light" }),
    )
    .await
    .unwrap();
    let csrf = csrf_of(&get(&app, "/settings/preferences", Some(&cookie)).await.body);

    let saved = post_form(
        &app,
        "/web/settings/preferences",
        &cookie,
        &[
            ("csrf", &csrf),
            ("posting_default_visibility", "unlisted"),
            ("posting_default_sensitive", "false"),
            ("posting_default_sensitive", "true"),
            ("reading_expand_media", "show_all"),
            ("reading_expand_spoilers", "false"),
            ("reading_expand_spoilers", "true"),
            ("reading_autoplay_gifs", "false"),
            ("reading_autoplay_gifs", "true"),
            ("timeline_order", "received"),
            ("thread_order", "flat"),
            ("live_notifications", "false"),
            ("live_notifications", "true"),
            ("notification_sound", "false"),
            ("notification_volume", "35"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    assert_eq!(
        saved.location.as_deref(),
        Some("/settings/preferences?saved=1")
    );

    // The posting language lives on the Languages page, not this one.
    let lcsrf = csrf_of(&get(&app, "/settings/languages", Some(&cookie)).await.body);
    let saved = post_form(
        &app,
        "/web/settings/languages",
        &cookie,
        &[("csrf", &lcsrf), ("posting_default_language", "fr")],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);

    let settings = user::settings_by_account_id(
        &pool,
        account::find_local_by_username(&pool, "alice")
            .await
            .unwrap()
            .unwrap()
            .id,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(settings.resolved_visibility(false), "unlisted");
    assert_eq!(settings.default_language(), Some("fr"));
    assert!(settings.posting_default_sensitive);
    assert_eq!(settings.timeline_order, user::TimelineOrder::Received);
    assert_eq!(settings.thread_order, user::ThreadOrder::Flat);
    let notifications = plamenu_db::web_setting::notification_preferences(&pool, alice_user.id)
        .await
        .unwrap();
    assert!(notifications.live_updates);
    assert!(!notifications.sound);
    assert_eq!(notifications.volume, 35);
    let raw = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT data FROM web_settings WHERE user_id = $1",
    )
    .bind(alice_user.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(raw["theme"], "mastodon-light", "other web settings survive");

    let home = get(&app, "/", Some(&cookie)).await;
    assert!(home.body.contains(r#"data-live-notifications="true""#));
    assert!(home.body.contains(r#"data-notification-sound="false""#));
    assert!(home.body.contains(r#"data-notification-volume="35""#));
    let csrf = csrf_of(&home.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[("csrf", &csrf), ("status", "bonjour defaults")],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let status_id = posted
        .location
        .as_deref()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let stored = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert_eq!(stored.visibility, "unlisted");
    assert!(stored.sensitive);
    assert_eq!(stored.language.as_deref(), Some("fr"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_privacy_save_persists_flags(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/settings/privacy", Some(&cookie)).await.body);

    let saved = post_form(
        &app,
        "/web/settings/privacy",
        &cookie,
        &[
            ("csrf", &csrf),
            ("discoverable", "false"),
            ("discoverable", "true"),
            ("indexable", "false"),
            ("indexable", "true"),
            ("hide_collections", "false"),
            ("hide_collections", "true"),
            ("private_mentions_policy", "drop"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    assert_eq!(saved.location.as_deref(), Some("/settings/privacy?saved=1"));

    let stored = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.discoverable, Some(true));
    assert!(stored.indexable);
    assert!(stored.hide_collections);

    let policy = notification_policy::get_or_default(&pool, stored.id)
        .await
        .unwrap();
    assert_eq!(
        policy.for_private_mentions,
        notification_policy::Disposition::Drop
    );
    let form = get(&app, "/settings/privacy?saved=1", Some(&cookie)).await;
    assert!(form.body.contains(r#"value="drop" selected"#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_security_password_change_reauthenticates(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/settings/security", Some(&cookie)).await.body);

    let wrong = post_form(
        &app,
        "/web/settings/security/password",
        &cookie,
        &[
            ("csrf", &csrf),
            ("current_password", "wrong"),
            ("new_password", "new secret"),
            ("confirm_password", "new secret"),
        ],
    )
    .await;
    assert_eq!(wrong.status, StatusCode::SEE_OTHER);
    assert_eq!(
        wrong.location.as_deref(),
        Some("/settings/security?error=current_password")
    );

    let changed = post_form(
        &app,
        "/web/settings/security/password",
        &cookie,
        &[
            ("csrf", &csrf),
            ("current_password", PASSWORD),
            ("new_password", "new secret"),
            ("confirm_password", "new secret"),
        ],
    )
    .await;
    assert_eq!(changed.status, StatusCode::SEE_OTHER);
    assert_eq!(
        changed.location.as_deref(),
        Some("/settings/security?saved=password")
    );
    let stored = user::find_by_email(&pool, EMAIL).await.unwrap().unwrap();
    assert!(verify_password("new secret", &stored.password_hash));
    assert!(!verify_password(PASSWORD, &stored.password_hash));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_account_email_change_handles_conflict(pool: PgPool) {
    seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", "bob-password").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/settings/account", Some(&cookie)).await.body);

    let conflict = post_form(
        &app,
        "/web/settings/account/email",
        &cookie,
        &[
            ("csrf", &csrf),
            ("email", "bob@example.com"),
            ("current_password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(conflict.status, StatusCode::SEE_OTHER);
    assert_eq!(
        conflict.location.as_deref(),
        Some("/settings/account?error=email_taken")
    );

    let saved = post_form(
        &app,
        "/web/settings/account/email",
        &cookie,
        &[
            ("csrf", &csrf),
            ("email", "alice-new@example.com"),
            ("current_password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    assert_eq!(
        saved.location.as_deref(),
        Some("/settings/account?saved=email")
    );
    assert!(user::find_by_email(&pool, EMAIL).await.unwrap().is_none());
    assert!(
        user::find_by_email(&pool, "alice-new@example.com")
            .await
            .unwrap()
            .is_some()
    );

    // An empty submission removes the address entirely; the username keeps
    // signing in.
    let removed = post_form(
        &app,
        "/web/settings/account/email",
        &cookie,
        &[
            ("csrf", &csrf),
            ("email", ""),
            ("current_password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    assert_eq!(
        removed.location.as_deref(),
        Some("/settings/account?saved=email_removed")
    );
    assert!(
        user::find_by_email(&pool, "alice-new@example.com")
            .await
            .unwrap()
            .is_none()
    );
    let relogin = post_login(&app, "alice", PASSWORD).await;
    assert_eq!(relogin.status, StatusCode::SEE_OTHER);
    assert!(relogin.set_cookie.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_account_delete_tombstones_and_clears_cookie(pool: PgPool) {
    seed_alice(&pool).await;
    let remote_bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([remote_bob.actor.clone()]);
    let stored_bob = remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    follow::create(&pool, stored_bob.id, alice.id, None)
        .await
        .unwrap();
    let state = common::test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let cookie = login(&app).await;
    let status_path = compose(&app, &cookie, "remove my account").await;
    let status_id = status_path
        .rsplit('/')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let csrf = csrf_of(&get(&app, "/settings/account", Some(&cookie)).await.body);

    let wrong_confirm = post_form(
        &app,
        "/web/settings/account/delete",
        &cookie,
        &[
            ("csrf", &csrf),
            ("confirm_handle", "alice"),
            ("current_password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(wrong_confirm.status, StatusCode::SEE_OTHER);
    assert_eq!(
        wrong_confirm.location.as_deref(),
        Some("/settings/account?error=delete_confirm")
    );

    let deleted = post_form(
        &app,
        "/web/settings/account/delete",
        &cookie,
        &[
            ("csrf", &csrf),
            ("confirm_handle", "@alice"),
            ("current_password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert_eq!(deleted.location.as_deref(), Some("/"));
    assert!(
        deleted
            .set_cookie
            .as_deref()
            .is_some_and(|cookie| cookie.contains("Max-Age=0"))
    );

    // The Delete(Actor) goes through the delivery queue to the follower
    // (alongside the earlier status's Create).
    assert_eq!(plamenu::delivery::run_due(&state).await, 2);
    let deliveries = stub.deliveries();
    let delete = deliveries
        .iter()
        .find(|d| d.activity["type"] == "Delete")
        .expect("Delete(Actor) delivered");
    assert_eq!(delete.inbox_url, "https://remote.example/inbox");
    assert_eq!(
        delete.activity["object"]["id"],
        "https://plamenu.test/users/alice"
    );

    // Mastodon semantics: the username stays reserved on a suspended
    // tombstone row; the login and content are gone.
    let tombstone = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .expect("account row reserved");
    assert!(tombstone.suspended());
    assert_eq!(tombstone.suspension_origin.as_deref(), Some("local"));
    assert_eq!(tombstone.display_name, "");
    assert!(user::find_by_email(&pool, EMAIL).await.unwrap().is_none());
    assert!(
        status::find_by_id(&pool, status_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        follow::follower_inboxes(&pool, tombstone.id)
            .await
            .unwrap()
            .is_empty()
    );

    // The session cookie is dead and a fresh login is impossible.
    let after = get(&app, "/settings/account", Some(&cookie)).await;
    assert_ne!(after.status, StatusCode::OK);
    let relogin = post_login(&app, EMAIL, PASSWORD).await;
    assert_eq!(relogin.status, StatusCode::UNAUTHORIZED);

    // The AP actor and its web profile answer 410 Gone.
    let actor = get(&app, "/users/alice", None).await;
    assert_eq!(actor.status, StatusCode::GONE);
}

// ---- Admin dashboard -----------------------------------------------------

/// The seeded Owner role (migration 0052) — every admin permission.
const OWNER_ROLE_ID: i64 = 3;

/// Grants `account` the Owner role so it can reach the admin dashboard.
async fn make_staff(pool: &PgPool, account_id: i64) {
    role::assign_to_account(pool, account_id, Some(OWNER_ROLE_ID))
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_pages_require_a_role(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;

    // A signed-in non-moderator is forbidden from every admin page.
    for uri in [
        "/admin",
        "/admin/accounts",
        "/admin/accounts/1",
        "/admin/reports",
        "/admin/reports/1",
        "/admin/audit-log",
    ] {
        let resp = get(&app, uri, Some(&cookie)).await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "GET {uri}");
    }
    // ...and never sees the admin link in the chrome.
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(!home.body.contains("href=\"/admin\""));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn federation_debug_probes_past_the_failure_budget(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let stub = std::sync::Arc::new(StubFederation::default());
    *stub.suppress_object_fetches.lock().unwrap() = true;
    let app = common::test_app_with(pool, stub.clone());
    let cookie = login(&app).await;
    let probe_uri = "/admin/federation-debug?url=https%3A%2F%2Fremote.example%2Fnote%2F1";

    // Suppressed target, and the probe past the budget also fails: the page
    // shows the suppression AND the underlying error, not just the refusal.
    let page = get(&app, probe_uri, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains("suppressed by the finite failure budget")
    );
    assert!(page.body.contains("remote answered 404"));

    // Once the origin serves the object, the probe succeeds and reports the
    // suppression as cleared.
    stub.objects.lock().unwrap().insert(
        "https://remote.example/note/1".to_owned(),
        serde_json::json!({"id": "https://remote.example/note/1", "type": "Note"}),
    );
    let page = get(&app, probe_uri, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("suppression is now cleared"));
    assert!(page.body.contains("Fetched"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_link_appears_for_staff(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let home = get(&app, "/", Some(&cookie)).await;
    assert!(home.body.contains("href=\"/admin\""));
}

/// Contextual shield menus follow the destination pages' exact permissions,
/// rather than the coarse fact that the viewer has some staff role. The role
/// is resolved on every request, so a change takes effect without signing out.
#[sqlx::test(migrations = "../db/migrations")]
async fn contextual_admin_links_follow_exact_permissions(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let remote_bob = RemoteUser::new("remote.example", "robert");
    let stored_remote = remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();

    // The built-in Moderator has MANAGE_USERS but not MANAGE_FEDERATION.
    role::assign_to_account(&pool, alice.id, Some(1))
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let bob_cookie = login_as(&app, "bob@example.com", PASSWORD).await;
    let permalink = compose(&app, &bob_cookie, "staff shortcut target").await;
    let cookie = login(&app).await;

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("data-privileged-menu"));
    assert!(thread.body.contains(&format!("/admin/accounts/{}", bob.id)));

    let remote_profile = get(&app, "/@robert@remote.example", Some(&cookie)).await;
    assert!(
        remote_profile
            .body
            .contains(&format!("/admin/accounts/{}", stored_remote.id))
    );
    assert!(
        !remote_profile
            .body
            .contains("/admin/instances/remote.example")
    );

    // A federation-only role gets the inverse set on the very next request.
    let federation = role::create(
        &pool,
        "Federation navigator",
        "",
        5,
        plamenu_db::role::permission::MANAGE_FEDERATION,
        false,
    )
    .await
    .unwrap();
    role::assign_to_account(&pool, alice.id, Some(federation.id))
        .await
        .unwrap();
    let remote_profile = get(&app, "/@robert@remote.example", Some(&cookie)).await;
    assert!(remote_profile.body.contains("data-privileged-menu"));
    assert!(
        remote_profile
            .body
            .contains("/admin/instances/remote.example")
    );
    assert!(
        !remote_profile
            .body
            .contains(&format!("/admin/accounts/{}", stored_remote.id))
    );
    let local_profile = get(&app, "/@bob", Some(&cookie)).await;
    assert!(!local_profile.body.contains("data-privileged-menu"));
    assert!(
        !local_profile
            .body
            .contains(&format!("/admin/accounts/{}", bob.id))
    );

    // An unrelated staff permission never produces an empty shield.
    let reports = role::create(
        &pool,
        "Reports only",
        "",
        5,
        plamenu_db::role::permission::MANAGE_REPORTS,
        false,
    )
    .await
    .unwrap();
    role::assign_to_account(&pool, alice.id, Some(reports.id))
        .await
        .unwrap();
    let remote_profile = get(&app, "/@robert@remote.example", Some(&cookie)).await;
    assert!(!remote_profile.body.contains("data-privileged-menu"));
}

/// Emoji managers can extract the source-specific emoji set from either a
/// profile or a post, review advertised/fallback categories, and turn a
/// selected remote row into an independent local upload.
#[sqlx::test(migrations = "../db/migrations")]
async fn contextual_custom_emoji_borrow_flow(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let emoji_role = role::create(
        &pool,
        "Emoji curator",
        "",
        5,
        plamenu_db::role::permission::MANAGE_CUSTOM_EMOJIS,
        false,
    )
    .await
    .unwrap();
    role::assign_to_account(&pool, alice.id, Some(emoji_role.id))
        .await
        .unwrap();

    let mut remote_bob = RemoteUser::new("remote.example", "robert");
    remote_bob.actor.name = Some("Robert :profilewave:".to_owned());
    remote_bob.actor.summary = Some("<p>Bio :bioblob:</p>".to_owned());
    remote_bob.actor.attachment = vec![serde_json::json!({
        "type": "PropertyValue",
        "name": "Mood :fieldstar:",
        "value": "Bright :fieldshine:",
    })];
    let stored_bob = remote::store_remote_actor(&pool, &remote_bob.actor)
        .await
        .unwrap();
    let emoji_url = |shortcode: &str| format!("https://remote.example/emoji/{shortcode}.png");
    for shortcode in [
        "profilewave",
        "bioblob",
        "fieldstar",
        "fieldshine",
        "postparty",
    ] {
        let image = emoji_url(shortcode);
        custom_emoji::upsert_remote(
            &pool,
            custom_emoji::RemoteEmojiData {
                shortcode,
                domain: "remote.example",
                uri: None,
                image_remote_url: &image,
                updated: None,
            },
        )
        .await
        .unwrap();
    }
    let post = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            uri: "https://remote.example/users/robert/statuses/7",
            account_id: stored_bob.id,
            content: "<p>A post :postparty:</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: Some("https://remote.example/@robert/7"),
            quote_approval_policy: 0,
            title: None,
            object_type: None,
            external_url: None,
        },
    )
    .await
    .unwrap();
    let reaction_url = "https://reactions.example/emoji/reactparty.png";
    custom_emoji::upsert_remote(
        &pool,
        custom_emoji::RemoteEmojiData {
            shortcode: "reactparty",
            domain: "reactions.example",
            uri: None,
            image_remote_url: reaction_url,
            updated: None,
        },
    )
    .await
    .unwrap();
    reaction::create(
        &pool,
        reaction::NewReaction {
            account_id: alice.id,
            status_id: post.id,
            name: "reactparty",
            custom_emoji_url: Some(reaction_url),
            uri: None,
        },
    )
    .await
    .unwrap();

    let stub = std::sync::Arc::new(StubFederation::default());
    stub.serve_page_as(
        "https://remote.example/api/v1/custom_emojis",
        "https://remote.example/api/v1/custom_emojis",
        "application/json",
        &serde_json::json!([
            {"shortcode": "profilewave", "category": "Profiles"},
            {"shortcode": "bioblob", "category": null},
            {"shortcode": "fieldstar", "category": "Profile fields"},
            {"shortcode": "fieldshine", "category": "Profile fields"},
            {"shortcode": "postparty", "category": "Celebration"},
        ])
        .to_string(),
    );
    stub.serve_page_as(
        "https://reactions.example/api/v1/custom_emojis",
        "https://reactions.example/api/v1/custom_emojis",
        "application/json",
        &serde_json::json!([
            {"shortcode": "reactparty", "category": "Reactions"},
        ])
        .to_string(),
    );
    stub.serve_media(&emoji_url("postparty"), "image/png", sample_png_bytes());
    let app = common::test_app_with(pool.clone(), stub.clone());
    let cookie = login(&app).await;

    let profile = get(&app, "/@robert@remote.example", Some(&cookie)).await;
    assert!(profile.body.contains("data-privileged-menu"));
    assert!(
        profile.body.contains(&format!(
            "/admin/custom-emojis/borrow/account/{}",
            stored_bob.id
        )),
        "{}",
        profile.body
    );
    assert!(
        !profile
            .body
            .contains(&format!("/admin/accounts/{}", stored_bob.id))
    );

    let thread = get(
        &app,
        &format!("/@robert@remote.example/{}", post.id),
        Some(&cookie),
    )
    .await;
    assert!(
        thread
            .body
            .contains(&format!("/admin/custom-emojis/borrow/status/{}", post.id))
    );

    let profile_borrow = get(
        &app,
        &format!("/admin/custom-emojis/borrow/account/{}", stored_bob.id),
        Some(&cookie),
    )
    .await;
    for shortcode in ["profilewave", "bioblob", "fieldstar", "fieldshine"] {
        assert!(profile_borrow.body.contains(&format!(":{shortcode}:")));
    }
    assert!(!profile_borrow.body.contains(":postparty:"));
    assert!(profile_borrow.body.contains(r#"value="Profiles""#));
    assert!(profile_borrow.body.contains(r#"value="remote.example""#));

    let post_borrow = get(
        &app,
        &format!("/admin/custom-emojis/borrow/status/{}", post.id),
        Some(&cookie),
    )
    .await;
    assert!(post_borrow.body.contains(":postparty:"));
    assert!(post_borrow.body.contains(":reactparty:"));
    assert!(!post_borrow.body.contains(":profilewave:"));
    assert!(post_borrow.body.contains(r#"value="Celebration""#));
    assert!(post_borrow.body.contains(r#"value="Reactions""#));
    let csrf = csrf_of(&post_borrow.body);
    let remote_emoji =
        custom_emoji::lookup(&pool, &["postparty".to_owned()], Some("remote.example"))
            .await
            .unwrap()
            .remove(0);
    let emoji_id = remote_emoji.id.to_string();
    let category_key = format!("category_{}", remote_emoji.id);
    let imported = post_form(
        &app,
        "/web/admin/custom-emojis/borrow",
        &cookie,
        &[
            ("csrf", csrf.as_str()),
            ("emoji_id", emoji_id.as_str()),
            (category_key.as_str(), "Celebration"),
        ],
    )
    .await;
    assert_eq!(imported.status, StatusCode::SEE_OTHER);
    assert_eq!(
        imported.location.as_deref(),
        Some("/admin/custom-emojis?borrowed=1&skipped=0&failed=0")
    );
    let local = custom_emoji::lookup(&pool, &["postparty".to_owned()], None)
        .await
        .unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].category.as_deref(), Some("Celebration"));
    assert_eq!(stub.media_fetches(), vec![emoji_url("postparty")]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_overview_shows_stats(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let overview = get(&app, "/admin", Some(&cookie)).await;
    assert_eq!(overview.status, StatusCode::OK);
    // The overview is at-a-glance figures, not a duplicate of the section nav.
    assert!(overview.body.contains("Local accounts"));
    assert!(overview.body.contains("Open reports"));
    assert!(overview.body.contains("admin-stat"));
    // The Local-accounts card links into the filtered moderation list (the
    // trailing quote pins it to that card, not the pending one below).
    assert!(overview.body.contains("/admin/accounts?origin=local\""));
    // Nothing is awaiting review, so the pending-review card is absent.
    assert!(!overview.body.contains("status=pending"));
    assert!(!overview.body.contains("Pending review"));
    // The measures render as trailing-30-day tiles with dimensions.
    assert!(overview.body.contains("Last 30 days"));
    assert!(overview.body.contains("New users"));
    assert!(overview.body.contains("Active users"));
    assert!(overview.body.contains("Reports opened"));
    assert!(overview.body.contains("Sign-up sources"));
    assert!(overview.body.contains("Top languages"));
    // A1: the dimensions/retention/space/version surfaces of the metrics API.
    assert!(overview.body.contains("Most active servers"));
    assert!(overview.body.contains("Retention by sign-up month"));
    assert!(overview.body.contains("Database size"));
    assert!(overview.body.contains("Media storage"));
    assert!(overview.body.contains("PostgreSQL"));
    assert!(overview.body.contains("Build"));
    assert!(overview.body.contains("Revision"));
    assert!(overview.body.contains("Architecture"));
    assert!(overview.body.contains(plamenu::BUILD_INFO.target));
    // O1/O2: the delivery roll-up, linking into the instances detail tables.
    assert!(overview.body.contains("Federation delivery"));
    assert!(overview.body.contains("Queued deliveries"));
    assert!(overview.body.contains("Unreachable hosts"));
    assert!(overview.body.contains("/admin/instances#delivery-health"));
}

/// With an account awaiting review, the overview grows a "Pending review"
/// card that links straight to the local+pending moderation filter.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_overview_links_pending_review(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    // A confirmed-but-unapproved local sign-up: the pending queue.
    let vesna = seed_user(&pool, "vesna", "vesna@example.com", PASSWORD).await;
    sqlx::query!(
        "UPDATE users SET approved = false WHERE account_id = $1",
        vesna.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let overview = get(&app, "/admin", Some(&cookie)).await;
    assert_eq!(overview.status, StatusCode::OK);
    assert!(overview.body.contains("Pending review"));
    // maud escapes the `&` between query params, so the href is `&amp;`.
    assert!(
        overview
            .body
            .contains("/admin/accounts?origin=local&amp;status=pending")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_reviews_trending_tags(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let tag_id = tag::ensure(&pool, "plamenu").await.unwrap();
    tag_trend::upsert(&pool, tag_id, 2.5, false).await.unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/trends", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("#plamenu"));
    assert!(page.body.contains("Pending review"));

    let csrf = csrf_of(&page.body);
    let resp = post_form(
        &app,
        &format!("/web/admin/trends/tags/{tag_id}/review"),
        &cookie,
        &[("csrf", &csrf), ("op", "approve")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    let reviewed = tag::admin_find(&pool, tag_id).await.unwrap().unwrap();
    assert_eq!(reviewed.trendable, Some(true));
    assert!(reviewed.reviewed_at.is_some());

    // An unknown review kind errors instead of 404ing the router.
    let bad = post_form(
        &app,
        &format!("/web/admin/trends/nonsense/{tag_id}/review"),
        &cookie,
        &[("csrf", &csrf), ("op", "approve")],
    )
    .await;
    assert_eq!(bad.status, StatusCode::SEE_OTHER);
    assert!(
        bad.location
            .as_deref()
            .unwrap_or("")
            .contains("flash=error")
    );
}

/// Seeds a trending link with the given override, returning its id. `trendable`
/// = `None` leaves it pending, `Some(true)` approved.
async fn seed_trending_link(
    pool: &PgPool,
    url: &str,
    title: &str,
    score: f64,
    trendable: Option<bool>,
) -> i64 {
    let card = preview_card::upsert(
        pool,
        preview_card::NewPreviewCard {
            url,
            title,
            kind: "link",
            provider_name: "Example News",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    preview_card_trend::upsert(pool, card.id, score, Some("en"), true)
        .await
        .unwrap();
    if let Some(allowed) = trendable {
        preview_card_trend::set_trendable(pool, card.id, allowed)
            .await
            .unwrap();
    }
    card.id
}

/// The trend review queue renders each trending post as a full read-only card
/// (author + content, not bare text), badges pending vs approved items, and —
/// because the preview is read-only (`view::status_preview`) — omits the
/// interaction bar entirely.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_trend_queue_renders_previews_badges_and_omits_the_action_bar(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let author = create_local_account(&pool, "author", "Trend Author").await;

    // A pending trending post (no override) and an approved one.
    let pending = status::create_local(
        &pool,
        status::NewLocalStatus::new(author.id, "<p>pending hot take</p>", "public", None),
    )
    .await
    .unwrap();
    status_trend::upsert(&pool, pending.id, author.id, 20.0, Some("en"), true)
        .await
        .unwrap();
    let approved = status::create_local(
        &pool,
        status::NewLocalStatus::new(author.id, "<p>approved gem</p>", "public", None),
    )
    .await
    .unwrap();
    status_trend::upsert(&pool, approved.id, author.id, 10.0, Some("en"), true)
        .await
        .unwrap();
    status_trend::set_trendable(&pool, approved.id, true)
        .await
        .unwrap();

    // A pending trending link and an approved one.
    seed_trending_link(
        &pool,
        "https://news.example/pending",
        "Pending Story",
        8.0,
        None,
    )
    .await;
    seed_trending_link(
        &pool,
        "https://news.example/approved",
        "Approved Story",
        5.0,
        Some(true),
    )
    .await;

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let page = get(&app, "/admin/trends", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);

    // Full post previews: the content and the author's name, inside the
    // read-only preview wrapper — not bare content.
    assert!(page.body.contains("admin-trend-preview"), "preview wrapper");
    assert!(
        page.body.contains("pending hot take"),
        "pending post content"
    );
    assert!(page.body.contains("approved gem"), "approved post content");
    assert!(
        page.body.contains("Trend Author"),
        "author rendered in the card"
    );
    // Trending links render their title.
    assert!(page.body.contains("Pending Story"));
    assert!(page.body.contains("Approved Story"));

    // Review badges distinguish pending from approved for both posts and links.
    assert!(page.body.contains("Pending review"), "pending badge");
    assert!(
        page.body.contains(">Approved</span>"),
        "approved badge (not the 'Approved Story' title)"
    );

    // The read-only preview shows the post but never the interaction bar — the
    // action-row footer (reply/boost/favourite) must not appear anywhere.
    assert!(
        !page.body.contains("status__actions"),
        "read-only preview omits the interaction bar"
    );
}

/// Rejecting a trending post or link is a dismissal: it leaves the review queue
/// entirely, because the `all_admin` queries drop `trendable = false`.
#[sqlx::test(migrations = "../db/migrations")]
async fn rejecting_a_trending_post_or_link_removes_it_from_the_queue(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let author = create_local_account(&pool, "author", "Trend Author").await;

    let post = status::create_local(
        &pool,
        status::NewLocalStatus::new(author.id, "<p>doomed post</p>", "public", None),
    )
    .await
    .unwrap();
    status_trend::upsert(&pool, post.id, author.id, 12.0, Some("en"), true)
        .await
        .unwrap();
    let card_id = seed_trending_link(
        &pool,
        "https://news.example/doomed",
        "Doomed Story",
        7.0,
        None,
    )
    .await;

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Both are in the queue to begin with.
    let before = get(&app, "/admin/trends", Some(&cookie)).await;
    assert!(before.body.contains("doomed post"));
    assert!(before.body.contains("Doomed Story"));
    let csrf = csrf_of(&before.body);

    for (kind, id) in [("statuses", post.id), ("links", card_id)] {
        let resp = post_form(
            &app,
            &format!("/web/admin/trends/{kind}/{id}/review"),
            &cookie,
            &[("csrf", &csrf), ("op", "reject")],
        )
        .await;
        assert_eq!(resp.status, StatusCode::SEE_OTHER);
        assert!(
            resp.location
                .as_deref()
                .unwrap_or("")
                .contains("flash=applied"),
            "{kind} reject applied"
        );
    }

    // The DB helpers the page reads now drop both, and the link's override is
    // recorded as rejected.
    assert!(
        status_trend::all_admin(&pool, 20, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        preview_card_trend::all_admin(&pool, 20, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        preview_card_trend::trendable_of(&pool, card_id)
            .await
            .unwrap(),
        Some(Some(false)),
    );

    // The queue page no longer shows either item.
    let after = get(&app, "/admin/trends", Some(&cookie)).await;
    assert!(!after.body.contains("doomed post"), "post dismissed");
    assert!(!after.body.contains("Doomed Story"), "link dismissed");
    assert!(after.body.contains("Nothing is trending."));
    assert!(after.body.contains("No links are trending."));
}

/// Unlike a post or link, rejecting a trending hashtag keeps it listed — marked
/// "Rejected" — so a moderator can still see and reverse the decision.
#[sqlx::test(migrations = "../db/migrations")]
async fn rejecting_a_trending_hashtag_keeps_it_listed_as_rejected(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let tag_id = tag::ensure(&pool, "spammy").await.unwrap();
    tag_trend::upsert(&pool, tag_id, 3.0, false).await.unwrap();

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let page = get(&app, "/admin/trends", Some(&cookie)).await;
    assert!(page.body.contains("#spammy"));
    assert!(page.body.contains("Pending review"));
    let csrf = csrf_of(&page.body);

    let resp = post_form(
        &app,
        &format!("/web/admin/trends/tags/{tag_id}/review"),
        &cookie,
        &[("csrf", &csrf), ("op", "reject")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    let after = get(&app, "/admin/trends", Some(&cookie)).await;
    assert!(
        after.body.contains("#spammy"),
        "rejected hashtag stays listed"
    );
    assert!(
        after.body.contains(">Rejected</span>"),
        "marked as rejected"
    );
    let reviewed = tag::admin_find(&pool, tag_id).await.unwrap().unwrap();
    assert_eq!(reviewed.trendable, Some(false));
}

/// Boost collapse: several people boosting the same post cost one card
/// naming all of them, and the API keeps every row. A community's Announce
/// This is exempt — it is the post reaching the community, not a repetition.
#[sqlx::test(migrations = "../db/migrations")]
async fn home_merges_repeated_boosts_into_one_card(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let erin = create_local_account(&pool, "erin", "Erin").await;
    let boosters = ["bob", "carol", "dave"];
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    let app = build_router(state.clone());

    // Alice follows the three boosters but not the author, so the post itself
    // is not in her feed — only the boosts of it are.
    let erin_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(erin.id, "<p>much-boosted</p>", "public", None),
    )
    .await
    .unwrap();
    let mut ids = Vec::new();
    for name in boosters {
        let account = create_local_account(&pool, name, name).await;
        follow::create(&pool, alice.id, account.id, None)
            .await
            .unwrap();
        status::create_local_reblog(&pool, account.id, erin_post.id)
            .await
            .unwrap();
        ids.push(account.id);
    }

    // A community alice follows announces the same post.
    let (group_account, _) = plamenu::groups::create_group(
        &state,
        plamenu::groups::CreateGroupParams {
            name: "hiking",
            display_name: "Hiking",
            membership_policy: plamenu_db::group::MembershipPolicy::Open,
            posting_policy: plamenu_db::group::PostingPolicy::Members,
            created_by: alice.id,
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    .unwrap();
    follow::create(&pool, alice.id, group_account.id, None)
        .await
        .unwrap();
    status::create_local_reblog(&pool, group_account.id, erin_post.id)
        .await
        .unwrap();

    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
    // Five rows in the feed (three boosts, one announce, alice's own group
    // membership post if any) render as two cards: the merged boost and the
    // community's announce.
    assert_eq!(
        home.body.matches("much-boosted").count(),
        2,
        "one merged boost card plus the community's own announce"
    );
    assert_eq!(
        home.body.matches("posted in").count(),
        1,
        "the community announce is never merged away"
    );
    // The merged card names every booster and keys all three for the in-place
    // moderation JS.
    for name in boosters {
        assert!(home.body.contains(name), "{name} named on the merged card");
    }
    let plural = format!(r#"data-booster-id="{} {} {}""#, ids[2], ids[1], ids[0]);
    assert!(
        home.body.contains(&plural),
        "booster ids go plural, newest boost first: {plural}"
    );

    // The API is untouched: three boost rows plus the announce, as before.
    let rows = status::home_timeline(
        &pool,
        alice.id,
        plamenu_db::user::TimelineOrder::default(),
        None,
        20,
    )
    .await
    .unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.reblog_of_id == Some(erin_post.id))
            .count(),
        4,
        "collapse is presentation only"
    );

    // A reader who wants the firehose turns it off for themselves.
    let user_row = user::find_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    let settings = user::settings_by_user_id(&pool, user_row.id)
        .await
        .unwrap()
        .unwrap();
    user::update_settings(
        &pool,
        user_row.id,
        user::UserSettings {
            reading_collapse_boosts: false,
            ..settings
        },
    )
    .await
    .unwrap()
    .unwrap();
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(
        home.body.matches("much-boosted").count(),
        4,
        "opted out: one card per boost again"
    );
}

/// Paging survives collapsing (the cursor is computed from the raw rows, not
/// the cards), and the operator's lookback window suppresses a boost of a post
/// already shown on an earlier page — the one case where a row does disappear,
/// because there is no card left on this page to merge into.
#[sqlx::test(migrations = "../db/migrations")]
async fn boost_collapse_leaves_paging_alone_and_honours_the_lookback(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let erin = create_local_account(&pool, "erin", "Erin").await;
    let frank = create_local_account(&pool, "frank", "Frank").await;
    follow::create(&pool, alice.id, frank.id, None)
        .await
        .unwrap();

    // The post itself is by an account alice does not follow, so only boosts
    // of it reach her feed.
    let erin_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(erin.id, "<p>much-boosted</p>", "public", None),
    )
    .await
    .unwrap();
    let boost = async |name: &str| {
        let account = create_local_account(&pool, name, name).await;
        follow::create(&pool, alice.id, account.id, None)
            .await
            .unwrap();
        status::create_local_reblog(&pool, account.id, erin_post.id)
            .await
            .unwrap();
    };
    // Oldest first: carol's boost sinks to page two, the fillers fill page one,
    // and bob and dave's boosts sit at the top where they merge.
    boost("carol").await;
    let mut filler = Vec::new();
    for i in 0..19 {
        filler.push(
            status::create_local(
                &pool,
                status::NewLocalStatus::new(
                    frank.id,
                    &format!("<p>filler-{i}</p>"),
                    "public",
                    None,
                ),
            )
            .await
            .unwrap(),
        );
    }
    boost("dave").await;
    boost("bob").await;

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let first = get(&app, "/", Some(&cookie)).await;
    assert_eq!(
        first.body.matches("much-boosted").count(),
        1,
        "the two top boosts merged into one card"
    );
    // The page came back full (20 rows), so it pages on the 20th *row* — the
    // 18th filler — even though the cards above it number 19.
    let cursor = filler[filler.len() - 18].id;
    assert!(
        first.body.contains(&format!("/?max_id={cursor}")),
        "the cursor is the last raw row, not the last card"
    );

    // Page two, window off (the default): carol's boost shows, because nothing
    // says the post was already seen.
    let second = get(&app, &format!("/?max_id={cursor}"), Some(&cookie)).await;
    assert_eq!(
        second.body.matches("much-boosted").count(),
        1,
        "page two carries the deep boost"
    );

    // With a window, the same page drops it: the post was on page one.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            boost_collapse_lookback: 80,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    // A fresh state, so the settings cache reads the new value.
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let second = get(&app, &format!("/?max_id={cursor}"), Some(&cookie)).await;
    assert!(
        !second.body.contains("much-boosted"),
        "already shown above the page"
    );
    assert!(
        second.body.contains("filler-0"),
        "everything else on the page is untouched"
    );
}

/// When the boosted post is itself on the page it wins the card, hoisted
/// to the newest occurrence's slot, and the boosters ride along as an "also
/// boosted by" line rather than a separate card.
#[sqlx::test(migrations = "../db/migrations")]
async fn home_folds_a_boost_into_the_post_it_boosts(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let erin = create_local_account(&pool, "erin", "Erin").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    follow::create(&pool, alice.id, erin.id, None)
        .await
        .unwrap();
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();

    let erin_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(erin.id, "<p>the-original</p>", "public", None),
    )
    .await
    .unwrap();
    status::create_local_reblog(&pool, bob.id, erin_post.id)
        .await
        .unwrap();

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
    assert_eq!(
        home.body.matches("the-original").count(),
        1,
        "the post and the boost of it are one card"
    );
    assert!(
        home.body.contains("also boosted by"),
        "the card is the post, with the booster named"
    );
    assert!(
        home.body
            .contains(&format!(r#"id="post-{}""#, erin_post.id)),
        "the surviving card is the post itself"
    );
    // The post's own card is not a boost card at all — muting the booster must
    // dim the name in the line, not a post that is in the feed on its author's
    // account.
    assert!(
        !home.body.contains(r#"data-kind="boost""#),
        "no boost card left once the boost folded into the post"
    );
}

/// Thread grouping: a reply and the post it answers both land in the page
/// and are drawn together, parent first, at the newest one's position — with
/// everything else in the feed left where it was.
#[sqlx::test(migrations = "../db/migrations")]
async fn home_groups_a_reply_with_the_post_it_answers(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    for account in [&bob, &carol] {
        follow::create(&pool, alice.id, account.id, None)
            .await
            .unwrap();
    }

    let root = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "<p>the-root</p>", "public", None),
    )
    .await
    .unwrap();
    // Posted between the two, so feed order alone would separate them.
    status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "<p>the-loner</p>", "public", None),
    )
    .await
    .unwrap();
    status::create_local(
        &pool,
        status::NewLocalStatus::new(carol.id, "<p>the-answer</p>", "public", Some(root.id)),
    )
    .await
    .unwrap();

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
    let at = |needle: &str| home.body.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(
        at("the-root") < at("the-answer"),
        "the parent is hoisted above the reply that answers it"
    );
    assert!(
        at("the-answer") < at("the-loner"),
        "the group takes the newest member's slot, above the older loner"
    );
    // Two authors, so it reads as a conversation rather than one person's thread.
    assert!(home.body.contains("thread-group--conversation"));
    assert!(
        home.body.contains(r#"role="group""#),
        "the grouping is named for a screen reader, which cannot see the rail"
    );

    // Presentation only: the API keeps feed order and knows nothing of groups.
    let rows = status::home_timeline(
        &pool,
        alice.id,
        plamenu_db::user::TimelineOrder::default(),
        None,
        20,
    )
    .await
    .unwrap();
    let contents: Vec<&str> = rows.iter().map(|row| row.content.as_str()).collect();
    assert_eq!(
        contents,
        ["<p>the-answer</p>", "<p>the-loner</p>", "<p>the-root</p>"]
    );
}

/// A long exchange keeps its ends and folds the middle into a disclosure, and a
/// boost of one of its posts never joins the group — it is in the feed because
/// someone repeated it, not because of what it answers.
#[sqlx::test(migrations = "../db/migrations")]
async fn a_long_thread_folds_its_middle_and_leaves_boosts_out(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    for account in [&bob, &dave] {
        follow::create(&pool, alice.id, account.id, None)
            .await
            .unwrap();
    }

    let erin = create_local_account(&pool, "erin", "Erin").await;
    let mut parent = None;
    for i in 0..4 {
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, &format!("<p>chain-{i}</p>"), "public", parent),
        )
        .await
        .unwrap();
        parent = Some(post.id);
    }
    // Dave boosts a stranger's post — unrelated to bob's thread, and the newest
    // row in the feed.
    let stray = status::create_local(
        &pool,
        status::NewLocalStatus::new(erin.id, "<p>the-stray</p>", "public", None),
    )
    .await
    .unwrap();
    status::create_local_reblog(&pool, dave.id, stray.id)
        .await
        .unwrap();

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(
        home.body.contains("thread-group__folded"),
        "four posts: the middle two fold away"
    );
    assert!(
        home.body.contains("Show 2 more posts in between"),
        "the disclosure says how much it holds"
    );
    // One author throughout, so it is a thread rather than a conversation.
    assert!(!home.body.contains("thread-group--conversation"));
    // The boost keeps its own card, outside the group and above it.
    let group_at = home.body.find("thread-group").unwrap();
    let boost_at = home.body.find(r#"data-kind="boost""#).unwrap();
    assert!(
        boost_at < group_at,
        "the boost is the newest row and keeps that position"
    );
}

/// The seam: a post hoisted to the top of the page because the page
/// also carried boosts of it is still a post, so its thread closes around
/// it — and the group inherits the slot the boost won, so the reader's place in
/// the feed does not move.
#[sqlx::test(migrations = "../db/migrations")]
async fn a_boosted_post_joins_its_thread_and_keeps_the_booster_line(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    for account in [&bob, &dave] {
        follow::create(&pool, alice.id, account.id, None)
            .await
            .unwrap();
    }

    let mut parent = None;
    let mut ids = Vec::new();
    for i in 0..3 {
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob.id, &format!("<p>chain-{i}</p>"), "public", parent),
        )
        .await
        .unwrap();
        parent = Some(post.id);
        ids.push(post.id);
    }
    // Dave boosts the middle of bob's thread, after the whole thread was posted.
    status::create_local_reblog(&pool, dave.id, ids[1])
        .await
        .unwrap();

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
    let at = |needle: &str| home.body.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert_eq!(
        home.body.matches("chain-1").count(),
        1,
        "the boost folded into the post it boosts"
    );
    assert!(
        home.body.contains("also boosted by"),
        "the booster line rides along into the group"
    );
    assert!(
        at("chain-0") < at("chain-1") && at("chain-1") < at("chain-2"),
        "the thread still reads root-down"
    );
    assert!(
        !home.body.contains(r#"data-kind="boost""#),
        "no separate boost card survives"
    );
}

/// Reply hints: when the post being replied to is *not* on the page, the
/// card carries a line of it — who and what — instead of only "Replying to
/// @someone". A parent behind a content warning shows nothing, following
/// Phanpy: the reader put a gate there and an excerpt would walk past it.
#[sqlx::test(migrations = "../db/migrations")]
async fn a_reply_shows_a_line_of_the_parent_that_is_off_the_page(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    custom_emoji::create_local(&pool, "party", "reply-peek.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    let bob = create_local_account(&pool, "bob", "Bob :party:").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    // Alice follows carol but not bob, so bob's posts are never in her feed and
    // carol's replies to them arrive without their other half.
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();

    let plain = status::create_local(
        &pool,
        status::NewLocalStatus::new(
            bob.id,
            "<p>@carol @alice what-bob-said :party:</p>",
            "public",
            None,
        ),
    )
    .await
    .unwrap();
    let mut warned = status::NewLocalStatus::new(bob.id, "<p>the-warned-body</p>", "public", None);
    warned.spoiler_text = "spoilers";
    let warned = status::create_local(&pool, warned).await.unwrap();
    for parent in [plain.id, warned.id] {
        status::create_local(
            &pool,
            status::NewLocalStatus::new(carol.id, "<p>carol-answers</p>", "public", Some(parent)),
        )
        .await
        .unwrap();
    }

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(
        home.body.contains("status__reply-peek"),
        "the reply carries a look at its parent"
    );
    let peek_start = home.body.find(r#"<a class="status__reply-peek""#).unwrap();
    let peek_tail = &home.body[peek_start..];
    let peek = &peek_tail[..peek_tail.find("</a>").unwrap() + "</a>".len()];
    assert!(
        peek.contains(r#"<svg class="icon""#),
        "the peek carries the reply-arrow icon: {peek}"
    );
    assert!(
        peek.contains("what-bob-said"),
        "the peek quotes the parent's useful first line"
    );
    assert!(
        !peek.contains("@carol") && !peek.contains("@alice"),
        "leading mentions are removed from the body excerpt: {peek}"
    );
    assert!(peek.contains("Bob"), "and names its author");
    assert_eq!(
        peek.matches(r#"alt=":party:""#).count(),
        2,
        "custom emoji render in both the author name and excerpt: {peek}"
    );
    assert!(
        !home.body.contains("the-warned-body"),
        "a content-warned parent peeks as nothing at all"
    );
    // The warned parent's reply still says it is a reply — it just says no more.
    assert!(home.body.contains("Replying to"));

    // Presentation only: nothing about the peek reaches the timeline rows.
    let rows = status::home_timeline(
        &pool,
        alice.id,
        plamenu_db::user::TimelineOrder::default(),
        None,
        20,
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "two replies, no parents");
}

/// A parent that *is* on the page needs no peek — the thread grouping already
/// put the two cards together — and a muted author's post never peeks at all.
#[sqlx::test(migrations = "../db/migrations")]
async fn a_peek_is_skipped_for_a_parent_on_the_page_or_from_a_muted_author(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    for account in [&bob, &carol] {
        follow::create(&pool, alice.id, account.id, None)
            .await
            .unwrap();
    }
    let parent = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "<p>what-bob-said</p>", "public", None),
    )
    .await
    .unwrap();
    status::create_local(
        &pool,
        status::NewLocalStatus::new(carol.id, "<p>carol-answers</p>", "public", Some(parent.id)),
    )
    .await
    .unwrap();

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(
        !home.body.contains("status__reply-peek"),
        "both halves are on the page and grouped; a peek would repeat one"
    );
    assert!(home.body.contains("thread-group"));

    // Muting bob takes his post out of the feed — and out of the peek that
    // would otherwise have carried it back in.
    mute::upsert(&pool, alice.id, bob.id, true, None)
        .await
        .unwrap();
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(
        !home.body.contains("what-bob-said"),
        "a muted author's post does not return as a hint"
    );
    assert!(home.body.contains("carol-answers"));
}

/// A reply whose parent never arrived has, until now, shown the "on {host}"
/// fallback forever — nothing ever went back for the parent. Rendering the feed
/// is that trigger: the parent is fetched out of band and adoption links
/// the reply. Exact resolution is context-only, so the next view uses the
/// parent peek without injecting the resolved object as its own timeline card.
#[sqlx::test(migrations = "../db/migrations")]
async fn an_orphan_reply_chases_its_parent_in_the_background(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    follow::create(&pool, alice.id, stored_bob.id, None)
        .await
        .unwrap();

    let parent_uri = format!("{}/statuses/1", bob.actor.id);
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        parent_uri.clone(),
        serde_json::json!({
            "id": parent_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>the-missing-parent</p>",
            "published": "2026-07-01T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [],
        }),
    );

    // The reply arrived on its own: it names a parent we have never seen.
    let orphan = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/2",
            account_id: stored_bob.id,
            content: "<p>the-orphan-reply</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: Some(&parent_uri),
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();

    let state = common::test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(
        home.body.contains("status__reply-to--unfetched"),
        "this render still shows the fallback — the fetch is out of band"
    );

    // The parent lands shortly after, and the reply is adopted.
    let mut adopted = None;
    for _ in 0..100 {
        if let Some(row) = status::find_by_id(&pool, orphan.id).await.unwrap()
            && row.in_reply_to_id.is_some()
        {
            adopted = Some(row);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let adopted = adopted.expect("the orphan's parent was fetched and adopted it");
    assert_eq!(
        stub.fetches()
            .iter()
            .filter(|uri| **uri == parent_uri)
            .count(),
        1,
        "one request for the parent, not one per card: {:?}",
        stub.fetches()
    );

    // The resolved parent supplies context and the fallback link is gone, but
    // exact resolution never injects a standalone timeline card.
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(home.body.contains("the-missing-parent"));
    assert!(!home.body.contains("status__reply-to--unfetched"));
    assert!(home.body.contains("status__reply-peek"));
    assert!(!home.body.contains("thread-group"));
    let before = stub.fetches().len();
    let _ = get(&app, "/", Some(&cookie)).await;
    assert_eq!(
        stub.fetches().len(),
        before,
        "a resolved parent is never chased again"
    );
    assert_eq!(adopted.id, orphan.id);
}

/// Cold history never performs secondary fetches at ingest time, so opening a
/// hydrated reply's own permalink must be enough local intent to chase its
/// missing parent. These rows cannot appear in the home/list feeds that were
/// previously the only parent-fetch trigger.
#[sqlx::test(migrations = "../db/migrations")]
async fn a_history_orphan_permalink_chases_its_parent(pool: PgPool) {
    seed_alice(&pool).await;
    let bob = RemoteUser::new("history.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let parent_uri = format!("{}/statuses/direct-parent", bob.actor.id);
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        parent_uri.clone(),
        serde_json::json!({
            "id": parent_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>the-direct-parent</p>",
            "published": "2026-08-01T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [],
        }),
    );
    let orphan = status::upsert_remote_with_provenance(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://history.example/users/bob/statuses/history-reply",
            account_id: stored_bob.id,
            content: "<p>the-history-reply</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: Some(&parent_uri),
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
        status::IngestProvenance::History,
    )
    .await
    .unwrap();

    let app = build_router(common::test_state_with(pool.clone(), stub.clone()));
    let cookie = login(&app).await;
    let permalink = format!("/@bob@history.example/{}", orphan.id);
    let first = get(&app, &permalink, Some(&cookie)).await;
    assert_eq!(first.status, StatusCode::OK);

    let mut adopted = None;
    for _ in 0..100 {
        if let Some(row) = status::find_by_id(&pool, orphan.id).await.unwrap()
            && row.in_reply_to_id.is_some()
        {
            adopted = Some(row);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let adopted = adopted.expect("the permalink fetch adopted the history orphan");
    assert_eq!(
        stub.fetches()
            .iter()
            .filter(|uri| **uri == parent_uri)
            .count(),
        1,
        "the parent is fetched once: {:?}",
        stub.fetches()
    );
    let parent = status::find_by_id(&pool, adopted.in_reply_to_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parent.uri.as_deref(), Some(parent_uri.as_str()));
    assert_eq!(
        status::ingest_provenance(&pool, parent.id)
            .await
            .unwrap()
            .as_deref(),
        Some("explicit_resolution")
    );

    let resolved = get(&app, &permalink, Some(&cookie)).await;
    assert_eq!(resolved.status, StatusCode::OK);
    assert!(resolved.body.contains("the-direct-parent"));
    assert!(!resolved.body.contains("unfetched-parent"));
}

/// The home hashtag-source banner flags only posts pulled into the feed
/// *solely* by a followed tag: a stranger's tagged post is banner-flagged, but a
/// followed account's or the viewer's own tagged post — which would be in the
/// feed regardless — is not, even though all three carry the followed tag.
#[sqlx::test(migrations = "../db/migrations")]
async fn home_banner_flags_only_followed_tag_injections(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;

    let rust = tag::ensure(&pool, "rust").await.unwrap();
    tag::follow(&pool, alice.id, rust).await.unwrap();
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();

    // A stranger's tagged post reaches the feed only via the followed tag.
    let stranger_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "<p>stranger on rust</p>", "public", None),
    )
    .await
    .unwrap();
    tag::attach(&pool, stranger_post.id, rust).await.unwrap();
    // A followed account's tagged post — in the feed via the follow, not the tag.
    let followed_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(carol.id, "<p>carol on rust</p>", "public", None),
    )
    .await
    .unwrap();
    tag::attach(&pool, followed_post.id, rust).await.unwrap();
    // The viewer's own tagged post.
    let own_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>my own rust post</p>", "public", None),
    )
    .await
    .unwrap();
    tag::attach(&pool, own_post.id, rust).await.unwrap();

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);

    // All three tagged posts are on the page...
    assert!(home.body.contains("stranger on rust"), "injection present");
    assert!(
        home.body.contains("carol on rust"),
        "followed author present"
    );
    assert!(home.body.contains("my own rust post"), "own post present");
    // ...but exactly one banner, and it links to the followed tag's timeline.
    assert_eq!(
        home.body.matches("In your feed because you follow").count(),
        1,
        "only the stranger's injection is banner-flagged"
    );
    assert!(home.body.contains("/tags/rust"), "banner links to the tag");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_hashtag_registry(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let tag_id = tag::ensure(&pool, "opensource").await.unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/tags", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("#opensource"));
    assert!(page.body.contains("Unreviewed"));

    let csrf = csrf_of(&page.body);
    let resp = post_form(
        &app,
        &format!("/web/admin/tags/{tag_id}/update"),
        &cookie,
        &[
            ("csrf", &csrf),
            ("display_name", "OpenSource"),
            ("usable", "1"),
            ("trendable", "1"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    let updated = tag::admin_find(&pool, tag_id).await.unwrap().unwrap();
    assert_eq!(updated.display_name.as_deref(), Some("OpenSource"));
    assert_eq!(updated.usable, Some(true));
    // The unchecked box means "not listable" — the form is authoritative.
    assert_eq!(updated.listable, Some(false));
    assert_eq!(updated.trendable, Some(true));
    assert!(updated.reviewed_at.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_lists_known_instances(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    seed_remote_account(&pool, "silenced.example", "bob").await;
    seed_remote_account(&pool, "plain.example", "carol").await;
    instance_policy::create_domain_block(
        &pool,
        instance_policy::NewDomainBlock {
            domain: "silenced.example",
            severity: "silence",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/instances", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("plain.example"));
    assert!(page.body.contains("silenced.example"));
    assert!(page.body.contains("Limited"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_oversees_all_invites(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let bob_user = user::find_by_account_id(&pool, bob.id)
        .await
        .unwrap()
        .unwrap();
    let created = invite::create(
        &pool,
        invite::NewInvite {
            user_id: bob_user.id,
            code: "BOBCODE1",
            expires_in: None,
            max_uses: None,
            comment: "",
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Alice (staff) sees bob's invite on the site-wide page…
    let page = get(&app, "/admin/invites", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("BOBCODE1"));
    assert!(page.body.contains("@bob"));
    assert!(page.body.contains("Active"));

    // …and can deactivate it even though it isn't hers.
    let csrf = csrf_of(&page.body);
    let resp = post_form(
        &app,
        &format!("/web/admin/invites/{}/expire", created.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let dead = invite::find_by_code(&pool, "BOBCODE1")
        .await
        .unwrap()
        .unwrap();
    assert!(!dead.valid_for_use());
}

/// A stored remote account on `domain`, without real key generation.
async fn seed_remote_account(pool: &PgPool, domain: &str, username: &str) -> account::Account {
    let uri = format!("https://{domain}/users/{username}");
    account::upsert_remote(
        pool,
        account::RemoteAccountData {
            username,
            domain,
            uri: &uri,
            display_name: "",
            note: "",
            inbox_url: &format!("{uri}/inbox"),
            shared_inbox_url: "",
            public_key_pem: "pub",
            public_key_id: &format!("{uri}#main-key"),
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: Vec::new(),
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: None,
            discoverable: true,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: None,
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_lists_and_shows_accounts(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let list = get(&app, "/admin/accounts", Some(&cookie)).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.body.contains("@bob"));

    let show = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert_eq!(show.status, StatusCode::OK);
    assert!(show.body.contains("@bob"));
    assert!(show.body.contains("Take action"));
    assert!(show.body.contains("Sign-up reason"));
    assert!(show.body.contains("Not provided"));
    assert!(
        show.body
            .contains(&format!("href=\"/admin/custom-emojis/users/{}\"", bob.id))
    );
    assert!(show.body.contains("Moderate this user's custom emoji"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_account_page_shows_saved_registration_metadata(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let alice_user = user::find_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    let invite = invite::create(
        &pool,
        invite::NewInvite {
            user_id: alice_user.id,
            code: "WELCOME42",
            expires_in: None,
            max_uses: None,
            comment: "",
        },
    )
    .await
    .unwrap();
    let redirect_uris = vec!["urn:ietf:wg:oauth:2.0:oob".to_owned()];
    let signup_app = oauth::create_app(
        &pool,
        oauth::NewApp {
            name: "Community browser",
            website: None,
            client_id: "registration-metadata-client",
            client_secret_hash: "hash",
            redirect_uris: &redirect_uris,
            scopes: "write",
        },
    )
    .await
    .unwrap();
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    sqlx::query(
        "UPDATE users
         SET locale = 'ka', sign_up_ip = '203.0.113.42',
             invite_request_text = 'I maintain community tools.\nI would like to help.',
             created_by_application_id = $2, invite_id = $3,
             time_zone = 'Asia/Tbilisi',
             age_verified_at = '2026-08-30T20:15:00Z',
             confirmation_sent_at = '2026-08-30T20:16:00Z',
             confirmed_at = '2026-08-30T20:17:00Z'
         WHERE account_id = $1",
    )
    .bind(bob.id)
    .bind(signup_app.id)
    .bind(invite.id)
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let show = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert_eq!(show.status, StatusCode::OK);
    for expected in [
        "Registration",
        "bob@example.com",
        "I maintain community tools.\nI would like to help.",
        "203.0.113.42",
        "Community browser",
        "ka",
        "Asia/Tbilisi",
        "WELCOME42",
        "Minimum age verified",
        "Confirmation e-mail",
    ] {
        assert!(
            show.body.contains(expected),
            "missing {expected}: {}",
            show.body
        );
    }
    assert!(
        show.body
            .contains(&format!(r#"href="/admin/accounts/{}">@alice"#, alice.id)),
        "{}",
        show.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_pending_applications_searches_and_previews_registration_reason(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    sqlx::query(
        "UPDATE users
         SET approved = false,
             invite_request_text = CASE account_id
                 WHEN $1 THEN 'Automated protocol deliverability probe'
                 ELSE 'I would like to join the community'
             END,
             sign_up_ip = CASE account_id
                 WHEN $1 THEN '203.0.113.42'
                 ELSE '203.0.113.43'
             END
         WHERE account_id = ANY($2)",
    )
    .bind(bob.id)
    .bind(vec![bob.id, carol.id])
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    // Search is a case-insensitive substring over the saved reason and keeps
    // the queue filter in the rendered pager/form state.
    let page = get(
        &app,
        "/admin/accounts?origin=local&status=pending&q=DELIVERABILITY%20PROBE",
        Some(&cookie),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("@bob"), "{}", page.body);
    assert!(!page.body.contains("@carol"), "{}", page.body);
    assert!(
        page.body
            .contains("Automated protocol deliverability probe"),
        "{}",
        page.body
    );
    assert!(page.body.contains("bob@example.com"), "{}", page.body);
    assert!(page.body.contains("203.0.113.42"), "{}", page.body);
    assert!(page.body.contains(r#"name="account_id""#), "{}", page.body);
    assert!(page.body.contains("Reject selected"), "{}", page.body);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_pending_queue_distinguishes_page_and_all_matching_selection(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    let mut ids = Vec::new();
    for n in 0..51 {
        let username = format!("probe{n:02}");
        let account = create_local_account(&pool, &username, &username).await;
        user::create(&pool, account.id, None, &hash).await.unwrap();
        ids.push(account.id);
    }
    sqlx::query(
        "UPDATE users
         SET approved = false, invite_request_text = 'shared probe signature'
         WHERE account_id = ANY($1)",
    )
    .bind(&ids)
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let page = get(
        &app,
        "/admin/accounts?origin=local&status=pending&q=probe",
        Some(&cookie),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(page.body.matches(r#"name="account_id""#).count(), 50);
    assert!(
        page.body
            .contains("Select all currently loaded applications"),
        "{}",
        page.body
    );
    assert!(
        page.body
            .contains("Select all 51 matching applications across every result page"),
        "{}",
        page.body
    );
    assert!(page.body.contains("Older →"), "{}", page.body);

    let csrf = csrf_of(&page.body);
    let snapshot_marker = r#"name="snapshot_max_id" value=""#;
    let snapshot_start =
        page.body.find(snapshot_marker).expect("snapshot boundary") + snapshot_marker.len();
    let snapshot = page.body[snapshot_start..]
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let confirmation = post_form(
        &app,
        "/web/admin/accounts/bulk/confirm",
        &cookie,
        &[
            ("csrf", &csrf),
            ("all_matching", "1"),
            ("snapshot_max_id", &snapshot),
            ("q", "probe"),
            ("username", ""),
            ("domain", ""),
        ],
    )
    .await;
    assert!(
        confirmation
            .body
            .contains("Reject 51 selected applications?"),
        "{}",
        confirmation.body
    );
    let selection_marker = r#"name="selection" value=""#;
    let selection_start = confirmation
        .body
        .find(selection_marker)
        .expect("server-side selection token")
        + selection_marker.len();
    let selection = confirmation.body[selection_start..]
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let progress = post_form(
        &app,
        "/web/admin/accounts/bulk/reject",
        &cookie,
        &[("csrf", &csrf), ("selection", &selection)],
    )
    .await;
    assert_eq!(progress.status, StatusCode::OK, "{}", progress.body);
    assert!(
        progress.body.contains("25 of 51 processed"),
        "{}",
        progress.body
    );
    assert!(
        progress.body.contains("Continue with the next batch"),
        "{}",
        progress.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_bulk_rejection_snapshots_revalidates_and_audits(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    sqlx::query(
        "UPDATE users
         SET approved = false, invite_request_text = 'Automated protocol deliverability probe'
         WHERE account_id = ANY($1)",
    )
    .bind(vec![bob.id, carol.id])
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(
        &app,
        "/admin/accounts?origin=local&status=pending&q=deliverability",
        Some(&cookie),
    )
    .await;
    let csrf = csrf_of(&page.body);
    let marker = r#"name="snapshot_max_id" value=""#;
    let start = page.body.find(marker).expect("snapshot boundary") + marker.len();
    let snapshot = page.body[start..].split('"').next().unwrap().to_owned();

    // A matching application that arrives after the list was rendered must
    // not be silently swept into the all-matching selection.
    let dave = seed_user(&pool, "dave", "dave@example.com", PASSWORD).await;
    sqlx::query(
        "UPDATE users
         SET approved = false, invite_request_text = 'Automated protocol deliverability probe'
         WHERE account_id = $1",
    )
    .bind(dave.id)
    .execute(&pool)
    .await
    .unwrap();

    let confirmation = post_form(
        &app,
        "/web/admin/accounts/bulk/confirm",
        &cookie,
        &[
            ("csrf", &csrf),
            ("all_matching", "1"),
            ("snapshot_max_id", &snapshot),
            ("q", "deliverability"),
            ("username", ""),
            ("domain", ""),
        ],
    )
    .await;
    assert_eq!(confirmation.status, StatusCode::OK, "{}", confirmation.body);
    assert!(
        confirmation
            .body
            .contains("Reject 2 selected applications?"),
        "{}",
        confirmation.body
    );
    let selection_marker = r#"name="selection" value=""#;
    let selection_start = confirmation
        .body
        .find(selection_marker)
        .expect("server-side selection token")
        + selection_marker.len();
    let selection = confirmation.body[selection_start..]
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let selected_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT account_id
         FROM admin_account_bulk_selections
         WHERE token = $1
         ORDER BY account_id",
    )
    .bind(&selection)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(selected_ids, vec![bob.id, carol.id]);
    assert!(!selected_ids.contains(&dave.id));

    // Carol is approved between confirmation and execution. Revalidation
    // skips her while rejecting Bob and reports both outcomes.
    assert!(user::approve(&pool, carol.id).await.unwrap());
    let result = post_form(
        &app,
        "/web/admin/accounts/bulk/reject",
        &cookie,
        &[("csrf", &csrf), ("selection", &selection)],
    )
    .await;
    assert_eq!(result.status, StatusCode::SEE_OTHER);
    assert_eq!(
        result.location.as_deref(),
        Some(
            "/admin/accounts?origin=local&status=pending&flash=bulk&rejected=1&skipped=1&failed=0"
        )
    );
    assert!(account::find_by_id(&pool, bob.id).await.unwrap().is_none());
    assert!(
        account::find_by_id(&pool, carol.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(account::find_by_id(&pool, dave.id).await.unwrap().is_some());
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM admin_action_logs WHERE action = 'reject' AND target_id = $1",
    )
    .bind(bob.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit_count, 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_suspends_account_via_web(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let csrf = csrf_of(
        &get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie))
            .await
            .body,
    );
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/action", bob.id),
        &cookie,
        &[("csrf", &csrf), ("type", "suspend"), ("text", "spam")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    // The account is suspended and a strike was recorded with the note.
    let view = account::find_by_id(&pool, bob.id).await.unwrap().unwrap();
    assert!(view.suspended());
    let strikes = account_warning::for_target(&pool, bob.id).await.unwrap();
    assert_eq!(strikes.len(), 1);
    assert_eq!(strikes[0].action, "suspend");
    assert_eq!(strikes[0].text, "spam");

    // Unsuspending through the op verb clears it again.
    let unsuspend = post_form(
        &app,
        &format!("/web/admin/accounts/{}/op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "unsuspend")],
    )
    .await;
    assert_eq!(unsuspend.status, StatusCode::SEE_OTHER);
    assert!(
        !account::find_by_id(&pool, bob.id)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );
}

/// TZ slice 2: the admin console renders in the moderator's zone, not raw
/// UTC. Seeded across a UTC midnight so the two zones disagree about the date
/// — the only case where a zone bug is visible at day resolution.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_audit_log_renders_in_the_moderator_zone(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    sqlx::query("UPDATE users SET time_zone = 'Europe/Berlin' WHERE account_id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let csrf = csrf_of(
        &get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie))
            .await
            .body,
    );
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/action", bob.id),
        &cookie,
        &[("csrf", &csrf), ("type", "suspend"), ("text", "spam")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    // 23:30Z in July is 01:30 the *next* day in Berlin.
    sqlx::query("UPDATE admin_action_logs SET created_at = '2026-07-03T23:30:00Z'")
        .execute(&pool)
        .await
        .unwrap();

    let log = get(&app, "/admin/audit-log", Some(&cookie)).await;
    assert_eq!(log.status, StatusCode::OK);
    // The Berlin reading, not the UTC one.
    assert!(log.body.contains("Jul 4, 2026, 01:30"), "{}", log.body);
    // R4: the machine-readable attribute stays UTC.
    assert!(
        log.body.contains(r#"datetime="2026-07-03T23:30:00Z""#),
        "{}",
        log.body
    );
    // The old hardcoded " UTC" suffix is gone; the zone lives in the chip and
    // the tooltip instead.
    assert!(log.body.contains("Times shown in"), "{}", log.body);
    assert!(log.body.contains("Europe/Berlin ("), "{}", log.body);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_audit_log_records_and_lists_actions(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Empty until something happens.
    let empty = get(&app, "/admin/audit-log", Some(&cookie)).await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(empty.body.contains("No actions have been recorded."));

    // A suspension applied through the web form lands in the log.
    let csrf = csrf_of(
        &get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie))
            .await
            .body,
    );
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/action", bob.id),
        &cookie,
        &[("csrf", &csrf), ("type", "suspend"), ("text", "spam")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    let log = get(&app, "/admin/audit-log", Some(&cookie)).await;
    assert_eq!(log.status, StatusCode::OK);
    assert!(log.body.contains("@alice"));
    assert!(log.body.contains("suspend"));
    assert!(log.body.contains("@bob"));
    // The line links to the target's admin page.
    assert!(
        log.body
            .contains(&format!("href=\"/admin/accounts/{}\"", bob.id))
    );

    // The target-type filter narrows the listing.
    let filtered = get(&app, "/admin/audit-log?target_type=Report", Some(&cookie)).await;
    assert!(filtered.body.contains("No actions have been recorded."));

    // So does the action-verb filter (A5).
    let by_action = get(&app, "/admin/audit-log?action=suspend", Some(&cookie)).await;
    assert!(by_action.body.contains("@bob"));
    let no_action = get(&app, "/admin/audit-log?action=unsuspend", Some(&cookie)).await;
    assert!(no_action.body.contains("No actions have been recorded."));

    // Submitting the filter form with "Any" moderator/target sends empty
    // strings; those mean "no filter", not a query-parse error (was a 500).
    let any = get(
        &app,
        "/admin/audit-log?account_id=&target_type=",
        Some(&cookie),
    )
    .await;
    assert_eq!(any.status, StatusCode::OK);
    assert!(any.body.contains("@alice"));

    // A populated moderator filter still parses.
    let by_alice = get(
        &app,
        &format!("/admin/audit-log?account_id={}&target_type=", alice.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(by_alice.status, StatusCode::OK);
    assert!(by_alice.body.contains("suspend"));

    // The audit-log tab is part of the section nav for staff.
    assert!(log.body.contains("href=\"/admin/audit-log\""));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_records_and_deletes_a_note(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let csrf = csrf_of(
        &get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie))
            .await
            .body,
    );
    let add = post_form(
        &app,
        &format!("/web/admin/accounts/{}/note", bob.id),
        &cookie,
        &[("csrf", &csrf), ("content", "keep an eye on this one")],
    )
    .await;
    assert_eq!(add.status, StatusCode::SEE_OTHER);

    let notes = account_moderation_note::for_target(&pool, bob.id)
        .await
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].account_id, Some(alice.id));
    let show = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert!(show.body.contains("keep an eye on this one"));

    let del = post_form(
        &app,
        &format!("/web/admin/accounts/{}/note/{}/delete", bob.id, notes[0].id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(del.status, StatusCode::SEE_OTHER);
    assert!(
        account_moderation_note::for_target(&pool, bob.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_action_rejects_a_bad_csrf(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/action", bob.id),
        &cookie,
        &[("csrf", "wrong"), ("type", "suspend"), ("text", "")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert!(
        !account::find_by_id(&pool, bob.id)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );
}

/// Files a report from `reporter` against `target`, returning its id.
async fn seed_report(pool: &PgPool, reporter: i64, target: i64, comment: &str) -> i64 {
    report::create(
        pool,
        report::NewReport {
            account_id: reporter,
            target_account_id: target,
            status_ids: &[],
            comment,
            category: "spam",
            forwarded: None,
            rule_ids: None,
            uri: None,
        },
    )
    .await
    .unwrap()
    .id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_lists_and_shows_reports(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    let report_id = seed_report(&pool, bob.id, carol.id, "posting spam links").await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let list = get(&app, "/admin/reports", Some(&cookie)).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.body.contains("@carol"));
    assert!(list.body.contains("Open"));

    let show = get(&app, &format!("/admin/reports/{report_id}"), Some(&cookie)).await;
    assert_eq!(show.status, StatusCode::OK);
    assert!(show.body.contains("@carol"));
    assert!(show.body.contains("@bob"));
    assert!(show.body.contains("posting spam links"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_assigns_and_resolves_a_report(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    let report_id = seed_report(&pool, bob.id, carol.id, "spam").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(
        &get(&app, &format!("/admin/reports/{report_id}"), Some(&cookie))
            .await
            .body,
    );

    // Assign to self.
    let assign = post_form(
        &app,
        &format!("/web/admin/reports/{report_id}/op"),
        &cookie,
        &[("csrf", &csrf), ("op", "assign")],
    )
    .await;
    assert_eq!(assign.status, StatusCode::SEE_OTHER);
    let r = report::find_by_id(&pool, report_id).await.unwrap().unwrap();
    assert_eq!(r.assigned_account_id, Some(alice.id));

    // Resolve, then reopen.
    post_form(
        &app,
        &format!("/web/admin/reports/{report_id}/op"),
        &cookie,
        &[("csrf", &csrf), ("op", "resolve")],
    )
    .await;
    let r = report::find_by_id(&pool, report_id).await.unwrap().unwrap();
    assert!(r.action_taken_at.is_some());
    assert_eq!(r.action_taken_by_account_id, Some(alice.id));

    post_form(
        &app,
        &format!("/web/admin/reports/{report_id}/op"),
        &cookie,
        &[("csrf", &csrf), ("op", "reopen")],
    )
    .await;
    let r = report::find_by_id(&pool, report_id).await.unwrap().unwrap();
    assert!(r.action_taken_at.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_records_and_deletes_a_report_note(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    let report_id = seed_report(&pool, bob.id, carol.id, "spam").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(
        &get(&app, &format!("/admin/reports/{report_id}"), Some(&cookie))
            .await
            .body,
    );

    let add = post_form(
        &app,
        &format!("/web/admin/reports/{report_id}/note"),
        &cookie,
        &[("csrf", &csrf), ("content", "looks like a serial spammer")],
    )
    .await;
    assert_eq!(add.status, StatusCode::SEE_OTHER);
    let notes = report_note::for_report(&pool, report_id).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].account_id, Some(alice.id));
    let show = get(&app, &format!("/admin/reports/{report_id}"), Some(&cookie)).await;
    assert!(show.body.contains("looks like a serial spammer"));

    let del = post_form(
        &app,
        &format!("/web/admin/reports/{report_id}/note/{}/delete", notes[0].id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(del.status, StatusCode::SEE_OTHER);
    assert!(
        report_note::for_report(&pool, report_id)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A3: the report page's category/rules edit form updates the report; the
/// checkbox set is authoritative, so saving with none checked clears the
/// cited rules. An unknown category is refused.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_edits_report_category_and_rules(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    let rule = db_rule::create(&pool, "No spam", "", None).await.unwrap();
    let report_id = seed_report(&pool, bob.id, carol.id, "spam").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let show = get(&app, &format!("/admin/reports/{report_id}"), Some(&cookie)).await;
    assert!(show.body.contains("Edit category"));
    assert!(show.body.contains("No spam"));
    let csrf = csrf_of(&show.body);

    let rule_id = rule.id.to_string();
    let edit = post_form(
        &app,
        &format!("/web/admin/reports/{report_id}/update"),
        &cookie,
        &[
            ("csrf", &csrf),
            ("category", "violation"),
            ("rule_ids", &rule_id),
        ],
    )
    .await;
    assert_eq!(edit.status, StatusCode::SEE_OTHER);
    let updated = report::find_by_id(&pool, report_id).await.unwrap().unwrap();
    assert_eq!(updated.category, "violation");
    assert_eq!(updated.rule_ids.as_deref(), Some(&[rule.id][..]));

    // No boxes checked clears the citation.
    post_form(
        &app,
        &format!("/web/admin/reports/{report_id}/update"),
        &cookie,
        &[("csrf", &csrf), ("category", "violation")],
    )
    .await;
    let cleared = report::find_by_id(&pool, report_id).await.unwrap().unwrap();
    assert_eq!(cleared.rule_ids.as_deref(), Some(&[][..]));

    // A category outside Mastodon's list is refused.
    let bad = post_form(
        &app,
        &format!("/web/admin/reports/{report_id}/update"),
        &cookie,
        &[("csrf", &csrf), ("category", "nonsense")],
    )
    .await;
    assert!(
        bad.location
            .as_deref()
            .unwrap_or("")
            .contains("flash=error")
    );
    let unchanged = report::find_by_id(&pool, report_id).await.unwrap().unwrap();
    assert_eq!(unchanged.category, "violation");
}

/// A3: arriving at the account page from a report threads the report through
/// the action form; applying the action cites the report on the strike and
/// resolves it in the same step, then returns to the report.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_takes_action_from_a_report(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    let cited = status::create_local(
        &pool,
        status::NewLocalStatus::new(carol.id, "<p>reported</p>", "public", None),
    )
    .await
    .unwrap();
    let report_id = report::create(
        &pool,
        report::NewReport {
            account_id: bob.id,
            target_account_id: carol.id,
            status_ids: &[cited.id],
            comment: "spam",
            category: "other",
            forwarded: Some(false),
            rule_ids: None,
            uri: None,
        },
    )
    .await
    .unwrap()
    .id;
    let other_open_report = seed_report(&pool, bob.id, carol.id, "more spam").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // The report page links into the action flow.
    let show = get(&app, &format!("/admin/reports/{report_id}"), Some(&cookie)).await;
    assert!(show.body.contains(&format!(
        "/admin/accounts/{}?report_id={report_id}",
        carol.id
    )));

    // The account page announces the citation and embeds it in the form.
    let account_page = get(
        &app,
        &format!("/admin/accounts/{}?report_id={report_id}", carol.id),
        Some(&cookie),
    )
    .await;
    assert!(account_page.body.contains(&format!("report #{report_id}")));
    assert!(account_page.body.contains("mark it resolved"));
    let csrf = csrf_of(&account_page.body);

    let rid = report_id.to_string();
    let act = post_form(
        &app,
        &format!("/web/admin/accounts/{}/action", carol.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("type", "silence"),
            ("text", "spamming"),
            ("report_id", &rid),
        ],
    )
    .await;
    assert_eq!(act.status, StatusCode::SEE_OTHER);
    // Back to the report, which the action resolved.
    assert_eq!(
        act.location.as_deref(),
        Some(format!("/admin/reports/{report_id}?flash=applied").as_str())
    );
    let resolved = report::find_by_id(&pool, report_id).await.unwrap().unwrap();
    assert!(resolved.action_taken_at.is_some());
    assert_eq!(resolved.action_taken_by_account_id, Some(alice.id));
    assert!(
        report::find_by_id(&pool, other_open_report)
            .await
            .unwrap()
            .unwrap()
            .action_taken_at
            .is_some(),
        "a state-changing action resolves every open report for the target"
    );
    // The strike cites the report; the action itself landed.
    let warnings = account_warning::for_target(&pool, carol.id).await.unwrap();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].report_id, Some(report_id));
    assert_eq!(warnings[0].status_ids, vec![cited.id]);
    let silenced = account::find_by_id(&pool, carol.id).await.unwrap().unwrap();
    assert!(silenced.silenced());

    // A report that names a different account is refused, not mis-cited.
    let other_report = seed_report(&pool, carol.id, bob.id, "retaliation").await;
    let oid = other_report.to_string();
    let mismatched = post_form(
        &app,
        &format!("/web/admin/accounts/{}/action", carol.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("type", "none"),
            ("text", ""),
            ("report_id", &oid),
        ],
    )
    .await;
    assert!(
        mismatched
            .location
            .as_deref()
            .unwrap_or("")
            .contains("flash=error")
    );
}

/// A2: permanent purge requires re-typing the handle, is offered only for a
/// temporarily suspended account, never on the moderator's own account, and
/// retains the reserved tombstone.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_hard_deletes_an_account(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Mastodon only exposes permanent destruction after a reversible local
    // suspension; unsuspended accounts do not get the form.
    let bob_page = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert!(!bob_page.body.contains("Delete account permanently"));
    account::suspend(&pool, bob.id, "local").await.unwrap();
    let bob_page = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert!(bob_page.body.contains("Delete account permanently"));
    // Never on your own account.
    let own_page = get(
        &app,
        &format!("/admin/accounts/{}", alice.id),
        Some(&cookie),
    )
    .await;
    assert!(!own_page.body.contains("Delete account permanently"));
    let csrf = csrf_of(&bob_page.body);

    // A wrong confirmation leaves the account alone.
    let wrong = post_form(
        &app,
        &format!("/web/admin/accounts/{}/op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "destroy"), ("confirm", "@wrong")],
    )
    .await;
    assert!(
        wrong
            .location
            .as_deref()
            .unwrap_or("")
            .contains("flash=error")
    );
    assert!(account::find_by_id(&pool, bob.id).await.unwrap().is_some());

    // The typed handle permanently purges to a reserved tombstone and logs
    // `destroy`.
    let deleted = post_form(
        &app,
        &format!("/web/admin/accounts/{}/op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "destroy"), ("confirm", "@bob")],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert_eq!(
        deleted.location.as_deref(),
        Some("/admin/accounts?flash=applied")
    );
    assert!(account::is_deleted(&pool, bob.id).await.unwrap());
    assert!(account::find_by_id(&pool, bob.id).await.unwrap().is_some());
    let log = admin_action_log::list(
        &pool,
        &admin_action_log::LogFilter {
            action: Some("destroy".into()),
            limit: 10,
            ..admin_action_log::LogFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(log.len(), 1);
}

/// #48: a Moderator holds `MANAGE_USERS` but not `DELETE_USER_DATA`, so the web
/// hard-delete form is not offered and a forged `op=destroy` is refused — the
/// previous code let anyone with `MANAGE_USERS` permanently delete any account.
#[sqlx::test(migrations = "../db/migrations")]
async fn moderator_cannot_hard_delete_an_account(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let moderator = role::find_by_name(&pool, "Moderator")
        .await
        .unwrap()
        .unwrap();
    role::assign_to_account(&pool, alice.id, Some(moderator.id))
        .await
        .unwrap();
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // The destructive form is not rendered for a Moderator lacking the bit.
    let bob_page = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert!(!bob_page.body.contains("Delete account permanently"));
    let csrf = csrf_of(&bob_page.body);

    // A forged submit (correct handle + csrf) is still refused server-side.
    let forged = post_form(
        &app,
        &format!("/web/admin/accounts/{}/op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "destroy"), ("confirm", "@bob")],
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert!(
        account::find_by_id(&pool, bob.id).await.unwrap().is_some(),
        "the account must survive a forbidden destroy"
    );
}

/// A6: without SMTP the mail-dependent verbs disappear and the generated
/// one-time password takes over: shown exactly once, sessions revoked, and
/// the new password actually works.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_sets_a_password_without_smtp(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // The test app has no SMTP: reset-by-mail is not offered, the generator is.
    let page = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert!(!page.body.contains(">Reset password<"));
    assert!(page.body.contains("Generate a new password"));
    assert!(page.body.contains("E-mail is not configured"));
    let csrf = csrf_of(&page.body);

    let set = post_form(
        &app,
        &format!("/web/admin/accounts/{}/user-op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "set_password")],
    )
    .await;
    assert_eq!(set.status, StatusCode::OK);
    assert!(set.body.contains("shown only this once"));
    let marker = "<code>";
    let start = set.body.find(marker).expect("a one-time password") + marker.len();
    let password = set.body[start..].split('<').next().unwrap().to_owned();
    assert_eq!(password.len(), 20);

    let target_user = user::find_by_account_id(&pool, bob.id)
        .await
        .unwrap()
        .unwrap();
    assert!(verify_password(&password, &target_user.password_hash));
    assert!(!verify_password(PASSWORD, &target_user.password_hash));
}

/// O1/O2: the instances page carries the server-wide delivery-health tables —
/// queue summary plus the circuit breaker's unreachable hosts.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_instances_show_delivery_health(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    // An open-breaker host, streak aged past the window like the breaker
    // itself requires.
    reachability::record_failure(
        &pool,
        "down.example",
        reachability::CLASS_TRANSIENT,
        "timeout",
    )
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE host_reachability SET unreachable_since = now(), consecutive_failures = 12
         WHERE host = 'down.example'"
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let page = get(&app, "/admin/instances", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Delivery health"));
    assert!(page.body.contains("The outbound delivery queue is empty."));
    assert!(page.body.contains("down.example"));
    assert!(page.body.contains("timeout"));
    assert!(page.body.contains("id=\"delivery-health\""));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_instance_policy_records(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/instance-policy", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Domain blocks"));
    assert!(page.body.contains("Canonical e-mail blocks"));
    let csrf = csrf_of(&page.body);

    let add_domain = post_form(
        &app,
        "/web/admin/instance-policy/domain-blocks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("domain", "Bad.Example"),
            ("severity", "suspend"),
            ("reject_media", "1"),
            ("private_comment", "staff note"),
            ("public_comment", "public note"),
        ],
    )
    .await;
    assert_eq!(add_domain.status, StatusCode::SEE_OTHER);
    let block = instance_policy::find_domain_block_by_domain(&pool, "bad.example")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(block.severity, "suspend");
    assert!(block.reject_media);

    let update_domain = post_form(
        &app,
        &format!(
            "/web/admin/instance-policy/domain-blocks/{}/update",
            block.id
        ),
        &cookie,
        &[
            ("csrf", &csrf),
            ("severity", "silence"),
            ("reject_reports", "1"),
            ("private_comment", ""),
            ("public_comment", "updated"),
        ],
    )
    .await;
    assert_eq!(update_domain.status, StatusCode::SEE_OTHER);
    let updated = instance_policy::find_domain_block(&pool, block.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.severity, "silence");
    assert!(updated.reject_reports);

    post_form(
        &app,
        "/web/admin/instance-policy/domain-allows",
        &cookie,
        &[("csrf", &csrf), ("domain", "Friend.Example")],
    )
    .await;
    assert!(
        instance_policy::domain_allows_federation(&pool, "friend.example")
            .await
            .unwrap()
    );

    post_form(
        &app,
        "/web/admin/instance-policy/email-domain-blocks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("domain", "Spam.Example"),
            ("allow_with_approval", "1"),
        ],
    )
    .await;
    assert!(
        instance_policy::find_email_domain_block_by_domain(&pool, "spam.example")
            .await
            .unwrap()
            .unwrap()
            .allow_with_approval
    );

    post_form(
        &app,
        "/web/admin/instance-policy/ip-blocks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("ip", "192.0.2.3"),
            ("severity", "no_access"),
            ("comment", "abuse"),
            ("expires_in", ""),
        ],
    )
    .await;
    let ip = instance_policy::find_ip_block_by_ip(&pool, "192.0.2.3/32")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ip.severity, "no_access");

    post_form(
        &app,
        "/web/admin/instance-policy/canonical-email-blocks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("email", "First.Last+tag@Example.COM"),
            ("canonical_email_hash", ""),
        ],
    )
    .await;
    let canonical = instance_policy::list_canonical_email_blocks(
        &pool,
        &instance_policy::Page {
            max_id: None,
            since_id: None,
            min_id: None,
            limit: 10,
        },
    )
    .await
    .unwrap();
    assert_eq!(canonical.len(), 1);

    // A4: the test-an-address probe matches through canonicalization (dots,
    // plus-tags and case fold away) and reports non-matches plainly.
    let hit = get(
        &app,
        "/admin/instance-policy?test_email=firstlast@example.com",
        Some(&cookie),
    )
    .await;
    assert!(hit.body.contains("is blocked by 1 canonical e-mail block"));
    let miss = get(
        &app,
        "/admin/instance-policy?test_email=someone.else@example.com",
        Some(&cookie),
    )
    .await;
    assert!(miss.body.contains("No canonical e-mail block matches"));

    let del = post_form(
        &app,
        &format!(
            "/web/admin/instance-policy/domain-blocks/{}/delete",
            block.id
        ),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(del.status, StatusCode::SEE_OTHER);
    assert!(
        instance_policy::find_domain_block(&pool, block.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_instance_rules(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/rules", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = csrf_of(&page.body);
    let add = post_form(
        &app,
        "/web/admin/rules",
        &cookie,
        &[("csrf", &csrf), ("text", "No spam"), ("hint", "Be human")],
    )
    .await;
    assert_eq!(add.status, StatusCode::SEE_OTHER);
    let rules = db_rule::list_ordered(&pool).await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].text, "No spam");

    let update = post_form(
        &app,
        &format!("/web/admin/rules/{}/update", rules[0].id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("text", "No spam or scams"),
            ("hint", ""),
            ("priority", "3"),
        ],
    )
    .await;
    assert_eq!(update.status, StatusCode::SEE_OTHER);
    let updated = db_rule::list_ordered(&pool).await.unwrap();
    assert_eq!(updated[0].text, "No spam or scams");
    assert_eq!(updated[0].priority, 3);

    let delete = post_form(
        &app,
        &format!("/web/admin/rules/{}/delete", rules[0].id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(delete.status, StatusCode::SEE_OTHER);
    assert!(db_rule::list_ordered(&pool).await.unwrap().is_empty());
}

/// TZ slice 3: announcement scheduling takes a wall-clock reading in the
/// admin's zone, not raw RFC 3339 UTC. Mirrors the composer's
/// `scheduling_a_post_queues_it_instead_of_publishing`: Berlin noon in January
/// (UTC+1) must store as 11:00Z, and echo back as noon on the form.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_schedules_an_announcement_in_their_own_zone(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    sqlx::query("UPDATE users SET time_zone = 'Europe/Berlin' WHERE account_id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/announcements", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    // The fields are pickers now, not free-text RFC 3339.
    assert!(
        page.body
            .contains(r#"type="datetime-local" name="scheduled_at""#),
        "{}",
        page.body
    );
    assert!(page.body.contains("Times shown in"), "{}", page.body);

    let csrf = csrf_of(&page.body);
    let create = post_form(
        &app,
        "/web/admin/announcements",
        &cookie,
        &[
            ("csrf", &csrf),
            ("text", "Maintenance at noon"),
            ("scheduled_at", "2030-01-15T12:00"),
            ("starts_at", ""),
            ("ends_at", ""),
            ("status_ids", ""),
        ],
    )
    .await;
    assert_eq!(create.status, StatusCode::SEE_OTHER);

    let announcements = announcement::list_all(&pool).await.unwrap();
    assert_eq!(announcements.len(), 1);
    let scheduled = announcements[0].scheduled_at.expect("scheduled");
    // Berlin winter is UTC+1, so noon on their clock is 11:00Z.
    assert_eq!(
        scheduled
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap(),
        "2030-01-15T11:00:00Z"
    );

    // And it round-trips: the edit form echoes the reading they typed.
    let page = get(&app, "/admin/announcements", Some(&cookie)).await;
    assert!(
        page.body.contains(r#"value="2030-01-15T12:00""#),
        "{}",
        page.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_announcements(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/announcements", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = csrf_of(&page.body);
    let create = post_form(
        &app,
        "/web/admin/announcements",
        &cookie,
        &[
            ("csrf", &csrf),
            ("text", "Maintenance tonight"),
            ("scheduled_at", ""),
            ("starts_at", ""),
            ("ends_at", ""),
            ("status_ids", ""),
        ],
    )
    .await;
    assert_eq!(create.status, StatusCode::SEE_OTHER);
    let announcements = announcement::list_all(&pool).await.unwrap();
    assert_eq!(announcements.len(), 1);
    assert!(announcements[0].published);

    let update = post_form(
        &app,
        &format!("/web/admin/announcements/{}/update", announcements[0].id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("text", "Maintenance moved"),
            ("scheduled_at", ""),
            ("starts_at", ""),
            ("ends_at", ""),
            ("all_day", "1"),
            ("status_ids", ""),
        ],
    )
    .await;
    assert_eq!(update.status, StatusCode::SEE_OTHER);
    let edited = announcement::find_by_id(&pool, announcements[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited.text, "Maintenance moved");
    assert!(edited.all_day);

    let unpublish = post_form(
        &app,
        &format!("/web/admin/announcements/{}/op", edited.id),
        &cookie,
        &[("csrf", &csrf), ("op", "unpublish")],
    )
    .await;
    assert_eq!(unpublish.status, StatusCode::SEE_OTHER);
    assert!(
        !announcement::find_by_id(&pool, edited.id)
            .await
            .unwrap()
            .unwrap()
            .published
    );

    let publish = post_form(
        &app,
        &format!("/web/admin/announcements/{}/op", edited.id),
        &cookie,
        &[("csrf", &csrf), ("op", "publish")],
    )
    .await;
    assert_eq!(publish.status, StatusCode::SEE_OTHER);
    assert!(
        announcement::find_by_id(&pool, edited.id)
            .await
            .unwrap()
            .unwrap()
            .published
    );

    let delete = post_form(
        &app,
        &format!("/web/admin/announcements/{}/delete", edited.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(delete.status, StatusCode::SEE_OTHER);
    assert!(
        announcement::find_by_id(&pool, edited.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_custom_emojis(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let state = common::test_state_with(pool.clone(), std::sync::Arc::default());
    let app = build_router(state.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/custom-emojis", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Upload emoji"));
    assert!(page.body.contains("admin-emoji-overview"));
    assert!(page.body.contains("Review trending custom emoji"));
    assert!(page.body.contains("Find a local member"));
    assert!(page.body.contains("Custom emoji settings"));
    assert!(!page.body.contains(">Account ID<"));
    assert!(!page.body.contains(">Emoji per member<"));
    let csrf = csrf_of(&page.body);

    let png = sample_png_bytes();
    state
        .media
        .put("personal-admin-test.png", png.clone())
        .await
        .unwrap();
    let custom_emoji::PersonalCreateOutcome::Created(personal_id) =
        custom_emoji::create_personal_upload(
            &pool,
            alice.id,
            "personal_admin_test",
            "personal-admin-test.png",
            "image/png",
            i64::try_from(png.len()).unwrap(),
            Some("Personal"),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let personal = custom_emoji::find_managed_by_id(&pool, personal_id)
        .await
        .unwrap()
        .unwrap();
    custom_emoji::record_post_usage(&pool, alice.id, &[personal.origin_id])
        .await
        .unwrap();

    let member_page = get(
        &app,
        &format!("/admin/custom-emojis/users/{}", alice.id),
        Some(&cookie),
    )
    .await;
    assert!(member_page.body.contains("admin-record admin-emoji"));
    assert!(member_page.body.contains("admin-emoji__moderation"));
    assert!(member_page.body.contains("admin-emoji__promotion"));
    assert!(member_page.body.contains("Make available to everyone"));
    assert!(member_page.body.contains(">Instance category<"));
    assert!(
        member_page
            .body
            .contains(r#"name="category" value="Personal""#)
    );
    assert!(
        !member_page
            .body
            .contains(r#"type="hidden" name="category""#)
    );

    let trending = get(
        &app,
        "/admin/custom-emojis/trending?scope=personal",
        Some(&cookie),
    )
    .await;
    assert!(trending.body.contains("nav-select"));
    assert!(trending.body.contains("admin-record admin-emoji"));
    assert!(trending.body.contains("admin-emoji__promote"));
    assert!(trending.body.contains(">Instance category<"));
    assert!(
        trending
            .body
            .contains(r#"name="category" value="Personal""#)
    );
    assert!(!trending.body.contains(r#"type="hidden" name="category""#));

    // The configured emoji-specific cap, not the generic image-upload cap,
    // governs web uploads and reports both the actual and allowed sizes.
    custom_emoji::set_settings(&pool, 50, 1).await.unwrap();
    let mut oversized = png.clone();
    oversized.resize(2 * 1024, 0);
    let rejected = post_multipart_file(
        &app,
        "/web/admin/custom-emojis",
        &cookie,
        &[("csrf", &csrf), ("shortcode", "too_large")],
        ("image", "large.png", "image/png", &oversized),
    )
    .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    assert!(rejected.body.contains("file is 2 KiB"), "{}", rejected.body);
    assert!(rejected.body.contains("up to 1 KiB"), "{}", rejected.body);
    assert!(custom_emoji::list_local(&pool).await.unwrap().is_empty());
    custom_emoji::set_settings(&pool, 50, 256).await.unwrap();

    let uploaded = post_multipart_file(
        &app,
        "/web/admin/custom-emojis",
        &cookie,
        &[("csrf", &csrf), ("shortcode", "party")],
        ("image", "party.png", "image/png", &png),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::SEE_OTHER);
    let emojis = custom_emoji::list_local(&pool).await.unwrap();
    assert_eq!(emojis.len(), 1);
    let emoji = &emojis[0];
    assert_eq!(emoji.shortcode, "party");
    assert!(!emoji.disabled);
    assert!(emoji.visible_in_picker);
    assert_eq!(emoji.image_content_type.as_deref(), Some("image/png"));
    let file_name = emoji.image_file_name.clone().expect("stored file name");
    assert_eq!(state.media.get(&file_name).await.unwrap(), png);

    let list = get(&app, "/admin/custom-emojis", Some(&cookie)).await;
    assert!(list.body.contains(":party:"));
    assert!(list.body.contains("admin-emoji__edit"));
    assert!(list.body.contains("admin-emoji__actions"));
    let update = post_form(
        &app,
        &format!("/web/admin/custom-emojis/{}/update", emoji.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("disabled", "1"),
            ("category", " reactions "),
        ],
    )
    .await;
    assert_eq!(update.status, StatusCode::SEE_OTHER);
    let updated = custom_emoji::find_local_by_id(&pool, emoji.id)
        .await
        .unwrap()
        .unwrap();
    assert!(updated.disabled);
    assert!(!updated.visible_in_picker);
    // The picker category is trimmed on the way in…
    assert_eq!(updated.category.as_deref(), Some("reactions"));

    // …and a blank submission clears it.
    let clear = post_form(
        &app,
        &format!("/web/admin/custom-emojis/{}/update", emoji.id),
        &cookie,
        &[("csrf", &csrf), ("category", "")],
    )
    .await;
    assert_eq!(clear.status, StatusCode::SEE_OTHER);
    let cleared = custom_emoji::find_local_by_id(&pool, emoji.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.category, None);

    let delete = post_form(
        &app,
        &format!("/web/admin/custom-emojis/{}/delete", emoji.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(delete.status, StatusCode::SEE_OTHER);
    assert!(custom_emoji::list_local(&pool).await.unwrap().is_empty());
    assert!(state.media.get(&file_name).await.is_err());

    let owner_id = alice.id.to_string();
    let promoted = post_form(
        &app,
        &format!("/web/admin/custom-emojis/{personal_id}/promote"),
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_owner", &owner_id),
            ("shortcode", "community_pick"),
            ("category", "Featured members"),
        ],
    )
    .await;
    assert_eq!(promoted.status, StatusCode::SEE_OTHER, "{}", promoted.body);
    assert_eq!(
        promoted.location.as_deref(),
        Some(format!("/admin/custom-emojis/users/{}?saved=1", alice.id).as_str())
    );
    let instance_emoji = custom_emoji::find_local_by_id(&pool, personal_id)
        .await
        .unwrap()
        .expect("promoted personal emoji");
    assert_eq!(instance_emoji.shortcode, "community_pick");
    assert_eq!(instance_emoji.category.as_deref(), Some("Featured members"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn member_manages_personal_emoji_and_permission_only_gates_additions(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let state = common::test_state_with(pool.clone(), std::sync::Arc::default());
    let app = build_router(state.clone());
    let cookie = login(&app).await;
    let page = get(&app, "/settings/custom-emojis", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("50 slots"));
    assert!(page.body.contains("settings-form__group"));
    assert!(page.body.contains("settings-field__hint"));
    assert!(page.body.contains("custom-emoji__collection"));
    let csrf = csrf_of(&page.body);
    let png = sample_png_bytes();
    let uploaded = post_multipart_file(
        &app,
        "/web/settings/custom-emojis",
        &cookie,
        &[
            ("csrf", &csrf),
            ("shortcode", "my_party"),
            ("category", "Mine"),
        ],
        ("image", "party.png", "image/png", &png),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::SEE_OTHER, "{}", uploaded.body);
    assert_eq!(
        uploaded.location.as_deref(),
        Some("/settings/custom-emojis?saved=1")
    );
    let personal = custom_emoji::list_personal(&pool, alice.id).await.unwrap();
    assert_eq!(personal.len(), 1);
    assert!(!personal[0].borrowed);
    let populated_page = get(&app, "/settings/custom-emojis", Some(&cookie)).await;
    assert!(
        populated_page
            .body
            .contains(r#"class="admin-record admin-emoji""#)
    );
    assert!(populated_page.body.contains("<span>Shortcode</span>"));
    assert!(populated_page.body.contains("<span>Category</span>"));
    assert!(populated_page.body.contains("admin-emoji__moderation"));

    let catalog = get(&app, "/web/custom-emojis", Some(&cookie)).await;
    assert_eq!(catalog.status, StatusCode::OK);
    let catalog: serde_json::Value = serde_json::from_str(&catalog.body).unwrap();
    assert_eq!(catalog[0]["shortcode"], "my_party");
    assert_eq!(catalog[0]["_personal"], true);

    let bob = create_local_account(&pool, "emoji_bob", "Bob").await;
    state.media.put("wave.png", png.clone()).await.unwrap();
    let custom_emoji::PersonalCreateOutcome::Created(source_id) =
        custom_emoji::create_personal_upload(
            &pool,
            bob.id,
            "wave",
            "wave.png",
            "image/png",
            i64::try_from(png.len()).unwrap(),
            Some("Hands"),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let borrowed = post_form(
        &app,
        &format!("/web/settings/custom-emojis/{source_id}/borrow"),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(borrowed.status, StatusCode::SEE_OTHER, "{}", borrowed.body);
    let personal = custom_emoji::list_personal(&pool, alice.id).await.unwrap();
    let borrowed = personal
        .iter()
        .find(|emoji| emoji.shortcode == "wave")
        .unwrap();
    assert!(borrowed.borrowed);
    let immutable = post_form(
        &app,
        &format!("/web/settings/custom-emojis/{}/update", borrowed.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("shortcode", "renamed"),
            ("category", "Other"),
        ],
    )
    .await;
    assert_eq!(
        immutable.location.as_deref(),
        Some("/settings/custom-emojis?error=immutable")
    );

    // Reaction-only custom emoji are borrow candidates too. The post text has
    // no shortcode at all: one reaction uses another local member's personal
    // emoji and the other carries a remote emoji URL. Both must make the
    // member menu appear and both must be present on its review page.
    state
        .media
        .put("reaction-wave.png", png.clone())
        .await
        .unwrap();
    let custom_emoji::PersonalCreateOutcome::Created(local_reaction_id) =
        custom_emoji::create_personal_upload(
            &pool,
            bob.id,
            "local_reaction",
            "reaction-wave.png",
            "image/png",
            i64::try_from(png.len()).unwrap(),
            Some("Reactions"),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let local_reaction = custom_emoji::find_managed_by_id(&pool, local_reaction_id)
        .await
        .unwrap()
        .unwrap();
    let permalink = compose(&app, &cookie, "This post has reaction emoji only").await;
    let reaction_status_id: i64 = permalink.rsplit('/').next().unwrap().parse().unwrap();
    let local_reaction_url = format!("https://{}/media/reaction-wave.png", common::TEST_DOMAIN);
    reaction::create_custom(
        &pool,
        reaction::NewReaction {
            account_id: bob.id,
            status_id: reaction_status_id,
            name: "local_reaction",
            custom_emoji_url: Some(&local_reaction_url),
            uri: None,
        },
        local_reaction.id,
        local_reaction.origin_id,
    )
    .await
    .unwrap();

    let remote_reaction_url = "https://reactions.example/emoji/remote_reaction.png";
    custom_emoji::upsert_remote(
        &pool,
        custom_emoji::RemoteEmojiData {
            shortcode: "remote_reaction",
            domain: "reactions.example",
            uri: None,
            image_remote_url: remote_reaction_url,
            updated: None,
        },
    )
    .await
    .unwrap();
    reaction::create(
        &pool,
        reaction::NewReaction {
            account_id: bob.id,
            status_id: reaction_status_id,
            name: "remote_reaction",
            custom_emoji_url: Some(remote_reaction_url),
            uri: None,
        },
    )
    .await
    .unwrap();

    let thread = get(&app, &permalink, Some(&cookie)).await;
    let borrow_path = format!("/settings/custom-emojis/borrow/status/{reaction_status_id}");
    assert!(thread.body.contains(&borrow_path), "{}", thread.body);
    assert!(thread.body.contains("Borrow custom emojis"));
    assert!(!thread.body.contains("Borrow emoji as personal"));

    let candidates = get(&app, &borrow_path, Some(&cookie)).await;
    assert_eq!(candidates.status, StatusCode::OK);
    assert!(candidates.body.contains(":local_reaction:"));
    assert!(candidates.body.contains(":remote_reaction:"));
    assert!(
        candidates
            .body
            .contains(r#"class="admin-record admin-emoji""#)
    );
    assert!(candidates.body.contains("Each borrowed emoji counts"));
    assert!(candidates.body.contains("custom-emoji__candidates"));
    assert!(candidates.body.contains(">Borrow</button>"));
    assert!(!candidates.body.contains("Borrow as personal"));

    // A role without the member capability blocks new uploads, with a useful
    // reason, but does not strand existing collection management.
    let restricted = role::create(&pool, "Restricted", "", 0, 0, false)
        .await
        .unwrap();
    role::assign_to_account(&pool, alice.id, Some(restricted.id))
        .await
        .unwrap();
    let denied = post_multipart_file(
        &app,
        "/web/settings/custom-emojis",
        &cookie,
        &[("csrf", &csrf), ("shortcode", "second"), ("category", "")],
        ("image", "second.png", "image/png", &png),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(
        denied
            .body
            .contains("does not allow uploading or borrowing")
    );

    let deleted = post_form(
        &app,
        &format!(
            "/web/settings/custom-emojis/{}/delete",
            personal.iter().find(|emoji| !emoji.borrowed).unwrap().id
        ),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    let remaining = custom_emoji::list_personal(&pool, alice.id).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert!(remaining[0].borrowed);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn member_borrow_uses_federated_source_and_renders_failures_in_context(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let stub = std::sync::Arc::new(StubFederation::default());
    let state = common::test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(
        &get(&app, "/settings/custom-emojis", Some(&cookie))
            .await
            .body,
    );

    let remote_user = RemoteUser::new("remote.example", "emoji_source");
    let stored_remote = remote::store_remote_actor(&pool, &remote_user.actor)
        .await
        .unwrap();
    let source_url = "https://remote.example/emoji/from_origin.png";
    let broken_url = "https://remote.example/emoji/broken.png";
    for (shortcode, image_remote_url) in
        [("from_origin", source_url), ("broken_source", broken_url)]
    {
        custom_emoji::upsert_remote(
            &pool,
            custom_emoji::RemoteEmojiData {
                shortcode,
                domain: "remote.example",
                uri: None,
                image_remote_url,
                updated: None,
            },
        )
        .await
        .unwrap();
    }
    let source = custom_emoji::lookup(&pool, &["from_origin".to_owned()], Some("remote.example"))
        .await
        .unwrap()
        .remove(0);
    let broken = custom_emoji::lookup(&pool, &["broken_source".to_owned()], Some("remote.example"))
        .await
        .unwrap()
        .remove(0);

    // This is the staging regression: the media proxy has transcoded its
    // display cache to AVIF, while the origin still advertises a valid PNG.
    let cached = b"locally processed AVIF display derivative".to_vec();
    state
        .media
        .put("from-origin.emoji.avif", cached.clone())
        .await
        .unwrap();
    custom_emoji::set_image_file(
        &pool,
        source.id,
        "from-origin.emoji.avif",
        "image/avif",
        i64::try_from(cached.len()).unwrap(),
    )
    .await
    .unwrap();
    let png = sample_png_bytes();
    stub.serve_media(source_url, "image/png", png.clone());
    stub.serve_media(broken_url, "image/png", b"not an image".to_vec());

    let post = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            uri: "https://remote.example/users/emoji_source/statuses/1",
            account_id: stored_remote.id,
            content: "<p>:from_origin: :broken_source:</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: Some("https://remote.example/@emoji_source/1"),
            quote_approval_policy: 0,
            title: None,
            object_type: None,
            external_url: None,
        },
    )
    .await
    .unwrap();
    let borrow_path = format!("/settings/custom-emojis/borrow/status/{}", post.id);
    let candidates = get(&app, &borrow_path, Some(&cookie)).await;
    assert_eq!(candidates.status, StatusCode::OK);
    assert!(candidates.body.contains(":from_origin:"));
    assert!(candidates.body.contains(":broken_source:"));
    assert!(
        candidates
            .body
            .contains(&format!(r#"name="return_to" value="{borrow_path}""#))
    );

    let borrowed = post_form(
        &app,
        &format!("/web/settings/custom-emojis/{}/borrow", source.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", &borrow_path)],
    )
    .await;
    assert_eq!(borrowed.status, StatusCode::SEE_OTHER, "{}", borrowed.body);
    assert_eq!(
        borrowed.location.as_deref(),
        Some("/settings/custom-emojis?saved=1")
    );
    let personal = custom_emoji::list_personal(&pool, alice.id).await.unwrap();
    let copy = personal
        .iter()
        .find(|emoji| emoji.shortcode == "from_origin")
        .unwrap();
    assert_eq!(copy.image_content_type.as_deref(), Some("image/png"));
    assert_eq!(
        state
            .media
            .get(copy.image_file_name.as_deref().unwrap())
            .await
            .unwrap(),
        png
    );
    assert_eq!(stub.media_fetches(), vec![source_url]);

    let rejected = post_form(
        &app,
        &format!("/web/settings/custom-emojis/{}/borrow", broken.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", &borrow_path)],
    )
    .await;
    assert_eq!(rejected.status, StatusCode::SEE_OTHER, "{}", rejected.body);
    let error_location = rejected.location.expect("contextual error redirect");
    assert!(error_location.starts_with(&format!("{borrow_path}?error=")));
    let error_page = get(&app, &error_location, Some(&cookie)).await;
    assert_eq!(error_page.status, StatusCode::OK);
    assert!(error_page.content_type.starts_with("text/html"));
    assert!(error_page.body.contains("Emoji used in this post"));
    assert!(
        error_page
            .body
            .contains(r#"class="settings__error" role="alert""#)
    );
    assert!(error_page.body.contains(
        "The borrowed emoji image was rejected: Validation failed: the file is not a supported PNG, GIF, or WebP image"
    ));
    assert_eq!(stub.media_fetches(), vec![source_url, broken_url]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_webhooks(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/webhooks", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Create webhook"));
    let csrf = csrf_of(&page.body);

    let created = post_form(
        &app,
        "/web/admin/webhooks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("url", "https://hooks.example/plamenu"),
            ("account_created", "1"),
            ("report_created", "1"),
            ("template", ""),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    let hooks = webhook::list(&pool).await.unwrap();
    assert_eq!(hooks.len(), 1);
    let hook = &hooks[0];
    assert_eq!(hook.url, "https://hooks.example/plamenu");
    assert_eq!(
        hook.events,
        vec![webhook::ACCOUNT_CREATED, webhook::REPORT_CREATED]
    );
    assert!(hook.enabled);
    assert!(hook.secret.len() >= 12);
    let original_secret = hook.secret.clone();

    let reveal = get(
        &app,
        created
            .location
            .as_deref()
            .expect("one-time reveal redirect"),
        Some(&cookie),
    )
    .await;
    assert_eq!(reveal.status, StatusCode::OK);
    assert!(reveal.body.contains(&original_secret));

    let list = get(&app, "/admin/webhooks", Some(&cookie)).await;
    assert!(list.body.contains("https://hooks.example/plamenu"));
    assert!(list.body.contains("Account created"));
    assert!(!list.body.contains(&original_secret));
    assert!(list.body.contains("Secret fingerprint:"));
    let update = post_form(
        &app,
        &format!("/web/admin/webhooks/{}/update", hook.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("url", "https://hooks.example/changed"),
            ("report_updated", "1"),
            ("template", "{{ payload }}"),
        ],
    )
    .await;
    assert_eq!(update.status, StatusCode::SEE_OTHER);
    let updated = webhook::find_by_id(&pool, hook.id).await.unwrap().unwrap();
    assert_eq!(updated.url, "https://hooks.example/changed");
    assert_eq!(updated.events, vec![webhook::REPORT_UPDATED]);
    assert_eq!(updated.template.as_deref(), Some("{{ payload }}"));

    let disabled = post_form(
        &app,
        &format!("/web/admin/webhooks/{}/op", hook.id),
        &cookie,
        &[("csrf", &csrf), ("op", "disable")],
    )
    .await;
    assert_eq!(disabled.status, StatusCode::SEE_OTHER);
    assert!(
        !webhook::find_by_id(&pool, hook.id)
            .await
            .unwrap()
            .unwrap()
            .enabled
    );

    let rotated = post_form(
        &app,
        &format!("/web/admin/webhooks/{}/op", hook.id),
        &cookie,
        &[("csrf", &csrf), ("op", "rotate")],
    )
    .await;
    assert_eq!(rotated.status, StatusCode::SEE_OTHER);
    let rotated_secret = webhook::find_by_id(&pool, hook.id)
        .await
        .unwrap()
        .unwrap()
        .secret;
    assert_ne!(rotated_secret, original_secret);
    let reveal = get(
        &app,
        rotated
            .location
            .as_deref()
            .expect("rotation reveal redirect"),
        Some(&cookie),
    )
    .await;
    assert!(reveal.body.contains(&rotated_secret));
    let consumed = get(&app, "/admin/webhooks", Some(&cookie)).await;
    assert!(!consumed.body.contains(&rotated_secret));

    let deleted = post_form(
        &app,
        &format!("/web/admin/webhooks/{}/delete", hook.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert!(webhook::list(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_webhook_events_require_matching_permissions(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let hook_role = role::create(
        &pool,
        "Webhook manager",
        "#444444",
        20,
        role::permission::MANAGE_WEBHOOKS,
        true,
    )
    .await
    .unwrap();
    role::assign_to_account(&pool, alice.id, Some(hook_role.id))
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/webhooks", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = csrf_of(&page.body);
    let created = post_form(
        &app,
        "/web/admin/webhooks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("url", "https://hooks.example/plamenu"),
            ("account_created", "1"),
            ("template", ""),
        ],
    )
    .await;

    assert_eq!(created.status, StatusCode::SEE_OTHER);
    assert_eq!(
        created.location.as_deref(),
        Some("/admin/webhooks?flash=error")
    );
    assert!(webhook::list(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_instance_settings(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/settings", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Server name"));
    let csrf = csrf_of(&page.body);

    // The contact must name a real local account (Mastodon's
    // `existing_username` validation).
    let bad = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("site_title", "Testburg"),
            ("site_contact_username", "nobody"),
        ],
    )
    .await;
    assert_eq!(bad.status, StatusCode::SEE_OTHER);
    assert_eq!(
        bad.location.as_deref(),
        Some("/admin/settings?flash=no_such_account")
    );
    let unchanged = plamenu_db::instance_settings::get(&pool).await.unwrap();
    assert_eq!(unchanged.site_title, "Plamenu");

    // Submitting without the rate-limit counts (present in the real form)
    // is rejected wholesale.
    let incomplete = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[("csrf", &csrf), ("site_title", "Testburg")],
    )
    .await;
    assert_eq!(
        incomplete.location.as_deref(),
        Some("/admin/settings?flash=error")
    );

    let saved = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("site_title", "Testburg"),
            ("site_short_description", "A cozy test box"),
            ("site_extended_description", "## Welcome\n\nBe kind."),
            ("site_contact_username", "@alice"),
            ("site_contact_email", "admin@testburg.example"),
            ("custom_css", ".column { border: 0 }"),
            ("rate_limiting_enabled", "1"),
            ("rate_limit_authenticated_api", "1500"),
            ("rate_limit_per_token_api", "300"),
            ("rate_limit_unauthenticated_api", "300"),
            ("rate_limit_api_media", "30"),
            ("rate_limit_api_delete", "30"),
            ("rate_limit_api_sign_up", "5"),
            ("rate_limit_app_registrations", "7"),
            ("rate_limit_paging", "300"),
            ("rate_limit_login_attempts", "25"),
            ("rate_limit_password_resets", "5"),
            ("rate_limit_sign_up_web", "25"),
            ("max_characters", "1000"),
            ("max_characters_long_form", "80000"),
            ("max_media_attachments", "6"),
            ("poll_max_options", "5"),
            ("media_full_processing", "avif"),
            ("media_preview_processing", "jpeg"),
            ("media_remote_full_processing", "jpeg"),
            ("media_cached_image_processing", "passthrough"),
            ("remote_video_max_mb", "512"),
            ("remote_video_max_height", "480"),
            // Settings-owned retention values (0013): the real form always
            // submits them, and an empty value is a wholesale rejection.
            ("media_cache_retention_days", "21"),
            ("ip_retention_days", "180"),
            ("translation_cache_retention_days", "14"),
            ("translation_cache_max_rows", "50000"),
            ("translation_backend_concurrency", "3"),
            ("translation_user_rate_limit_per_hour", "45"),
            ("translation_refresh_on_provider_change", "1"),
            ("timeline_preview_federated", "1"),
            ("public_search", "1"),
            ("anon_trends", "1"),
            ("anon_directory_federated", "1"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    assert_eq!(
        saved.location.as_deref(),
        Some("/admin/settings?flash=applied")
    );
    let settings = plamenu_db::instance_settings::get(&pool).await.unwrap();
    assert_eq!(settings.site_title, "Testburg");
    // The leading @ is shed before storing, like Mastodon's presenter expects.
    assert_eq!(settings.site_contact_username, "alice");
    assert!(settings.rate_limiting_enabled);
    assert_eq!(settings.rate_limit_app_registrations, 7);
    assert_eq!(settings.max_characters, 1000);
    assert_eq!(settings.max_characters_long_form, 80_000);
    assert_eq!(settings.max_media_attachments, 6);
    assert_eq!(settings.poll_max_options, 5);
    assert_eq!(settings.media_full_processing, "avif");
    assert_eq!(settings.media_preview_processing, "jpeg");
    assert_eq!(settings.media_remote_full_processing, "jpeg");
    assert_eq!(settings.media_cached_image_processing, "passthrough");
    assert_eq!(settings.remote_video_max_mb, 512);
    assert_eq!(settings.remote_video_max_height, 480);
    assert_eq!(settings.media_cache_retention_days, 21);
    assert_eq!(settings.ip_retention_days, 180);
    assert_eq!(settings.translation_cache_retention_days, 14);
    assert_eq!(settings.translation_cache_max_rows, 50000);
    assert_eq!(settings.translation_backend_concurrency, 3);
    assert_eq!(settings.translation_user_rate_limit_per_hour, 45);
    assert!(settings.translation_refresh_on_provider_change);
    // Checkbox semantics: submitted boxes turn on, absent ones turn off.
    assert!(settings.timeline_preview_federated);
    assert!(!settings.timeline_preview_local);
    assert!(!settings.timeline_preview_tag);
    assert!(settings.public_search);
    assert!(settings.anon_trends);
    assert!(!settings.anon_directory);
    assert!(settings.anon_directory_federated);
    assert!(!settings.anon_groups);
    assert!(!settings.emit_integrity_proofs);
    assert!(!settings.emit_rfc9421);

    // The saved values round-trip into the form and the public stylesheet.
    let page = get(&app, "/admin/settings", Some(&cookie)).await;
    assert!(page.body.contains("Testburg"));
    // The media-processing selects reflect the saved choice.
    assert!(page.body.contains("Image processing"));
    let css = get(&app, "/custom.css", None).await;
    assert_eq!(css.status, StatusCode::OK);
    assert!(css.body.contains(".column { border: 0 }"));
}

/// The settings page is split into collapsible groups, each its own form. A
/// submit must write only its own group — in particular the checkboxes it
/// does not carry (absent = off, within the group) must not zero out the
/// switches of every other group.
#[sqlx::test(migrations = "../db/migrations")]
async fn admin_settings_sections_save_independently(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/settings", Some(&cookie)).await;
    let csrf = csrf_of(&page.body);
    assert!(page.body.contains(r#"id="sec-custom-emoji""#));
    assert!(page.body.contains(">Personal emoji per member<"));
    assert!(page.body.contains(">Largest custom emoji file (KiB)<"));
    assert!(
        page.body.contains(r#"name="public_timeline_replies""#),
        "the reply-policy knob must render, not just persist"
    );
    let before = plamenu_db::instance_settings::get(&pool).await.unwrap();

    let custom_emoji = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("section", "custom-emoji"),
            ("personal_emoji_limit", "75"),
            ("custom_emoji_max_file_size_kb", "512"),
        ],
    )
    .await;
    assert_eq!(
        custom_emoji.location.as_deref(),
        Some("/admin/settings?flash=applied&open=custom-emoji")
    );
    let emoji_settings = plamenu_db::custom_emoji::settings(&pool).await.unwrap();
    assert_eq!(emoji_settings.personal_limit, 75);
    assert_eq!(emoji_settings.max_file_size_kb, 512);

    let anonymous = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("section", "anonymous"),
            ("timeline_preview_local", "1"),
            ("public_search", "1"),
        ],
    )
    .await;
    // The redirect re-expands the group it wrote.
    assert_eq!(
        anonymous.location.as_deref(),
        Some("/admin/settings?flash=applied&open=anonymous")
    );

    let discovery = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("section", "discovery"),
            ("trends_enabled", "1"),
            ("peers_api_enabled", "1"),
            ("public_timeline_replies", "1"),
            ("show_domain_blocks", "all"),
        ],
    )
    .await;
    assert_eq!(
        discovery.location.as_deref(),
        Some("/admin/settings?flash=applied&open=discovery")
    );

    // The boost-collapse group, whose window is a number rather than a
    // switch — a section that carries one of each.
    let timelines = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("section", "timelines"),
            ("boost_collapse", "1"),
            ("boost_collapse_lookback", "80"),
        ],
    )
    .await;
    assert_eq!(
        timelines.location.as_deref(),
        Some("/admin/settings?flash=applied&open=timelines")
    );

    // Neither group carried the other's checkboxes, and both survived.
    let settings = plamenu_db::instance_settings::get(&pool).await.unwrap();
    assert!(settings.timeline_preview_local);
    assert!(settings.public_search);
    assert!(settings.trends_enabled);
    assert!(settings.peers_api_enabled);
    // A new setting costs three coordinated edits (form field, base_update,
    // section arm) or it silently never persists — this is the tripwire.
    assert!(settings.public_timeline_replies);
    assert!(settings.boost_collapse);
    assert_eq!(settings.boost_collapse_lookback, 80);
    assert_eq!(settings.show_domain_blocks, "all");

    // A branding submit carries no switches and no numbers at all; every
    // other group keeps what it had.
    let branding = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("section", "branding"),
            ("site_title", "Sectionville"),
        ],
    )
    .await;
    assert_eq!(
        branding.location.as_deref(),
        Some("/admin/settings?flash=applied&open=branding")
    );
    let settings = plamenu_db::instance_settings::get(&pool).await.unwrap();
    assert_eq!(settings.site_title, "Sectionville");
    assert!(settings.timeline_preview_local);
    assert!(settings.trends_enabled);
    assert_eq!(settings.boost_collapse_lookback, 80);
    assert_eq!(
        settings.rate_limit_authenticated_api,
        before.rate_limit_authenticated_api
    );
    assert_eq!(settings.max_characters, before.max_characters);
    assert_eq!(settings.ip_retention_days, before.ip_retention_days);

    // A group's own validation still applies, and points back at the group:
    // scrubbing the sign-in log after 0 days would erase it wholesale.
    let bad = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("section", "retention"),
            ("media_cache_retention_days", "7"),
            ("remote_video_max_mb", "0"),
            ("remote_video_max_height", "0"),
            ("ip_retention_days", "0"),
        ],
    )
    .await;
    assert_eq!(
        bad.location.as_deref(),
        Some("/admin/settings?flash=error&open=retention")
    );
    let settings = plamenu_db::instance_settings::get(&pool).await.unwrap();
    assert_eq!(
        settings.media_cache_retention_days,
        before.media_cache_retention_days
    );

    // A section name nothing claims is refused rather than applied as
    // "everything".
    let bogus = post_form(
        &app,
        "/web/admin/settings",
        &cookie,
        &[
            ("csrf", &csrf),
            ("section", "nonsense"),
            ("site_title", "Hijacked"),
        ],
    )
    .await;
    assert_eq!(
        bogus.location.as_deref(),
        Some("/admin/settings?flash=error")
    );
    let settings = plamenu_db::instance_settings::get(&pool).await.unwrap();
    assert_eq!(settings.site_title, "Sectionville");

    // Groups are collapsed by default; `open` expands the one just written.
    let page = get(&app, "/admin/settings", Some(&cookie)).await;
    assert!(page.body.contains(r#"id="sec-federation""#));
    assert!(!page.body.contains(r#"id="sec-federation" open"#));
    let page = get(&app, "/admin/settings?open=federation", Some(&cookie)).await;
    assert!(page.body.contains(r#"id="sec-federation" open"#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_site_uploads(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let state = common::test_state_with(pool.clone(), std::sync::Arc::default());
    let app = build_router(state.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/settings", Some(&cookie)).await;
    assert!(page.body.contains("Site images"));
    let csrf = csrf_of(&page.body);

    // No favicon uploaded yet: the probe serves the embedded brand-mark
    // default (every page links `rel=icon`, so the route never 404s).
    let default_bytes = favicon_bytes(&app).await;
    assert!(default_bytes.starts_with(b"\x89PNG"));

    let png = sample_png_bytes();
    let uploaded = post_multipart_file(
        &app,
        "/web/admin/site-uploads/favicon",
        &cookie,
        &[("csrf", &csrf)],
        ("image", "favicon.png", "image/png", &png),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::SEE_OTHER);
    // The redirect re-expands the Site images group it wrote.
    assert_eq!(
        uploaded.location.as_deref(),
        Some("/admin/settings?flash=applied&open=site-images")
    );
    let favicon = plamenu_db::site_upload::get(&pool, "favicon")
        .await
        .unwrap()
        .expect("favicon stored");
    let variants = plamenu_db::site_upload::variants_for(&pool, "favicon")
        .await
        .unwrap();
    // Mastodon's FAVICON_SIZES: 16/32/48 renditions.
    assert_eq!(
        variants.iter().map(|v| v.width).collect::<Vec<_>>(),
        vec![16, 32, 48]
    );
    // The route now serves the upload's largest rendition, not the default.
    let served = favicon_bytes(&app).await;
    let largest = state
        .media
        .get(&variants.last().unwrap().file_name)
        .await
        .unwrap();
    assert_eq!(served, largest);
    assert_ne!(served, default_bytes);

    // A mascot upload shows up on the sign-in page.
    let uploaded = post_multipart_file(
        &app,
        "/web/admin/site-uploads/mascot",
        &cookie,
        &[("csrf", &csrf)],
        ("image", "mascot.png", "image/png", &png),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::SEE_OTHER);
    let mascot = plamenu_db::site_upload::get(&pool, "mascot")
        .await
        .unwrap()
        .expect("mascot stored");
    let login_page = get(&app, "/login", None).await;
    assert!(login_page.body.contains(&mascot.file_name));

    // An unknown slot is refused.
    let bogus = post_multipart_file(
        &app,
        "/web/admin/site-uploads/banner",
        &cookie,
        &[("csrf", &csrf)],
        ("image", "x.png", "image/png", &png),
    )
    .await;
    assert_eq!(bogus.status, StatusCode::NOT_FOUND);

    // Removing the favicon deletes the stored files.
    let variant_files: Vec<String> = variants.iter().map(|v| v.file_name.clone()).collect();
    let deleted = post_form(
        &app,
        "/web/admin/site-uploads/favicon/delete",
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::site_upload::get(&pool, "favicon")
            .await
            .unwrap()
            .is_none()
    );
    assert!(state.media.get(&favicon.file_name).await.is_err());
    for file in variant_files {
        assert!(state.media.get(&file).await.is_err());
    }
    // With the upload gone, the route falls back to the embedded default.
    assert_eq!(favicon_bytes(&app).await, default_bytes);
}

/// Fetches `/favicon.ico` raw — the body is a PNG, which the UTF-8 `get`
/// helper can't carry.
async fn png_bytes(app: &Router, uri: &str) -> Vec<u8> {
    let response = tower::ServiceExt::oneshot(
        app.clone(),
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE].to_str().unwrap(),
        "image/png"
    );
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn favicon_bytes(app: &Router) -> Vec<u8> {
    png_bytes(app, "/favicon.ico").await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_relays(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let state = common::test_state_with(pool.clone(), std::sync::Arc::default());
    let app = build_router(state.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/relays", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Add a relay"));
    let csrf = csrf_of(&page.body);

    // A non-https inbox is refused.
    let bad = post_form(
        &app,
        "/web/admin/relays",
        &cookie,
        &[("csrf", &csrf), ("inbox_url", "http://relay.example/inbox")],
    )
    .await;
    assert_eq!(bad.location.as_deref(), Some("/admin/relays?flash=error"));

    // Creating subscribes immediately (Mastodon calls enable! after save).
    let created = post_form(
        &app,
        "/web/admin/relays",
        &cookie,
        &[
            ("csrf", &csrf),
            ("inbox_url", "https://relay.example/inbox"),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    assert_eq!(
        created.location.as_deref(),
        Some("/admin/relays?flash=applied")
    );
    let relays = plamenu_db::relay::list(&pool).await.unwrap();
    assert_eq!(relays.len(), 1);
    assert_eq!(relays[0].state, "pending");
    assert!(relays[0].follow_activity_id.is_some());

    let dup = post_form(
        &app,
        "/web/admin/relays",
        &cookie,
        &[
            ("csrf", &csrf),
            ("inbox_url", "https://relay.example/inbox"),
        ],
    )
    .await;
    assert_eq!(
        dup.location.as_deref(),
        Some("/admin/relays?flash=duplicate")
    );

    let page = get(&app, "/admin/relays", Some(&cookie)).await;
    assert!(page.body.contains("https://relay.example/inbox"));
    assert!(page.body.contains("Waiting for approval"));

    let disabled = post_form(
        &app,
        &format!("/web/admin/relays/{}/op", relays[0].id),
        &cookie,
        &[("csrf", &csrf), ("op", "disable")],
    )
    .await;
    assert_eq!(disabled.status, StatusCode::SEE_OTHER);
    let relay = plamenu_db::relay::find(&pool, relays[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(relay.state, "idle");
    assert!(relay.follow_activity_id.is_none());

    let deleted = post_form(
        &app,
        &format!("/web/admin/relays/{}/delete", relays[0].id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert!(plamenu_db::relay::list(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn composer_quote_policy_selector_posts_and_follows_the_preference(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    // The composer offers the selector, defaulting to the (public) preference.
    let compose_page = get(&app, "/compose", Some(&cookie)).await;
    assert!(compose_page.body.contains(r#"name="quote_policy""#));
    assert!(
        compose_page
            .body
            .contains(r#"<option value="public" selected>"#)
    );

    // An explicit selection is stored on the post.
    let csrf = csrf_of(&compose_page.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "followers may quote this"),
            ("visibility", "public"),
            ("quote_policy", "followers"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let status_id = posted
        .location
        .expect("redirect to the new post")
        .rsplit('/')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let stored = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert_eq!(
        stored.quote_approval_policy,
        plamenu_ap::quote_policy::AUTOMATIC_FOLLOWERS
    );

    // Saving a "nobody" default in the preferences feeds the composer.
    let pcsrf = csrf_of(&get(&app, "/settings/preferences", Some(&cookie)).await.body);
    let saved = post_form(
        &app,
        "/web/settings/preferences",
        &cookie,
        &[("csrf", &pcsrf), ("posting_default_quote_policy", "nobody")],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    let preferences = get(&app, "/settings/preferences", Some(&cookie)).await;
    assert!(
        preferences
            .body
            .contains(r#"<option value="nobody" selected>"#)
    );
    let compose_page = get(&app, "/compose", Some(&cookie)).await;
    assert!(
        compose_page
            .body
            .contains(r#"<option value="nobody" selected>"#)
    );

    // A param-less quick post from the timeline uses the preference.
    let csrf = csrf_of(&compose_page.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "nobody may quote this"),
            ("visibility", "public"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let status_id = posted
        .location
        .expect("redirect")
        .rsplit('/')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let stored = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert_eq!(stored.quote_approval_policy, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn composer_language_selector_follows_the_enabled_set(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    // With no restriction stored, the composer offers the full inventory as a
    // proper-name select, with the (en) preference selected.
    let compose_page = get(&app, "/compose", Some(&cookie)).await;
    assert!(compose_page.body.contains(r#"name="language""#));
    assert!(
        compose_page
            .body
            .contains(r#"<option value="en" selected>English</option>"#)
    );
    assert!(
        compose_page
            .body
            .contains(r#"<option value="tok">toki pona (Toki Pona)</option>"#)
    );

    // Enabling a subset on the new settings page narrows the selector to it.
    let lang_page = get(&app, "/settings/languages", Some(&cookie)).await;
    let csrf = csrf_of(&lang_page.body);
    let saved = post_form(
        &app,
        "/web/settings/languages",
        &cookie,
        &[
            ("csrf", &csrf),
            ("languages[]", "en"),
            ("languages[]", "fr"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    assert_eq!(
        saved.location.as_deref(),
        Some("/settings/languages?saved=1")
    );

    let compose_page = get(&app, "/compose", Some(&cookie)).await;
    assert!(
        compose_page
            .body
            .contains(r#"<option value="en" selected>English</option>"#)
    );
    assert!(
        compose_page
            .body
            .contains(r#"<option value="fr">Français (French)</option>"#)
    );
    assert!(!compose_page.body.contains(r#"<option value="de""#));

    // The checklist reloads with the stored set checked.
    let lang_page = get(&app, "/settings/languages", Some(&cookie)).await;
    assert!(lang_page.body.contains(r#"value="fr" checked"#));
    assert!(!lang_page.body.contains(r#"value="de" checked"#));

    // A default posting language outside the enabled set is still offered
    // (prepended), so submitting the form untouched never changes it.
    let pcsrf = csrf_of(&get(&app, "/settings/languages", Some(&cookie)).await.body);
    let saved = post_form(
        &app,
        "/web/settings/languages",
        &cookie,
        &[
            ("csrf", &pcsrf),
            ("posting_default_language", "de"),
            ("languages[]", "en"),
            ("languages[]", "fr"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    let compose_page = get(&app, "/compose", Some(&cookie)).await;
    assert!(
        compose_page
            .body
            .contains(r#"<option value="de" selected>Deutsch (German)</option>"#)
    );

    // Deselecting everything clears the restriction: full inventory again.
    let csrf = csrf_of(&get(&app, "/settings/languages", Some(&cookie)).await.body);
    let saved = post_form(&app, "/web/settings/languages", &cookie, &[("csrf", &csrf)]).await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    let compose_page = get(&app, "/compose", Some(&cookie)).await;
    assert!(compose_page.body.contains(r#"<option value="tok">"#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn language_forms_reject_unknown_codes(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    // Both forms are selects/checkboxes over the inventory, so an unknown
    // code can only be a tampered request.
    let csrf = csrf_of(&get(&app, "/settings/languages", Some(&cookie)).await.body);
    let resp = post_form(
        &app,
        "/web/settings/languages",
        &cookie,
        &[("csrf", &csrf), ("languages[]", "xx")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);

    let resp = post_form(
        &app,
        "/web/settings/languages",
        &cookie,
        &[("csrf", &csrf), ("posting_default_language", "xx")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);

    // An interface language with no catalog is refused the same way: the
    // picker only offers the locales Plamenu is actually translated into.
    let resp = post_form(
        &app,
        "/web/settings/languages",
        &cookie,
        &[("csrf", &csrf), ("locale", "fr")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);

    // Neither rejection stored anything.
    let account_id = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    let settings = user::settings_by_account_id(&pool, account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settings.default_language(), Some("en"));
    let compose_page = get(&app, "/compose", Some(&cookie)).await;
    assert!(compose_page.body.contains(r#"<option value="tok">"#));
}

// ---- Emoji reaction chips -------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn unicode_reaction_catalog_is_complete_lazy_and_immutable(pool: PgPool) {
    let app = common::test_app(pool);
    let catalog = get(&app, "/assets/emoji-17.0.json", None).await;
    assert_eq!(catalog.status, StatusCode::OK);
    assert_eq!(catalog.content_type, "application/json; charset=utf-8");
    assert!(catalog.cache_control.contains("immutable"));
    let catalog_body = catalog.body;
    let parsed: serde_json::Value = serde_json::from_str(&catalog_body).unwrap();
    assert_eq!(parsed["version"], "17.0");
    let count: usize = parsed["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["subgroups"].as_array().unwrap())
        .map(|subgroup| subgroup["emoji"].as_array().unwrap().len())
        .sum();
    assert_eq!(count, 3_953, "the full Unicode 17.0 RGI set");
    assert!(catalog_body.contains("distorted face"));

    // app.js is part of every interactive page, so the full inventory must
    // not be embedded there or pre-cached by the install worker.
    let script = get(&app, "/assets/app.js", None).await;
    assert!(!script.body.contains("distorted face"));
    let worker = get(&app, "/sw.js", None).await;
    assert!(!worker.body.contains("emoji-17.0.json"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn full_reaction_picker_works_without_javascript_and_returns(pool: PgPool) {
    let categorized = custom_emoji::create_local(&pool, "party", "7.png", "image/png", 0, None)
        .await
        .unwrap();
    let categorized = categorized.unwrap();
    custom_emoji::update_local_flags(&pool, categorized.id, false, true, Some("Celebration"))
        .await
        .unwrap()
        .unwrap();
    custom_emoji::create_local(&pool, "wave", "8.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "pick any reaction").await;
    let status_id = permalink.rsplit('/').next().unwrap();

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(
        thread.body.contains(&format!(
            r#"href="/web/statuses/{status_id}/reaction?return_to=%2F%40alice%2F{status_id}""#
        )),
        "the action is a normal link carrying its return destination"
    );
    assert!(
        thread
            .body
            .contains(r#"data-unicode-catalog="/assets/emoji-17.0.json""#)
    );
    assert!(
        !thread.body.contains("distorted face") && !thread.body.contains("Celebration"),
        "ordinary status HTML carries neither full Unicode nor custom-picker data"
    );

    let picker_path =
        format!("/web/statuses/{status_id}/reaction?return_to=%2F%40alice%2F{status_id}");
    let picker = get(&app, &picker_path, Some(&cookie)).await;
    assert_eq!(picker.status, StatusCode::OK);
    assert!(picker.body.contains("Choose a reaction"));
    assert!(picker.body.contains("Smileys &amp; Emotion"));
    assert!(picker.body.contains("People &amp; Body"));
    assert!(picker.body.contains("Component"));
    assert!(picker.body.contains("Celebration"));
    assert!(picker.body.contains("Other custom emoji"));
    assert!(picker.body.contains(r#"value="party""#));
    assert!(picker.body.contains(r#"value="wave""#));
    assert!(picker.body.contains(r#"value="🫪""#));
    assert!(picker.body.contains(r#"title="distorted face""#));
    assert!(picker.body.contains(&format!(
        r#"form class="reaction-catalog" method="post" action="/web/statuses/{status_id}/react""#
    )));

    let reacted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/react"),
        &cookie,
        &[
            ("csrf", &csrf_of(&picker.body)),
            ("return_to", &permalink),
            ("emoji", "🫪"),
        ],
    )
    .await;
    assert_eq!(reacted.status, StatusCode::SEE_OTHER);
    assert_eq!(reacted.location.as_deref(), Some(permalink.as_str()));
    assert!(
        get(&app, &permalink, Some(&cookie))
            .await
            .body
            .contains(r#"data-reaction-name="🫪""#),
        "the selected Unicode 17 reaction was applied"
    );

    let unsafe_return = get(
        &app,
        &format!("/web/statuses/{status_id}/reaction?return_to=%2F%2Fevil.example"),
        Some(&cookie),
    )
    .await;
    assert!(unsafe_return.body.contains(r#"name="return_to" value="/""#));
    assert!(!unsafe_return.body.contains("evil.example"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reaction_chips_toggle_through_the_form(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "react to me").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    // A fresh post renders the chip row's container empty (the script's
    // swap anchor; `:empty` collapses it) — no chips, no toggle forms. The
    // signed-in page still carries the progressively enhanced picker link in
    // the action bar.
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains("status__reactions"));
    assert!(!thread.body.contains("reaction-form"));
    assert!(thread.body.contains("data-reaction-picker"));
    let csrf = csrf_of(&thread.body);

    // React with a Unicode emoji (🔥, percent-encoded in the path).
    let reacted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/react/%F0%9F%94%A5"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &permalink)],
    )
    .await;
    assert_eq!(reacted.status, StatusCode::SEE_OTHER);
    assert_eq!(reacted.location.as_deref(), Some(permalink.as_str()));

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains(r#"data-reaction-name="🔥""#));
    assert!(thread.body.contains(r#"class="reaction is-active""#));
    assert!(
        thread
            .body
            .contains(&format!("/web/statuses/{status_id}/unreact/🔥")),
        "an own chip flips to the unreact form"
    );

    // Un-react: the chip disappears again.
    let unreacted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/unreact/%F0%9F%94%A5"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &permalink)],
    )
    .await;
    assert_eq!(unreacted.status, StatusCode::SEE_OTHER);
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(!thread.body.contains("data-reaction-name"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn custom_emoji_reaction_chip_shows_the_image(pool: PgPool) {
    custom_emoji::create_local(&pool, "party", "7.png", "image/png", 0, None)
        .await
        .unwrap();
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "party time").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();
    let csrf = csrf_of(&get(&app, &permalink, Some(&cookie)).await.body);

    let reacted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/react/party"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &permalink)],
    )
    .await;
    assert_eq!(reacted.status, StatusCode::SEE_OTHER);

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(thread.body.contains(r#"data-reaction-name="party""#));
    assert!(
        thread.body.contains(r#"<img class="reaction__emoji""#) && thread.body.contains("7.png"),
        "a custom-emoji chip renders the emoji image"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_viewers_get_inert_reaction_chips(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "look, reactions").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();
    let csrf = csrf_of(&get(&app, &permalink, Some(&cookie)).await.body);
    post_form(
        &app,
        &format!("/web/statuses/{status_id}/react/%F0%9F%94%A5"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &permalink)],
    )
    .await;

    let anon = get(&app, &permalink, None).await;
    assert!(anon.body.contains(r#"data-reaction-name="🔥""#));
    assert!(
        !anon.body.contains(&format!("{status_id}/react")),
        "anonymous chips carry no toggle form"
    );
    assert!(!anon.body.contains("data-reaction-picker"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_custom_reactions_toggle_by_joining(pool: PgPool) {
    let bob = create_local_account(&pool, "bob", "Bob").await;
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "blobs welcome").await;
    let status_id: i64 = permalink.rsplit('/').next().unwrap().parse().unwrap();
    // A reaction whose custom-emoji image lives on another server renders
    // under its qualified `shortcode@host` name; the chip is a toggle form
    // that joins (+1s) the existing reaction, Pleroma-style.
    reaction::create(
        &pool,
        reaction::NewReaction {
            account_id: bob.id,
            status_id,
            name: "blob",
            custom_emoji_url: Some("https://remote.example/emoji/blob.png"),
            uri: None,
        },
    )
    .await
    .unwrap();

    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(
        thread
            .body
            .contains(r#"data-reaction-name="blob@remote.example""#)
    );
    assert!(
        thread
            .body
            .contains(&format!("{status_id}/react/blob@remote.example"))
    );

    let csrf = csrf_of(&thread.body);
    let resp = post_form(
        &app,
        &format!("/web/statuses/{status_id}/react/blob@remote.example"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &permalink)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let joined = get(&app, &permalink, Some(&cookie)).await;
    assert!(joined.body.contains(r#"class="reaction is-active""#));
    assert!(
        joined
            .body
            .contains(r#"<span class="reaction__count">2</span>"#)
    );
    assert!(
        joined
            .body
            .contains(&format!("{status_id}/unreact/blob@remote.example"))
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_page_prefills_source_and_saves(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "the first wording"),
            ("spoiler_text", "cw label"),
            ("visibility", "public"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let permalink = posted.location.expect("redirect to the new post");
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    // The edit page prefills the source text and the content warning.
    let page = get(
        &app,
        &format!("/web/statuses/{status_id}/edit"),
        Some(&cookie),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("the first wording"), "{}", page.body);
    assert!(page.body.contains(r#"value="cw label""#), "{}", page.body);
    assert!(page.body.contains("Save changes"));
    // Visibility is locked after posting: no visibility select in the form.
    assert!(!page.body.contains(r#"name="visibility""#));

    // Saving lands back on the thread with the new content, sans CW.
    let csrf = csrf_of(&page.body);
    let saved = post_form(
        &app,
        &format!("/web/statuses/{status_id}/edit"),
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "the corrected wording"),
            ("spoiler_text", ""),
            ("language", "en"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    assert_eq!(saved.location.as_deref(), Some(permalink.as_str()));
    let thread = get(&app, &permalink, Some(&cookie)).await;
    assert!(
        thread.body.contains("the corrected wording"),
        "{}",
        thread.body
    );
    assert!(!thread.body.contains("the first wording"));
    // The detail view's "Edited" marker links to the history page.
    assert!(
        thread.body.contains(&format!("{permalink}/history")),
        "edited marker should link to history: {}",
        thread.body
    );

    // The history page shows both versions, newest first.
    let history = get(&app, &format!("{permalink}/history"), Some(&cookie)).await;
    assert_eq!(history.status, StatusCode::OK);
    assert!(history.body.contains("Most recent"), "{}", history.body);
    assert!(history.body.contains("Original"));
    let newest = history.body.find("the corrected wording").unwrap();
    let original = history.body.find("the first wording").unwrap();
    assert!(newest < original, "newest version should render first");
    assert!(history.body.contains("cw label"), "{}", history.body);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_preview_renders_for_js_and_no_js_without_mutating_the_post(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "the durable original").await;
    let status_id: i64 = permalink.rsplit('/').next().unwrap().parse().unwrap();
    let edit_uri = format!("/web/statuses/{status_id}/edit");

    let page = get(&app, &edit_uri, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains(r#"name="op" value="preview""#),
        "edit page has no Preview button: {}",
        page.body
    );
    assert!(page.body.contains("data-compose-preview"));
    assert!(page.body.contains("data-compose-preview-urlencoded"));
    let csrf = csrf_of(&page.body);
    let fields = [
        ("csrf", csrf.as_str()),
        ("op", "preview"),
        ("status", "**bold corrected draft**"),
        ("spoiler_text", "draft warning"),
        ("content_type", "text/markdown"),
        ("language", "en"),
        ("quote_policy", "public"),
    ];

    // The enhanced path gets only the rendered card fragment.
    let fragment = edit_preview_fragment(&app, &edit_uri, &cookie, &fields).await;
    assert_eq!(fragment.status, StatusCode::OK, "{}", fragment.body);
    assert!(fragment.body.contains("compose__preview-heading"));
    assert!(
        fragment
            .body
            .contains("<strong>bold corrected draft</strong>"),
        "markdown not rendered in edit preview: {}",
        fragment.body
    );
    assert!(fragment.body.contains("draft warning"));
    assert!(!fragment.body.contains("data-compose"));
    assert!(!fragment.body.contains("<title"));

    // The same submit without the enhancement re-renders the complete edit
    // page with both the raw draft and the preview preserved.
    let no_js = post_form(&app, &edit_uri, &cookie, &fields).await;
    assert_eq!(no_js.status, StatusCode::OK, "{}", no_js.body);
    assert!(no_js.body.contains("data-compose"));
    assert!(no_js.body.contains("compose__preview-heading"));
    assert!(no_js.body.contains("<strong>bold corrected draft</strong>"));
    assert!(no_js.body.contains("**bold corrected draft**"));
    assert!(no_js.body.contains(r#"value="draft warning""#));

    // Validation failures stay on the edit surface and remain non-mutating.
    let invalid_fields = [
        ("csrf", csrf.as_str()),
        ("op", "preview"),
        ("status", ""),
        ("spoiler_text", ""),
        ("content_type", "text/plain"),
        ("language", "en"),
    ];
    let invalid = post_form(&app, &edit_uri, &cookie, &invalid_fields).await;
    assert_eq!(invalid.status, StatusCode::OK, "{}", invalid.body);
    assert!(
        invalid
            .body
            .contains("Validation failed: Text can't be blank"),
        "{}",
        invalid.body
    );
    assert!(invalid.body.contains("data-compose"));
    let invalid_fragment = edit_preview_fragment(&app, &edit_uri, &cookie, &invalid_fields).await;
    assert_eq!(invalid_fragment.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        invalid_fragment
            .body
            .contains("Validation failed: Text can't be blank")
    );
    assert!(!invalid_fragment.body.contains("data-compose"));

    // Preview is a true dry run: no row, source or edit-history mutation.
    let stored = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert!(stored.content.contains("the durable original"));
    assert_eq!(stored.edited_at, None);
    assert_eq!(
        status::source_of(&pool, status_id)
            .await
            .unwrap()
            .unwrap()
            .text,
        "the durable original"
    );
    assert!(
        plamenu_db::status_edit::for_status(&pool, status_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_updates_media_alt_text_and_drops_unkept(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let png = sample_png_bytes();
    let posted = post_multipart_file(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "with a picture"),
            ("visibility", "public"),
            ("media_alt[]", "old alt"),
        ],
        ("media[]", "square.png", "image/png", &png),
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let permalink = posted.location.expect("redirect to the new post");
    let status_id: i64 = permalink.rsplit('/').next().unwrap().parse().unwrap();
    let media_id = media::for_statuses(&pool, &[status_id])
        .await
        .unwrap()
        .remove(&status_id)
        .expect("an attachment")[0]
        .id;

    // The edit page lists the attachment with its current alt text.
    let edit_uri = format!("/web/statuses/{status_id}/edit");
    let page = get(&app, &edit_uri, Some(&cookie)).await;
    assert!(page.body.contains(r#"value="old alt""#), "{}", page.body);
    assert!(page.body.contains(r#"name="media_keep[]""#));

    // Preview overlays the pending alt text in memory without changing the
    // attachment row or marking the status edited.
    let csrf = csrf_of(&page.body);
    let keep = media_id.to_string();
    let alt_key = format!("media_alt_{media_id}");
    let previewed = post_form(
        &app,
        &edit_uri,
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "preview"),
            ("status", "with a picture"),
            ("spoiler_text", ""),
            ("content_type", "text/plain"),
            ("language", "en"),
            ("sensitive", "false"),
            ("media_keep[]", &keep),
            (&alt_key, "preview alt"),
        ],
    )
    .await;
    assert_eq!(previewed.status, StatusCode::OK, "{}", previewed.body);
    assert!(
        previewed.body.contains(r#"alt="preview alt""#),
        "pending alt text missing from preview: {}",
        previewed.body
    );
    let row = media::find_owned(&pool, media_id, alice_id(&pool).await)
        .await
        .unwrap()
        .expect("attachment remains");
    assert_eq!(row.description.as_deref(), Some("old alt"));
    assert!(
        status::find_by_id(&pool, status_id)
            .await
            .unwrap()
            .unwrap()
            .edited_at
            .is_none()
    );

    // Keeping the attachment with a new description updates the alt text
    // and marks the post edited.
    let csrf = csrf_of(&page.body);
    let saved = post_form(
        &app,
        &edit_uri,
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "with a picture"),
            ("spoiler_text", ""),
            ("language", "en"),
            ("sensitive", "false"),
            ("media_keep[]", &keep),
            (&alt_key, "new alt"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    let row = media::find_owned(&pool, media_id, alice_id(&pool).await)
        .await
        .unwrap()
        .expect("attachment kept");
    assert_eq!(row.description.as_deref(), Some("new alt"));
    assert_eq!(row.status_id, Some(status_id));
    let stored = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert!(stored.edited_at.is_some(), "alt change should mark an edit");

    // Unchecking the attachment removes it from the post.
    let page = get(&app, &edit_uri, Some(&cookie)).await;
    let csrf = csrf_of(&page.body);
    let saved = post_form(
        &app,
        &edit_uri,
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "with a picture"),
            ("spoiler_text", ""),
            ("language", "en"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    assert!(
        media::for_statuses(&pool, &[status_id])
            .await
            .unwrap()
            .remove(&status_id)
            .unwrap_or_default()
            .is_empty(),
        "unkept attachment should be detached"
    );
}

/// `@alice`'s account id, for db-level assertions.
async fn alice_id(pool: &PgPool) -> i64 {
    account::find_local_by_username(pool, "alice")
        .await
        .unwrap()
        .unwrap()
        .id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_is_owner_only(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let alice = login(&app).await;
    let permalink = compose(&app, &alice, "alice's own words").await;
    let status_id = permalink.rsplit('/').next().unwrap().to_owned();

    let bob = login_as(&app, "bob@example.com", PASSWORD).await;
    let page = get(&app, &format!("/web/statuses/{status_id}/edit"), Some(&bob)).await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);

    let csrf = csrf_of(&get(&app, "/", Some(&bob)).await.body);
    let posted = post_form(
        &app,
        &format!("/web/statuses/{status_id}/edit"),
        &bob,
        &[
            ("csrf", &csrf),
            ("status", "hijacked"),
            ("spoiler_text", ""),
            ("language", "en"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::NOT_FOUND);
    let thread = get(&app, &permalink, Some(&alice)).await;
    assert!(
        thread.body.contains("alice&#39;s own words") || thread.body.contains("alice's own words")
    );
}

// ---- Filters honored in the web UI ---------------------------------------

/// Creates an active filter for `account_id` matching the word "verboten".
async fn seed_filter(pool: &PgPool, account_id: i64, action: &str, context: &[&str]) {
    let context: Vec<String> = context.iter().map(|c| (*c).to_owned()).collect();
    plamenu_db::custom_filter::create(
        pool,
        account_id,
        "Bad words",
        action,
        &context,
        None,
        &[plamenu_db::custom_filter::NewKeyword {
            keyword: "verboten".to_owned(),
            whole_word: true,
        }],
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn warn_filter_collapses_matching_posts_in_feeds(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    seed_filter(&pool, alice.id, "warn", &["public"]).await;
    let app = common::test_app(pool);
    let bob = login_as(&app, "bob@example.com", PASSWORD).await;
    compose(&app, &bob, "verboten wares for sale").await;

    let alice_cookie = login(&app).await;
    let page = get(&app, "/public", Some(&alice_cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Filtered: Bad words"), "warn bar shown");
    assert!(page.body.contains("Show anyway"));
    assert!(
        page.body.contains("verboten wares for sale"),
        "the collapsed body is still reachable without JS"
    );

    // Bob's own filter-free view is untouched.
    let bob_view = get(&app, "/public", Some(&bob)).await;
    assert!(!bob_view.body.contains("Filtered:"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hide_filter_drops_matching_posts_from_feeds(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    seed_filter(&pool, alice.id, "hide", &["public"]).await;
    let app = common::test_app(pool);
    let bob = login_as(&app, "bob@example.com", PASSWORD).await;
    compose(&app, &bob, "verboten wares for sale").await;
    compose(&app, &bob, "an innocent post").await;

    let alice_cookie = login(&app).await;
    let page = get(&app, "/public", Some(&alice_cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("verboten"), "hidden without a trace");
    assert!(!page.body.contains("Filtered:"));
    assert!(
        page.body.contains("an innocent post"),
        "others still render"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hide_downgrades_to_the_warn_bar_on_the_focused_post(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    seed_filter(&pool, alice.id, "hide", &["thread"]).await;
    let app = common::test_app(pool);
    let bob = login_as(&app, "bob@example.com", PASSWORD).await;
    let permalink = compose(&app, &bob, "verboten wares for sale").await;

    // The viewer deliberately opened the post's own page: it collapses
    // behind the bar instead of vanishing.
    let alice_cookie = login(&app).await;
    let page = get(&app, &permalink, Some(&alice_cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Filtered: Bad words"));
    assert!(page.body.contains("verboten wares for sale"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hide_filtered_status_drops_its_whole_notification(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    seed_filter(&pool, alice.id, "hide", &["notifications"]).await;
    let app = common::test_app(pool.clone());
    let bob_cookie = login_as(&app, "bob@example.com", PASSWORD).await;
    let permalink = compose(&app, &bob_cookie, "verboten wares for sale").await;
    let status_id: i64 = permalink.rsplit('/').next().unwrap().parse().unwrap();
    let bob = account::find_local_by_username(&pool, "bob")
        .await
        .unwrap()
        .unwrap();
    plamenu_db::notification::create(&pool, alice.id, bob.id, "mention", Some(status_id))
        .await
        .unwrap();
    plamenu_db::notification::create(&pool, alice.id, bob.id, "follow", None)
        .await
        .unwrap();

    let alice_cookie = login(&app).await;
    let page = get(&app, "/notifications", Some(&alice_cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("mentioned you"), "notification dropped");
    assert!(!page.body.contains("verboten"));
    assert!(page.body.contains("followed you"), "others still render");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_custom_roles(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());

    // Ordinary users are shut out.
    let bob_cookie = login_as(&app, "bob@example.com", PASSWORD).await;
    let denied = get(&app, "/admin/roles", Some(&bob_cookie)).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    let cookie = login(&app).await;
    let page = get(&app, "/admin/roles", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Owner"));
    assert!(page.body.contains("Moderator"));
    let csrf = csrf_of(&page.body);

    // Create a role with a subset of permissions.
    let resp = post_form(
        &app,
        "/web/admin/roles",
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "Helper"),
            ("color", "#336699"),
            ("position", "5"),
            ("manage_reports", "1"),
            ("view_dashboard", "1"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let helper = role::find_by_name(&pool, "Helper").await.unwrap().unwrap();
    assert!(helper.can(role::permission::MANAGE_REPORTS));
    assert!(!helper.can(role::permission::MANAGE_USERS));
    assert!(!helper.highlighted);

    // The edit page renders the form; updating rewrites the bitmask.
    let edit = get(&app, &format!("/admin/roles/{}", helper.id), Some(&cookie)).await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(edit.body.contains("Helper"));
    let resp = post_form(
        &app,
        &format!("/web/admin/roles/{}/update", helper.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "Support"),
            ("position", "5"),
            ("manage_reports", "1"),
            ("manage_users", "1"),
            ("highlighted", "1"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let support = role::find_by_id(&pool, helper.id).await.unwrap().unwrap();
    assert_eq!(support.name, "Support");
    assert!(support.can(role::permission::MANAGE_USERS));
    assert!(support.highlighted);

    // Positioning a role above your own is refused (Mastodon's elevation
    // guard) — Owner sits at 100.
    let resp = post_form(
        &app,
        &format!("/web/admin/roles/{}/update", helper.id),
        &cookie,
        &[("csrf", &csrf), ("name", "Support"), ("position", "999")],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("error"));
    assert_eq!(
        role::find_by_id(&pool, helper.id)
            .await
            .unwrap()
            .unwrap()
            .position,
        5
    );

    // The Owner role isn't outranked by its own holder, so it can't be
    // edited or deleted — the page degrades to a read-only summary.
    let owner = role::find_by_name(&pool, "Owner").await.unwrap().unwrap();
    let own = get(&app, &format!("/admin/roles/{}", owner.id), Some(&cookie)).await;
    assert!(own.body.contains("cannot"));
    let resp = post_form(
        &app,
        &format!("/web/admin/roles/{}/delete", owner.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("error"));
    assert!(role::find_by_id(&pool, owner.id).await.unwrap().is_some());

    // Deleting the custom role works and the whole lifecycle was logged.
    let resp = post_form(
        &app,
        &format!("/web/admin/roles/{}/delete", helper.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(role::find_by_id(&pool, helper.id).await.unwrap().is_none());
    let log = admin_action_log::list(
        &pool,
        &admin_action_log::LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let role_lines: Vec<_> = log
        .iter()
        .filter(|l| l.target_type == "UserRole")
        .map(|l| l.action.as_str())
        .collect();
    assert_eq!(role_lines, ["destroy", "update", "create"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_warning_presets_fill_moderation_actions(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Create a preset through the CRUD page.
    let page = get(&app, "/admin/warning-presets", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No warning presets yet."));
    let csrf = csrf_of(&page.body);
    let resp = post_form(
        &app,
        "/web/admin/warning-presets",
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "Spam"),
            ("text", "Please stop posting spam."),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let presets = warning_preset::list(&pool).await.unwrap();
    assert_eq!(presets.len(), 1);

    // The account action form offers it, and picking it with an empty text
    // field stamps the preset text onto the strike.
    let show = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert!(show.body.contains("Spam"));
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/action", bob.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("type", "none"),
            ("text", ""),
            ("preset", &presets[0].id.to_string()),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let strikes = account_warning::for_target(&pool, bob.id).await.unwrap();
    assert_eq!(strikes[0].text, "Please stop posting spam.");

    // Update and delete round-trip.
    let resp = post_form(
        &app,
        &format!("/web/admin/warning-presets/{}/update", presets[0].id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "Spam"),
            ("text", "Final warning."),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(
        warning_preset::find_by_id(&pool, presets[0].id)
            .await
            .unwrap()
            .unwrap()
            .text,
        "Final warning."
    );
    let resp = post_form(
        &app,
        &format!("/web/admin/warning-presets/{}/delete", presets[0].id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(warning_preset::list(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_manages_username_blocklist(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/admin/username-blocks", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No usernames are reserved."));
    let csrf = csrf_of(&page.body);

    let resp = post_form(
        &app,
        "/web/admin/username-blocks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("username", "admin"),
            ("comparison", "equals"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        username_block::matches(&pool, "4dm1n", false)
            .await
            .unwrap()
    );

    // Duplicates are refused (unique per username).
    let resp = post_form(
        &app,
        "/web/admin/username-blocks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("username", "Admin"),
            ("comparison", "equals"),
        ],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("error"));

    // Flip it to allow-with-approval via the row form.
    let block = &username_block::list(&pool).await.unwrap()[0];
    let resp = post_form(
        &app,
        &format!("/web/admin/username-blocks/{}/update", block.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("username", "admin"),
            ("comparison", "equals"),
            ("allow_with_approval", "1"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        !username_block::matches(&pool, "admin", false)
            .await
            .unwrap()
    );
    assert!(username_block::matches(&pool, "admin", true).await.unwrap());

    // Delete lifts the reservation; the lifecycle is audit-logged.
    let resp = post_form(
        &app,
        &format!("/web/admin/username-blocks/{}/delete", block.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(username_block::list(&pool).await.unwrap().is_empty());
    let log = admin_action_log::list(
        &pool,
        &admin_action_log::LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let lines: Vec<_> = log
        .iter()
        .filter(|l| l.target_type == "UsernameBlock")
        .map(|l| l.action.as_str())
        .collect();
    assert_eq!(lines, ["destroy", "update", "create"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn terms_of_service_editor_publishes(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Nothing published: the public page says so and the API 404s.
    let public = get(&app, "/terms-of-service", None).await;
    assert_eq!(public.status, StatusCode::OK);
    assert!(public.body.contains("has not published"));
    let api = get(&app, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(api.status, StatusCode::NOT_FOUND);

    // Save a draft first — still nothing public.
    let editor = get(&app, "/admin/terms-of-service", Some(&cookie)).await;
    assert_eq!(editor.status, StatusCode::OK);
    let csrf = csrf_of(&editor.body);
    let resp = post_form(
        &app,
        "/web/admin/terms-of-service",
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "save"),
            ("text", "# Terms\n\nBe kind on %{domain}."),
            ("changelog", "First version"),
            ("effective_date", ""),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(terms_of_service::draft(&pool).await.unwrap().is_some());
    assert!(terms_of_service::current(&pool).await.unwrap().is_none());
    // The editor re-opens the draft.
    let editor = get(&app, "/admin/terms-of-service", Some(&cookie)).await;
    assert!(editor.body.contains("Be kind on"));
    assert!(editor.body.contains("First version"));

    // Publish. The public page and API now serve the rendered Markdown with
    // the domain substituted.
    let resp = post_form(
        &app,
        "/web/admin/terms-of-service",
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "publish"),
            ("text", "# Terms\n\nBe kind on %{domain}."),
            ("changelog", "First version"),
            ("effective_date", ""),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let public = get(&app, "/terms-of-service", None).await;
    assert_eq!(public.status, StatusCode::OK);
    assert!(public.body.contains("<h1>Terms</h1>"));
    assert!(public.body.contains("Be kind on plamenu.test."));
    let api = get(&app, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(api.status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&api.body).unwrap();
    assert_eq!(json["effective"], true);
    assert!(json["succeeded_by"].is_null());
    assert!(
        json["content"]
            .as_str()
            .unwrap()
            .contains("Be kind on plamenu.test.")
    );

    // Publishing was audit-logged and the version list shows it as current.
    let log = admin_action_log::list(
        &pool,
        &admin_action_log::LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        log.iter()
            .any(|l| l.target_type == "TermsOfService" && l.action == "publish")
    );
    let editor = get(&app, "/admin/terms-of-service", Some(&cookie)).await;
    assert!(editor.body.contains("(current)"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_user_access_ops(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let bob_user = user::find_by_account_id(&pool, bob.id)
        .await
        .unwrap()
        .unwrap();
    // Give bob an active TOTP setup and an unconfirmed address.
    user::set_otp_secret(&pool, bob_user.id, "encrypted-secret")
        .await
        .unwrap();
    user::enable_otp(&pool, bob_user.id).await.unwrap();
    sqlx::query!(
        "UPDATE users SET confirmed_at = NULL WHERE id = $1",
        bob_user.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app_smtp(pool.clone());
    let cookie = login(&app).await;

    let show = get(&app, &format!("/admin/accounts/{}", bob.id), Some(&cookie)).await;
    assert!(show.body.contains("User access"));
    let csrf = csrf_of(&show.body);

    // Disable 2FA clears the TOTP requirement.
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/user-op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "disable_2fa")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let refreshed = user::find_by_id(&pool, bob_user.id).await.unwrap().unwrap();
    assert!(!refreshed.otp_required_for_login);
    assert!(refreshed.otp_secret.is_none());

    // Resending confirmation mints a token and queues a mail.
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/user-op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "resend_confirmation")],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("applied"));
    assert_eq!(email::pending(&pool).await.unwrap(), 1);

    // Password reset scrambles the hash, revokes sessions and mails a link.
    let old_hash = refreshed.password_hash.clone();
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/user-op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "reset_password")],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("applied"));
    let reset = user::find_by_id(&pool, bob_user.id).await.unwrap().unwrap();
    assert_ne!(reset.password_hash, old_hash);
    assert_eq!(email::pending(&pool).await.unwrap(), 2);

    // Changing the address takes effect immediately.
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/user-op", bob.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("op", "change_email"),
            ("email", "bob-new@example.com"),
        ],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("applied"));
    assert_eq!(
        user::find_by_id(&pool, bob_user.id)
            .await
            .unwrap()
            .unwrap()
            .email
            .as_deref(),
        Some("bob-new@example.com")
    );
    // A taken address is refused.
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/user-op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "change_email"), ("email", EMAIL)],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("error"));

    // Peers are untouchable: bob is promoted to Owner, so alice (also Owner)
    // no longer outranks him.
    make_staff(&pool, bob.id).await;
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/user-op", bob.id),
        &cookie,
        &[("csrf", &csrf), ("op", "disable_2fa")],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("error"));

    // Every applied op was audit-logged on the User target.
    let log = admin_action_log::list(
        &pool,
        &admin_action_log::LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let verbs: Vec<_> = log
        .iter()
        .filter(|l| l.target_type == "User")
        .map(|l| l.action.as_str())
        .collect();
    assert_eq!(
        verbs,
        ["change_email", "reset_password", "resend", "disable_2fa"]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn user_appeals_strike_and_admin_resolves(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool.clone());

    // Alice silences bob through the moderation form.
    let alice_cookie = login(&app).await;
    let csrf = csrf_of(
        &get(
            &app,
            &format!("/admin/accounts/{}", bob.id),
            Some(&alice_cookie),
        )
        .await
        .body,
    );
    let resp = post_form(
        &app,
        &format!("/web/admin/accounts/{}/action", bob.id),
        &alice_cookie,
        &[("csrf", &csrf), ("type", "silence"), ("text", "tone")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        account::find_by_id(&pool, bob.id)
            .await
            .unwrap()
            .unwrap()
            .silenced()
    );

    // Bob sees the strike on his settings page and appeals it.
    let bob_cookie = login_as(&app, "bob@example.com", PASSWORD).await;
    let strikes_page = get(&app, "/settings/strikes", Some(&bob_cookie)).await;
    assert_eq!(strikes_page.status, StatusCode::OK);
    assert!(strikes_page.body.contains("Account limited"));
    assert!(strikes_page.body.contains("Appeal this action"));
    let bob_csrf = csrf_of(&strikes_page.body);
    let strike_id = account_warning::for_target(&pool, bob.id).await.unwrap()[0].id;
    let resp = post_form(
        &app,
        &format!("/web/settings/strikes/{strike_id}/appeal"),
        &bob_cookie,
        &[("csrf", &bob_csrf), ("text", "I was quoting song lyrics")],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("saved"));
    // One appeal per strike.
    let resp = post_form(
        &app,
        &format!("/web/settings/strikes/{strike_id}/appeal"),
        &bob_cookie,
        &[("csrf", &bob_csrf), ("text", "again")],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("error"));
    let strikes_page = get(&app, "/settings/strikes", Some(&bob_cookie)).await;
    assert!(strikes_page.body.contains("pending review"));

    // The admin queue lists it; approving reverses the silence and stamps
    // the strike overruled.
    let queue = get(&app, "/admin/appeals", Some(&alice_cookie)).await;
    assert_eq!(queue.status, StatusCode::OK);
    assert!(queue.body.contains("@bob appeals a silence strike"));
    assert!(queue.body.contains("I was quoting song lyrics"));
    let appeal_id = appeal::for_account(&pool, bob.id).await.unwrap()[0].id;
    let resp = post_form(
        &app,
        &format!("/web/admin/appeals/{appeal_id}/approve"),
        &alice_cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("applied"));
    assert!(
        !account::find_by_id(&pool, bob.id)
            .await
            .unwrap()
            .unwrap()
            .silenced()
    );
    let resolved = appeal::for_account(&pool, bob.id).await.unwrap();
    assert!(resolved[0].approved_at.is_some());
    // Deciding twice is refused.
    let resp = post_form(
        &app,
        &format!("/web/admin/appeals/{appeal_id}/reject"),
        &alice_cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("error"));

    // Bob sees the outcome; the decision is in the audit log.
    let strikes_page = get(&app, "/settings/strikes", Some(&bob_cookie)).await;
    assert!(
        strikes_page
            .body
            .contains("approved — the action was reversed")
    );
    let log = admin_action_log::list(
        &pool,
        &admin_action_log::LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        log.iter()
            .any(|l| l.target_type == "Appeal" && l.action == "approve")
    );

    // Appeals of stale strikes are refused (20-day window).
    let stale = account_warning::create(
        &pool,
        account_warning::NewAccountWarning {
            account_id: Some(alice.id),
            target_account_id: bob.id,
            action: "none",
            text: "old warning",
            report_id: None,
            status_ids: &[],
        },
    )
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE account_warnings SET created_at = now() - interval '30 days' WHERE id = $1",
        stale.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let strikes_page = get(&app, "/settings/strikes", Some(&bob_cookie)).await;
    assert!(
        strikes_page
            .body
            .contains("appeal window for this action has passed")
    );
    let resp = post_form(
        &app,
        &format!("/web/settings/strikes/{}/appeal", stale.id),
        &bob_cookie,
        &[("csrf", &bob_csrf), ("text", "too late")],
    )
    .await;
    assert!(resp.location.as_deref().unwrap_or("").contains("error"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn domain_block_purges_cached_media(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let evil = seed_remote_account(&pool, "evil.example", "villain").await;
    // Cached profile images, a cached attachment copy and a cached emoji.
    plamenu_db::account_media::set_file_name(&pool, evil.id, "avatar", "cached-avatar.webp")
        .await
        .unwrap();
    plamenu_db::account_media::set_file_name(&pool, evil.id, "header", "cached-header.webp")
        .await
        .unwrap();
    let attachment_id = plamenu_db::id::next();
    sqlx::query!(
        "INSERT INTO media_attachments
             (id, account_id, content_type, remote_url, file_name, cached_at)
         VALUES ($1, $2, 'image/png', 'https://evil.example/a.png', 'cached-a.png', now())",
        attachment_id,
        evil.id,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO custom_emojis (id, shortcode, domain, image_file_name)
         VALUES ($1, 'evil', 'evil.example', 'cached-emoji.png')",
        plamenu_db::id::next(),
    )
    .execute(&pool)
    .await
    .unwrap();
    // A bystander domain must stay cached.
    let bystander = seed_remote_account(&pool, "fine.example", "friend").await;
    plamenu_db::account_media::set_file_name(&pool, bystander.id, "avatar", "keep.webp")
        .await
        .unwrap();

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(
        &get(&app, "/admin/instance-policy", Some(&cookie))
            .await
            .body,
    );
    let resp = post_form(
        &app,
        "/web/admin/instance-policy/domain-blocks",
        &cookie,
        &[
            ("csrf", &csrf),
            ("domain", "evil.example"),
            ("severity", "noop"),
            ("reject_media", "1"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    // The purge runs in a background task; wait for it to land.
    let mut purged = false;
    for _ in 0..100 {
        let account = account::find_by_id(&pool, evil.id).await.unwrap().unwrap();
        if account.avatar_file_name.is_none() {
            purged = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(purged, "cached avatar was not purged");

    let evil_after = account::find_by_id(&pool, evil.id).await.unwrap().unwrap();
    assert!(evil_after.header_file_name.is_none());
    let attachment = sqlx::query!(
        "SELECT file_name, remote_url FROM media_attachments WHERE id = $1",
        attachment_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(attachment.file_name.is_none(), "attachment copy kept");
    assert!(attachment.remote_url.is_some(), "origin URL must survive");
    let emoji_count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM custom_emojis WHERE domain = 'evil.example'"#
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(emoji_count, 0, "blocked domain's emoji rows must go");

    // The bystander's cache is untouched.
    let friend = account::find_by_id(&pool, bystander.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(friend.avatar_file_name.as_deref(), Some("keep.webp"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn featured_tags_page_features_and_unfeatures(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/settings/featured-tags", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Featured hashtags"));

    // Feature a hashtag by name.
    let csrf = csrf_of(&page.body);
    let resp = post_form(
        &app,
        "/web/settings/featured-tags",
        &cookie,
        &[("csrf", &csrf), ("name", "#rust")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let listed = featured_tag::list(&pool, alice.id).await.unwrap();
    assert_eq!(
        listed.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["rust"]
    );

    // It now shows on the page, with an unfeature control.
    let page = get(&app, "/settings/featured-tags", Some(&cookie)).await;
    assert!(page.body.contains("#rust"));

    // Unfeature by its row id.
    let csrf = csrf_of(&page.body);
    let remove = post_form(
        &app,
        &format!("/web/settings/featured-tags/{}/remove", listed[0].id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(remove.status, StatusCode::SEE_OTHER);
    assert!(
        featured_tag::list(&pool, alice.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn relationships_page_bulk_unfollows(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // The following view lists bob.
    let page = get(&app, "/settings/relationships?rel=following", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("@bob"));

    // Bulk-unfollow bob.
    let csrf = csrf_of(&page.body);
    let bob_id = bob.id.to_string();
    let resp = post_form(
        &app,
        "/web/settings/relationships",
        &cookie,
        &[
            ("csrf", &csrf),
            ("action", "unfollow"),
            ("rel", "following"),
            ("ids", &bob_id),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(!follow::exists(&pool, alice.id, bob.id).await.unwrap());
}

/// The blocked/muted/blocked-domains views list what the viewer has blocked or
/// muted and bulk-lift those relationships.
#[sqlx::test(migrations = "../db/migrations")]
async fn relationships_page_lists_and_lifts_blocks_and_mutes(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    block::create(&pool, alice.id, bob.id, None).await.unwrap();
    mute::upsert(&pool, alice.id, carol.id, true, None)
        .await
        .unwrap();
    account_domain_block::create(&pool, alice.id, "spam.example")
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Blocked view lists bob; bulk-unblock lifts it.
    let page = get(&app, "/settings/relationships?rel=blocked", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("@bob"));
    let csrf = csrf_of(&page.body);
    let bob_id = bob.id.to_string();
    let resp = post_form(
        &app,
        "/web/settings/relationships",
        &cookie,
        &[
            ("csrf", &csrf),
            ("action", "unblock"),
            ("rel", "blocked"),
            ("ids", &bob_id),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(!block::exists(&pool, alice.id, bob.id).await.unwrap());

    // Muted view lists carol; bulk-unmute lifts it.
    let page = get(&app, "/settings/relationships?rel=muted", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("@carol"));
    let csrf = csrf_of(&page.body);
    let carol_id = carol.id.to_string();
    let resp = post_form(
        &app,
        "/web/settings/relationships",
        &cookie,
        &[
            ("csrf", &csrf),
            ("action", "unmute"),
            ("rel", "muted"),
            ("ids", &carol_id),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        mute::find_active(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .is_none()
    );

    // Blocked-domains view lists the domain; bulk-unblock lifts it.
    let page = get(
        &app,
        "/settings/relationships?rel=blocked-domains",
        Some(&cookie),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("spam.example"));
    let csrf = csrf_of(&page.body);
    let resp = post_form(
        &app,
        "/web/settings/relationships",
        &cookie,
        &[
            ("csrf", &csrf),
            ("action", "unblock_domain"),
            ("rel", "blocked-domains"),
            ("domains", "spam.example"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        !account_domain_block::exists(&pool, alice.id, "spam.example")
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_dashboard_shows_software_update_banner(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    // The seeded Admin role carries ADMINISTRATOR, implying view_devops.
    let admin = role::find_by_name(&pool, "Admin").await.unwrap().unwrap();
    role::assign_to_account(&pool, alice.id, Some(admin.id))
        .await
        .unwrap();
    software_update::replace_with(
        &pool,
        &[software_update::NewSoftwareUpdate {
            version: "9.9.9".to_owned(),
            urgent: true,
            release_type: "patch".to_owned(),
            release_notes: "https://example.com/notes".to_owned(),
        }],
    )
    .await
    .unwrap();

    let app = common::test_app(pool);
    let cookie = login(&app).await;
    let resp = get(&app, "/admin", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.body.contains("Security update available"),
        "urgent banner shown"
    );
    assert!(resp.body.contains("9.9.9"));
    assert!(resp.body.contains("https://example.com/notes"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_dashboard_hides_banner_without_updates(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let admin = role::find_by_name(&pool, "Admin").await.unwrap().unwrap();
    role::assign_to_account(&pool, alice.id, Some(admin.id))
        .await
        .unwrap();

    let app = common::test_app(pool);
    let cookie = login(&app).await;
    let resp = get(&app, "/admin", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(!resp.body.contains("Update available"));
    assert!(!resp.body.contains("admin-update"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn missing_png_placeholder_is_served(pool: PgPool) {
    // The account serializer hands out `/static/missing.png` for image-less
    // accounts; clients that don't special-case the name (unlike Phanpy) must
    // get a real image back rather than a 404.
    let app = common::test_app(pool);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/static/missing.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
}

// ---- User lists (web UI) -----------------------------------------------

/// A list timeline collapses repeated boosts exactly as home does: the
/// feed shape is the same, so the presentation is too.
#[sqlx::test(migrations = "../db/migrations")]
async fn list_timeline_merges_repeated_boosts(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let erin = create_local_account(&pool, "erin", "Erin").await;
    let erin_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(erin.id, "<p>much-boosted</p>", "public", None),
    )
    .await
    .unwrap();
    let list = plamenu_db::list::create(&pool, alice.id, "Boosters", "list", false)
        .await
        .unwrap();
    for name in ["bob", "carol"] {
        let account = create_local_account(&pool, name, name).await;
        follow::create(&pool, alice.id, account.id, None)
            .await
            .unwrap();
        plamenu_db::list::add_members(&pool, list.id, alice.id, &[account.id])
            .await
            .unwrap()
            .unwrap();
        status::create_local_reblog(&pool, account.id, erin_post.id)
            .await
            .unwrap();
    }

    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let page = get(&app, &format!("/lists/{}", list.id), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.body.matches("much-boosted").count(),
        1,
        "both boosts on one card"
    );
    assert!(page.body.contains("bob"));
    assert!(page.body.contains("carol"));
}

/// The full list lifecycle through the web UI: create, populate (membership
/// gated on following), watch a member's post land on the timeline, edit,
/// and delete.
#[sqlx::test(migrations = "../db/migrations")]
async fn lists_crud_and_membership_flow(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Empty state, then create a list — landing on its timeline.
    let index = get(&app, "/lists", Some(&cookie)).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("You haven't created any lists yet."));
    let csrf = csrf_of(&index.body);
    let created = post_form(
        &app,
        "/web/lists",
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "Close friends"),
            ("replies_policy", "list"),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    let list_path = created.location.expect("redirect to the new list");
    assert!(list_path.starts_with("/lists/"));
    let list_id: i64 = list_path.rsplit('/').next().unwrap().parse().unwrap();

    // The index now shows it; the timeline is empty.
    assert!(
        get(&app, "/lists", Some(&cookie))
            .await
            .body
            .contains("Close friends")
    );
    assert!(
        get(&app, &list_path, Some(&cookie))
            .await
            .body
            .contains("No posts here yet")
    );

    // Adding an un-followed account fails with a readable error.
    let members_url = format!("/lists/{list_id}/members");
    let mcsrf = csrf_of(&get(&app, &members_url, Some(&cookie)).await.body);
    let add_url = format!("/web/lists/{list_id}/members/add");
    let rejected = post_form(
        &app,
        &add_url,
        &cookie,
        &[("csrf", &mcsrf), ("handle", "@bob")],
    )
    .await;
    assert_eq!(rejected.status, StatusCode::SEE_OTHER);
    assert!(rejected.location.unwrap().contains("error"));

    // Following bob lets the add succeed.
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    let added = post_form(
        &app,
        &add_url,
        &cookie,
        &[("csrf", &mcsrf), ("handle", "@bob")],
    )
    .await;
    assert_eq!(added.status, StatusCode::SEE_OTHER);
    assert!(
        get(&app, &members_url, Some(&cookie))
            .await
            .body
            .contains("Bob")
    );

    // Bob's public post appears on the list timeline.
    status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "<p>hello list</p>", "public", None),
    )
    .await
    .unwrap();
    assert!(
        get(&app, &list_path, Some(&cookie))
            .await
            .body
            .contains("hello list")
    );

    // Removing bob empties the member list again.
    let removed = post_form(
        &app,
        &format!("/web/lists/{list_id}/members/remove"),
        &cookie,
        &[("csrf", &mcsrf), ("account_id", &bob.id.to_string())],
    )
    .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    assert!(
        get(&app, &members_url, Some(&cookie))
            .await
            .body
            .contains("This list has no members yet.")
    );

    // Editing changes the title, replies policy and exclusivity.
    let ecsrf = csrf_of(
        &get(&app, &format!("/lists/{list_id}/edit"), Some(&cookie))
            .await
            .body,
    );
    let updated = post_form(
        &app,
        &format!("/web/lists/{list_id}/edit"),
        &cookie,
        &[
            ("csrf", &ecsrf),
            ("title", "Best friends"),
            ("replies_policy", "none"),
            ("exclusive", "true"),
        ],
    )
    .await;
    assert_eq!(updated.status, StatusCode::SEE_OTHER);
    let stored = plamenu_db::list::find_owned(&pool, alice.id, list_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.title, "Best friends");
    assert_eq!(stored.replies_policy, "none");
    assert!(stored.exclusive);

    // Deleting removes it.
    let deleted = post_form(
        &app,
        &format!("/web/lists/{list_id}/delete"),
        &cookie,
        &[("csrf", &ecsrf)],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::list::find_owned(&pool, alice.id, list_id)
            .await
            .unwrap()
            .is_none()
    );
}

/// A list a viewer doesn't own is a styled 404, not the API's JSON error.
#[sqlx::test(migrations = "../db/migrations")]
async fn other_accounts_list_is_not_found(pool: PgPool) {
    seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let bob_list = plamenu_db::list::create(&pool, bob.id, "Secret", "list", false)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let resp = get(&app, &format!("/lists/{}", bob_list.id), Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert!(resp.body.contains("Not found"));
    assert!(!resp.body.contains("Secret"));
}

/// Every list surface — index, creation page, the three per-list tabs and the
/// profile panel — renders in the owner's stored locale, and a refused write
/// carries a code that the page it lands on re-states in that language rather
/// than an English sentence smuggled through the query string.
#[sqlx::test(migrations = "../db/migrations")]
async fn lists_pages_use_stored_russian_locale(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let stored = user::find_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    user::update_locale(&pool, stored.id, Some("ru"))
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // The empty index, and the creation page behind it.
    let index = get(&app, "/lists", Some(&cookie)).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(index.body.contains("<title>Списки — Plamenu</title>"));
    assert!(index.body.contains("Вы ещё не создали ни одного списка."));
    assert!(index.body.contains("Создать список"));
    assert!(!index.body.contains("Create list"));

    let new_page = get(&app, "/lists/new", Some(&cookie)).await;
    assert!(
        new_page
            .body
            .contains("<title>Новый список — Plamenu</title>")
    );
    assert!(new_page.body.contains("Показывать ответы"));
    assert!(new_page.body.contains("Участникам списка"));
    assert!(
        new_page
            .body
            .contains("Скрыть эти аккаунты из главной ленты")
    );

    let csrf = csrf_of(&new_page.body);
    let created = post_form(
        &app,
        "/web/lists",
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "Близкие друзья"),
            ("replies_policy", "list"),
        ],
    )
    .await;
    let list_path = created.location.expect("redirect to the new list");
    let list_id: i64 = list_path.rsplit('/').next().unwrap().parse().unwrap();

    // The three tabs of a single list.
    let timeline = get(&app, &list_path, Some(&cookie)).await;
    assert!(timeline.body.contains("Разделы списка"));
    assert!(timeline.body.contains("Здесь пока нет записей."));
    assert!(!timeline.body.contains(">Timeline<"));

    let members_url = format!("/lists/{list_id}/members");
    let members = get(&app, &members_url, Some(&cookie)).await;
    assert!(members.body.contains("Добавить участника"));
    assert!(members.body.contains("В этом списке пока нет участников."));
    // The example handles are markup inside one translated sentence.
    assert!(members.body.contains("<code>@alice</code>"));
    assert!(members.body.contains("Укажите адрес, например"));

    let edit = get(
        &app,
        &format!("/lists/{list_id}/edit?saved=1"),
        Some(&cookie),
    )
    .await;
    assert!(edit.body.contains("Список обновлён."));
    assert!(edit.body.contains("Настройки списка"));
    assert!(
        edit.body
            .contains("Удалить этот список? Это действие необратимо.")
    );

    // A refusal: bob is not followed, so the add is declined. The redirect
    // carries the code, and the members page states it in Russian.
    let add = post_form(
        &app,
        &format!("/web/lists/{list_id}/members/add"),
        &cookie,
        &[("csrf", &csrf_of(&members.body)), ("handle", "@bob")],
    )
    .await;
    assert_eq!(
        add.location.as_deref(),
        Some(format!("{members_url}?error=not_followed").as_str()),
        "the refusal travels as a code, not a sentence"
    );
    let refused = get(
        &app,
        &format!("{members_url}?error=not_followed"),
        Some(&cookie),
    )
    .await;
    assert!(
        refused
            .body
            .contains("Добавлять можно только аккаунты, на которые вы подписаны.")
    );

    // Anything else in that parameter renders nothing at all.
    let injected = get(
        &app,
        &format!("{members_url}?error=Call%20555%20to%20verify"),
        Some(&cookie),
    )
    .await;
    assert_eq!(injected.status, StatusCode::OK);
    assert!(!injected.body.contains("Call 555"));

    // The panel reached from a profile names the account inside its heading.
    let panel = get(
        &app,
        &format!("/web/accounts/{}/lists", bob.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(panel.status, StatusCode::OK);
    assert!(panel.body.contains("Добавить @bob в списки"));
    assert!(panel.body.contains("Отметьте списки"));
    assert!(panel.body.contains("Близкие друзья"));
}

/// The "add or remove from lists" panel reached from a profile toggles the
/// account's membership across the viewer's lists, and the endorsement verbs
/// feature the account on the viewer's own profile.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_lists_panel_and_endorsements(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();

    let csrf = csrf_of(&get(&app, "/lists", Some(&cookie)).await.body);
    let created = post_form(
        &app,
        "/web/lists",
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "Friends"),
            ("replies_policy", "list"),
        ],
    )
    .await;
    let list_id: i64 = created
        .location
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    // The panel lists it, unticked; ticking it adds the membership.
    let panel_url = format!("/web/accounts/{}/lists", bob.id);
    let panel = get(&app, &panel_url, Some(&cookie)).await;
    assert_eq!(panel.status, StatusCode::OK);
    assert!(panel.body.contains("Friends"));
    let pcsrf = csrf_of(&panel.body);
    let saved = post_form(
        &app,
        &panel_url,
        &cookie,
        &[
            ("csrf", &pcsrf),
            ("return_to", "/@bob"),
            ("list_ids", &list_id.to_string()),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    assert_eq!(saved.location.as_deref(), Some("/@bob"));
    assert_eq!(
        plamenu_db::list::containing(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // Submitting with nothing ticked reconciles the membership away.
    let cleared = post_form(
        &app,
        &panel_url,
        &cookie,
        &[("csrf", &pcsrf), ("return_to", "/@bob")],
    )
    .await;
    assert_eq!(cleared.status, StatusCode::SEE_OTHER);
    assert!(
        plamenu_db::list::containing(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_empty()
    );

    // Endorsing bob features him on alice's Featured tab.
    let endorsed = post_form(
        &app,
        &format!("/web/accounts/{}/endorse", bob.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", "/@bob")],
    )
    .await;
    assert_eq!(endorsed.status, StatusCode::SEE_OTHER);
    let own = get(&app, "/@alice/featured", Some(&cookie)).await;
    assert!(own.body.contains("profile-featured"));
    assert!(own.body.contains("Bob"));
    // Bob's profile menu now offers to un-feature him.
    assert!(
        get(&app, "/@bob", Some(&cookie))
            .await
            .body
            .contains(&format!("/web/accounts/{}/unendorse", bob.id))
    );

    // Un-endorsing clears the Featured tab.
    let unendorsed = post_form(
        &app,
        &format!("/web/accounts/{}/unendorse", bob.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", "/@bob")],
    )
    .await;
    assert_eq!(unendorsed.status, StatusCode::SEE_OTHER);
    assert!(
        !get(&app, "/@alice/featured", Some(&cookie))
            .await
            .body
            .contains("profile-featured")
    );
}

/// Opts a local account into discoverability, the precondition for being
/// featured in a collection.
async fn make_discoverable(pool: &PgPool, account_id: i64) {
    sqlx::query!(
        "UPDATE accounts SET discoverable = true WHERE id = $1",
        account_id
    )
    .execute(pool)
    .await
    .unwrap();
}

/// The full collection flow through the web UI: create one under settings,
/// feature an account in it, and see it surface on the profile, the public
/// collection page, and the profile overflow menu's checkbox panel.
#[sqlx::test(migrations = "../db/migrations")]
async fn collections_create_feature_and_display(pool: PgPool) {
    seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    make_discoverable(&pool, bob.id).await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    // The Collections tab is listed among the settings sections, with both
    // halves: your collections and the (empty) "featuring you" list. Alice is
    // not discoverable, so the empty state nudges her toward Privacy and reach.
    let index = get(&app, "/settings/collections", Some(&cookie)).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("Your collections"));
    assert!(index.body.contains("Collections featuring you"));
    assert!(
        index
            .body
            .contains("You haven't been added to any collections yet")
    );
    assert!(index.body.contains("/settings/privacy"));

    // Create a collection.
    let created = post_form(
        &app,
        "/web/settings/collections",
        &cookie,
        &[
            ("csrf", &csrf_of(&index.body)),
            ("name", "My favourites"),
            ("description", "People I like"),
            ("discoverable", "1"),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    let manage_path = created.location.expect("redirect to the new collection");
    assert!(manage_path.starts_with("/settings/collections/"));
    let collection_id = manage_path.rsplit('/').next().unwrap().to_owned();

    // Feature @bob in it.
    let manage = get(&app, &manage_path, Some(&cookie)).await;
    assert!(manage.body.contains("My favourites"));
    let added = post_form(
        &app,
        &format!("/web/settings/collections/{collection_id}/members/add"),
        &cookie,
        &[("csrf", &csrf_of(&manage.body)), ("handle", "@bob")],
    )
    .await;
    assert_eq!(added.status, StatusCode::SEE_OTHER);
    assert!(
        get(&app, &manage_path, Some(&cookie))
            .await
            .body
            .contains("Bob"),
        "featured member shows on the manage page"
    );

    // The collection surfaces on alice's Featured tab and its public page
    // shows Bob.
    let profile = get(&app, "/@alice/featured", Some(&cookie)).await;
    assert!(
        profile.body.contains("My favourites"),
        "collection on the Featured tab"
    );
    let public = get(&app, &format!("/@alice/collections/{collection_id}"), None).await;
    assert_eq!(public.status, StatusCode::OK);
    assert!(public.body.contains("My favourites"));
    assert!(public.body.contains("Bob"), "member card on public page");

    // Bob's overflow menu offers to feature him; the panel pre-ticks the
    // collection he already belongs to.
    let bob_profile = get(&app, "/@bob", Some(&cookie)).await;
    assert!(bob_profile.body.contains("Feature in collections"));
    let panel = get(
        &app,
        &format!("/web/accounts/{}/collections", bob.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(panel.status, StatusCode::OK);
    assert!(panel.body.contains("My favourites"));
    assert!(
        panel.body.contains(&format!(
            r#"name="collection_ids" value="{collection_id}" checked"#
        )),
        "the collection Bob is in is pre-checked"
    );

    // Removing him via the manage page empties the collection.
    let manage = get(&app, &manage_path, Some(&cookie)).await;
    let item_marker = r#"name="item_id" value=""#;
    let item_start = manage.body.find(item_marker).expect("an item id") + item_marker.len();
    let item_id = manage.body[item_start..]
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let removed = post_form(
        &app,
        &format!("/web/settings/collections/{collection_id}/members/remove"),
        &cookie,
        &[("csrf", &csrf_of(&manage.body)), ("item_id", &item_id)],
    )
    .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    assert!(
        get(&app, &format!("/@alice/collections/{collection_id}"), None)
            .await
            .body
            .contains("This collection is empty")
    );
}

/// Seeds an accepted membership: `owner` features `member_id` in a fresh
/// discoverable local collection. Returns the collection id.
async fn feature_in_collection(
    pool: &PgPool,
    owner: &plamenu_db::account::Account,
    member_id: i64,
    name: &str,
) -> i64 {
    let coll = plamenu_db::collection::create(
        pool,
        plamenu_db::collection::NewCollection {
            account_id: owner.id,
            name,
            description: "",
            language: None,
            sensitive: false,
            discoverable: true,
            local: true,
            tag_id: None,
            uri: None,
            url: None,
            original_number_of_items: None,
        },
    )
    .await
    .unwrap();
    plamenu_db::collection::add_item(
        pool,
        plamenu_db::collection::NewCollectionItem {
            item_id: plamenu_db::id::next(),
            collection_id: coll.id,
            account_id: Some(member_id),
            state: "accepted",
            uri: None,
            object_uri: None,
            activity_uri: None,
            approval_uri: None,
        },
    )
    .await
    .unwrap();
    coll.id
}

/// The "Featuring you" half: a collection owned by someone else that features
/// you shows under Settings › Collections with the owner's handle, and you can
/// remove yourself from it.
#[sqlx::test(migrations = "../db/migrations")]
async fn collections_featuring_you_can_be_left(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let collection_id = feature_in_collection(&pool, &carol, alice.id, "Carol's picks").await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    // Alice sees Carol's collection under "Collections featuring you", with a
    // Remove-me control aimed at the leave endpoint.
    let index = get(&app, "/settings/collections", Some(&cookie)).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("Collections featuring you"));
    assert!(index.body.contains("Carol's picks"));
    assert!(index.body.contains("@carol"));
    assert!(
        index
            .body
            .contains(&format!("/web/collections/{collection_id}/leave")),
        "a leave control for the collection featuring alice"
    );

    // Removing herself revokes the membership; it drops off the list.
    let left = post_form(
        &app,
        &format!("/web/collections/{collection_id}/leave"),
        &cookie,
        &[("csrf", &csrf_of(&index.body))],
    )
    .await;
    assert_eq!(left.status, StatusCode::SEE_OTHER);
    assert!(
        !get(&app, "/settings/collections", Some(&cookie))
            .await
            .body
            .contains("Carol's picks"),
        "the revoked collection no longer lists alice"
    );
}

/// A collection's public page is anon-reachable, so it renders in the locale
/// the visitor's `Accept-Language` negotiated rather than always in English.
#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_collection_page_negotiates_russian(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let collection = plamenu_db::collection::create(
        &pool,
        plamenu_db::collection::NewCollection {
            account_id: alice.id,
            name: "Empty picks",
            description: "",
            language: None,
            sensitive: false,
            discoverable: true,
            local: true,
            tag_id: None,
            uri: None,
            url: None,
            original_number_of_items: None,
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool);

    let request = Request::builder()
        .uri(format!("/@alice/collections/{}", collection.id))
        .header(header::ACCEPT_LANGUAGE, "ru-RU")
        .body(Body::empty())
        .unwrap();
    let resp = send(&app, request).await;

    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(resp.body.contains("Эта подборка пуста."));
    assert!(!resp.body.contains("This collection is empty"));
}

/// Public pages carry the link-preview metadata scrapers read: a profile is
/// an `og:type profile` card with the avatar, follower summary and canonical
/// URL, plus the `rel=alternate` hop to the `ActivityPub` actor.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_page_carries_open_graph_metadata(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    compose(&app, &cookie, "hello metadata world").await;

    // Anonymous view: exactly what a preview scraper sees.
    let profile = get(&app, "/@alice", None).await;
    assert_eq!(profile.status, StatusCode::OK);
    let body = &profile.body;
    assert!(body.contains(r#"property="og:type" content="profile""#));
    assert!(body.contains(r#"property="og:title" content="alice (@alice@plamenu.test)""#));
    assert!(body.contains(r#"property="og:site_name" content="Plamenu""#));
    assert!(body.contains(r#"rel="canonical" href="https://plamenu.test/@alice""#));
    assert!(body.contains(r#"property="og:url" content="https://plamenu.test/@alice""#));
    assert!(
        body.contains(
            r#"rel="alternate" type="application/activity+json" href="https://plamenu.test/users/alice""#
        ),
        "the AP actor is offered as the alternate representation"
    );
    assert!(
        body.contains(r#"content="1 post, 0 following, 0 followers · test account""#),
        "the description is Mastodon's stat summary plus the bio: {body}"
    );
    assert!(
        body.contains(r#"property="og:image" content="https://plamenu.test/static/missing.png""#)
    );
    assert!(body.contains(r#"property="profile:username" content="alice@plamenu.test""#));
    assert!(body.contains(r#"property="twitter:card" content="summary""#));
    assert!(
        !body.contains("noindex"),
        "an indexable profile carries no robots directive"
    );
}

/// A thread page is an `og:type article` card: the status text as the
/// description, its publication time, the Mastodon-style quoted `<title>`,
/// and the `rel=alternate` hop to the `ActivityPub` object.
#[sqlx::test(migrations = "../db/migrations")]
async fn thread_page_carries_open_graph_metadata(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(
        &app,
        &cookie,
        "The quick brown fox jumps over the lazy dog and keeps on running",
    )
    .await;
    let id = permalink.rsplit('/').next().unwrap().to_owned();

    let thread = get(&app, &permalink, None).await;
    assert_eq!(thread.status, StatusCode::OK);
    let body = &thread.body;
    assert!(
        body.contains("<title>alice: \u{201c}The quick brown fox jumps over the lazy dog and ke…\u{201d} — Plamenu</title>"),
        "the title quotes the first fifty characters: {body}"
    );
    assert!(body.contains(r#"property="og:type" content="article""#));
    assert!(body.contains(r#"property="og:title" content="alice (@alice@plamenu.test)""#));
    assert!(
        body.contains(&format!(
            r#"rel="canonical" href="https://plamenu.test/@alice/{id}""#
        )),
        "the canonical URL is the local permalink"
    );
    assert!(body.contains(&format!(
        r#"rel="alternate" type="application/activity+json" href="https://plamenu.test/users/alice/statuses/{id}""#
    )));
    assert!(
        body.contains(
            r#"name="description" content="The quick brown fox jumps over the lazy dog and keeps on running""#
        ),
        "the description is the status text: {body}"
    );
    assert!(body.contains(r#"property="og:published_time""#));
    assert!(
        body.contains(r#"property="twitter:card" content="summary""#),
        "a text-only post gets the small summary card"
    );
}

/// The instance-level entry points (`/login`, where bare-domain shares land,
/// and the public explore timeline) carry the instance's own preview card.
#[sqlx::test(migrations = "../db/migrations")]
async fn instance_entry_points_carry_site_metadata(pool: PgPool) {
    let app = app_with_alice(pool).await;

    let login = get(&app, "/login", None).await;
    assert_eq!(login.status, StatusCode::OK);
    assert!(
        login
            .body
            .contains(r#"property="og:type" content="website""#)
    );
    assert!(
        login
            .body
            .contains(r#"property="og:title" content="Plamenu""#)
    );
    assert!(
        login
            .body
            .contains(r#"rel="canonical" href="https://plamenu.test/""#)
    );
    assert!(
        login
            .body
            .contains(r#"property="og:site_name" content="Plamenu hosted on plamenu.test""#)
    );
    // Every page links the install manifest, touch icon and ordinary favicon.
    assert!(
        login
            .body
            .contains(r#"link rel="manifest" href="/manifest.webmanifest""#)
    );
    assert!(login.body.contains(
        r#"link rel="apple-touch-icon" sizes="180x180" href="/pwa/apple-touch-icon.png""#
    ));
    assert!(
        login
            .body
            .contains(r#"link rel="icon" type="image/png" sizes="48x48" href="/favicon.ico""#)
    );

    let public = get(&app, "/public", None).await;
    assert_eq!(public.status, StatusCode::OK);
    assert!(
        public
            .body
            .contains(r#"rel="canonical" href="https://plamenu.test/public""#)
    );
    assert!(
        public
            .body
            .contains(r#"property="og:type" content="website""#)
    );
}

/// A collection page is `noindex` (like Mastodon's) but still previews with
/// its name and canonical URL, and offers the AP collection as the alternate.
#[sqlx::test(migrations = "../db/migrations")]
async fn collection_page_carries_metadata_and_noindex(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let collection_id = feature_in_collection(&pool, &carol, alice.id, "Carol's picks").await;
    let app = common::test_app(pool);

    let page = get(&app, &format!("/@carol/collections/{collection_id}"), None).await;
    assert_eq!(page.status, StatusCode::OK);
    let body = &page.body;
    assert!(body.contains(r#"meta name="robots" content="noindex, noarchive""#));
    assert!(body.contains(r#"property="og:title" content="Carol's picks""#));
    assert!(body.contains(&format!(
        r#"rel="canonical" href="https://plamenu.test/@carol/collections/{collection_id}""#
    )));
    assert!(body.contains(&format!(
        r#"rel="alternate" type="application/activity+json" href="https://plamenu.test/users/carol/collections/{collection_id}""#
    )));
}

// --- Content negotiation on shareable pretty URLs -----------------------
//
// A peer that searches by link dereferences the pretty `/@handle…` URL a user
// copies from their address bar, not the canonical `/users/…` AP id. Mastodon
// scrapes the `rel=alternate` tag out of the HTML, but Pleroma/Akkoma and
// GoToSocial refuse any non-AP content-type outright and never look for it — so
// the pretty URL must itself answer `application/activity+json` with the AP
// document, served in place (no redirect, to keep the caller's signature valid).

/// `/@alice` answers the actor document to an `ActivityPub` `Accept`, so a peer
/// resolving a pasted profile link finds the account.
#[sqlx::test(migrations = "../db/migrations")]
async fn pretty_profile_url_serves_activitypub_to_peers(pool: PgPool) {
    let app = app_with_alice(pool).await;

    let resp = get_ap(&app, "/@alice").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.content_type,
        "application/activity+json; charset=utf-8"
    );
    let body: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(body["type"], "Person");
    assert_eq!(body["id"], "https://plamenu.test/users/alice");
    assert_eq!(body["url"], "https://plamenu.test/@alice");

    // A browser still gets the human profile page off the same URL.
    let html = get(&app, "/@alice", None).await;
    assert_eq!(html.status, StatusCode::OK);
    assert!(
        html.content_type.starts_with("text/html"),
        "{}",
        html.content_type
    );
}

/// `/@alice/{id}` answers the Note document to an `ActivityPub` `Accept` — the
/// case Pleroma/Akkoma/GoToSocial search-by-link failed on before.
#[sqlx::test(migrations = "../db/migrations")]
async fn pretty_status_url_serves_activitypub_to_peers(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "findable by its link").await;
    let id = permalink.rsplit('/').next().unwrap().to_owned();

    let resp = get_ap(&app, &permalink).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.content_type,
        "application/activity+json; charset=utf-8"
    );
    let body: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(body["type"], "Note");
    assert_eq!(
        body["id"],
        format!("https://plamenu.test/users/alice/statuses/{id}")
    );
    assert!(
        body["content"]
            .as_str()
            .unwrap()
            .contains("findable by its link")
    );

    // A browser still gets the human thread page off the same URL.
    let html = get(&app, &permalink, None).await;
    assert_eq!(html.status, StatusCode::OK);
    assert!(
        html.content_type.starts_with("text/html"),
        "{}",
        html.content_type
    );
}

/// `/@carol/collections/{id}` answers the FEP-7aa9 collection document to an
/// `ActivityPub` `Accept`.
#[sqlx::test(migrations = "../db/migrations")]
async fn pretty_collection_url_serves_activitypub_to_peers(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let collection_id = feature_in_collection(&pool, &carol, alice.id, "Carol's picks").await;
    let app = common::test_app(pool);

    let resp = get_ap(&app, &format!("/@carol/collections/{collection_id}")).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.content_type,
        "application/activity+json; charset=utf-8"
    );
    let body: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(
        body["id"],
        format!("https://plamenu.test/users/carol/collections/{collection_id}")
    );
    assert!(
        body["type"].as_str().unwrap().contains("Collection"),
        "type was {}",
        body["type"]
    );
}

/// Under authorized fetch the pretty status URL enforces the same signature
/// gate as the canonical object URL: an unsigned AP fetch is rejected, so
/// serving AP off the pretty route opens no secure-mode bypass. Browsers still
/// pass through to the HTML page.
#[sqlx::test(migrations = "../db/migrations")]
async fn pretty_status_url_requires_signature_under_authorized_fetch(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "guarded by secure mode").await;

    let secure = common::test_app_secure(pool, std::sync::Arc::<StubFederation>::default());

    let html = get(&secure, &permalink, None).await;
    assert_eq!(html.status, StatusCode::OK);
    assert!(
        html.content_type.starts_with("text/html"),
        "{}",
        html.content_type
    );

    let ap = get_ap(&secure, &permalink).await;
    assert_eq!(ap.status, StatusCode::UNAUTHORIZED);
}

/// The profile's Activity tab defaults to Mastodon's "Posts and boosts":
/// replies to others hidden, boosts shown — and the `replies`/`boosts` query
/// flags switch all four filter states.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_activity_filter_switches_replies_and_boosts(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let bob_post = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "<p>bob-original</p>", "public", None),
    )
    .await
    .unwrap();
    status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>alice-plain</p>", "public", None),
    )
    .await
    .unwrap();
    status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>alice-reply</p>", "public", Some(bob_post.id)),
    )
    .await
    .unwrap();
    status::create_local_reblog(&pool, alice.id, bob_post.id)
        .await
        .unwrap();
    let app = common::test_app(pool);

    // Default: posts and boosts, replies to others hidden.
    let page = get(&app, "/@alice", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("alice-plain"));
    assert!(page.body.contains("bob-original"), "boost shown by default");
    assert!(
        !page.body.contains("alice-reply"),
        "reply hidden by default"
    );
    assert!(page.body.contains("Posts and boosts"), "filter selector");

    // All activity: replies and boosts both in.
    let page = get(&app, "/@alice?replies=1", None).await;
    assert!(page.body.contains("alice-reply"));
    assert!(page.body.contains("bob-original"));

    // Posts and replies: boosts out.
    let page = get(&app, "/@alice?replies=1&boosts=0", None).await;
    assert!(page.body.contains("alice-reply"));
    assert!(!page.body.contains("bob-original"));

    // Posts only.
    let page = get(&app, "/@alice?boosts=0", None).await;
    assert!(page.body.contains("alice-plain"));
    assert!(!page.body.contains("alice-reply"));
    assert!(!page.body.contains("bob-original"));

    // Mastodon's URL shape for the replies view redirects onto the flag.
    let compat = get(&app, "/@alice/with_replies", None).await;
    assert!(compat.status.is_redirection());
    assert_eq!(compat.location.as_deref(), Some("/@alice?replies=1"));
}

/// A full profile page offers "Load older posts" carrying the filter flags,
/// and the next page picks up where it left off.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_activity_pages_older_posts(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    for n in 0..21 {
        status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, &format!("<p>post-{n}</p>"), "public", None),
        )
        .await
        .unwrap();
    }
    let app = common::test_app(pool);

    let page = get(&app, "/@alice", None).await;
    assert!(page.body.contains("post-20"));
    assert!(!page.body.contains("post-0</p>"), "21st post on next page");
    assert!(page.body.contains("Load older posts"));
    let older = page
        .body
        .split("href=\"")
        .find(|part| part.starts_with("/@alice?max_id="))
        .expect("older link")
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let next = get(&app, &older, None).await;
    assert!(next.body.contains("post-0"));
    assert!(!next.body.contains("post-20</p>"));
}

/// `/@handle/media` shows only posts with attachments, is linked from the
/// section tabs, and the owner's `show_media` setting turns it off.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_media_tab_gated_by_show_media(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>text-only</p>", "public", None),
    )
    .await
    .unwrap();
    let with_media = status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>with-a-picture</p>", "public", None),
    )
    .await
    .unwrap();
    let upload = media::create_local(
        &pool,
        media::NewLocalMedia {
            width: Some(10),
            height: Some(10),
            ..media::NewLocalMedia::new(alice.id, id::next(), "pic.jpg", "image/jpeg")
        },
    )
    .await
    .unwrap();
    media::attach(&pool, &[upload.id], with_media.id, alice.id)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());

    let profile = get(&app, "/@alice", None).await;
    assert!(profile.body.contains(r#"href="/@alice/media""#));

    let media_tab = get(&app, "/@alice/media", None).await;
    assert_eq!(media_tab.status, StatusCode::OK);
    // The tab is an attachment wall, not a feed: a tile links to the
    // containing post and carries the media file for the lightbox, and the
    // posts' own text (media post or not) stays off the page.
    assert!(media_tab.body.contains("media-wall__tile"));
    assert!(
        media_tab.body.contains(&format!(
            r#"href="/@alice/{}#post-{}""#,
            with_media.id, with_media.id
        )),
        "tile links the containing post (with a scroll-to fragment): {}",
        media_tab.body
    );
    assert!(media_tab.body.contains("data-media-url"));
    assert!(!media_tab.body.contains("with-a-picture"));
    assert!(!media_tab.body.contains("text-only"));

    // The owner switches the tab off: the link disappears and a direct hit
    // lands back on the profile.
    account::update_local_profile(
        &pool,
        alice.id,
        account::ProfileUpdate {
            show_media: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let profile = get(&app, "/@alice", None).await;
    assert!(!profile.body.contains(r#"href="/@alice/media""#));
    let gone = get(&app, "/@alice/media", None).await;
    assert!(gone.status.is_redirection());
    assert_eq!(gone.location.as_deref(), Some("/@alice"));
}

/// `/@handle/featured` lists the endorsed accounts (moved off the main
/// profile page), and the owner's `show_featured` setting turns it off.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_featured_tab_gated_by_show_featured(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    endorsement::endorse(&pool, alice.id, bob.id).await.unwrap();
    let app = common::test_app(pool.clone());

    let featured = get(&app, "/@alice/featured", None).await;
    assert_eq!(featured.status, StatusCode::OK);
    assert!(featured.body.contains(r#"href="/@bob""#));
    assert!(featured.body.contains("Profiles"));

    // The endorsements live on the Featured tab now, not the main page —
    // which links the tab instead.
    let profile = get(&app, "/@alice", None).await;
    assert!(!profile.body.contains("profile-featured"));
    assert!(profile.body.contains(r#"href="/@alice/featured""#));

    account::update_local_profile(
        &pool,
        alice.id,
        account::ProfileUpdate {
            show_featured: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let profile = get(&app, "/@alice", None).await;
    assert!(!profile.body.contains(r#"href="/@alice/featured""#));
    let gone = get(&app, "/@alice/featured", None).await;
    assert!(gone.status.is_redirection());
    assert_eq!(gone.location.as_deref(), Some("/@alice"));
}

/// A featured hashtag shows as a chip on the Activity tab and
/// `/@handle/tagged/{tag}` narrows the feed to that tag.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_featured_hashtag_links_tagged_view(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let tagged = status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>about-rust</p>", "public", None),
    )
    .await
    .unwrap();
    let tag_id = tag::ensure(&pool, "rust").await.unwrap();
    tag::attach(&pool, tagged.id, tag_id).await.unwrap();
    status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>off-topic</p>", "public", None),
    )
    .await
    .unwrap();
    featured_tag::feature(&pool, alice.id, tag_id)
        .await
        .unwrap();
    let app = common::test_app(pool);

    let profile = get(&app, "/@alice", None).await;
    assert!(profile.body.contains(r#"href="/@alice/tagged/rust""#));

    let tagged_view = get(&app, "/@alice/tagged/rust", None).await;
    assert_eq!(tagged_view.status, StatusCode::OK);
    assert!(tagged_view.body.contains("about-rust"));
    assert!(!tagged_view.body.contains("off-topic"));
    assert!(tagged_view.body.contains("#rust"));
}

// ---- Groups (web UI) -----------------------------------------------------

/// The group creation flow through `/groups`: empty state, create, land on
/// the group's profile, and see it under "Your groups".
#[sqlx::test(migrations = "../db/migrations")]
async fn groups_page_creates_and_lists(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let index = get(&app, "/groups", Some(&cookie)).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("You haven't joined any groups yet."));
    // Creation moved to its own page; the index links to it.
    assert!(index.body.contains(r#"href="/groups/new""#));
    let new_page = get(&app, "/groups/new", Some(&cookie)).await;
    assert_eq!(new_page.status, StatusCode::OK);
    assert!(new_page.body.contains("New group"));
    let csrf = csrf_of(&new_page.body);
    let created = post_form(
        &app,
        "/web/groups",
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "hiking"),
            ("display_name", "Hiking & trails"),
            ("membership_policy", "open"),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    assert_eq!(created.location.as_deref(), Some("/@hiking"));

    // The group's page is its profile, wearing the Group badge.
    let profile = get(&app, "/@hiking", Some(&cookie)).await;
    assert_eq!(profile.status, StatusCode::OK);
    assert!(profile.body.contains("Group"));
    assert!(profile.body.contains("Hiking &amp; trails"));

    // The creator is a member, so the group sits under "Your groups".
    let index = get(&app, "/groups", Some(&cookie)).await;
    assert!(index.body.contains("hiking"));
    assert!(!index.body.contains("You haven't joined any groups yet."));

    // A taken name bounces back with a readable error.
    let csrf = csrf_of(&index.body);
    let clash = post_form(
        &app,
        "/web/groups",
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "hiking"),
            ("membership_policy", "open"),
        ],
    )
    .await;
    assert_eq!(clash.status, StatusCode::SEE_OTHER);
    assert!(clash.location.unwrap().contains("error"));
}

/// With `group_creation_policy = admins`, ordinary accounts see no create
/// form and their POST is turned away.
#[sqlx::test(migrations = "../db/migrations")]
async fn groups_creation_respects_the_instance_policy(pool: PgPool) {
    seed_alice(&pool).await;
    let settings = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            group_creation_policy: plamenu_db::instance_settings::GroupCreationPolicy::Admins,
            ..settings.as_update()
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let index = get(&app, "/groups", Some(&cookie)).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(!index.body.contains("New group"));
    // The nav CSRF is still on the page (logout form) — use it to prove the
    // POST is policy-gated, not CSRF-gated.
    let csrf = csrf_of(&index.body);
    let denied = post_form(
        &app,
        "/web/groups",
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "hiking"),
            ("membership_policy", "open"),
        ],
    )
    .await;
    assert_eq!(denied.status, StatusCode::SEE_OTHER);
    assert!(denied.location.unwrap().contains("error"));
    assert!(
        plamenu_db::account::find_local_by_username(&pool, "hiking")
            .await
            .unwrap()
            .is_none()
    );
}

/// The group management console: the owner reaches every tab, the
/// settings form persists and refreshes the locked flag, and a group that
/// isn't theirs (or doesn't exist) 404s.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_management_console_owner_flow(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let index = get(&app, "/groups", Some(&cookie)).await;
    let csrf = csrf_of(&index.body);
    post_form(
        &app,
        "/web/groups",
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "hiking"),
            ("membership_policy", "approval"),
        ],
    )
    .await;
    let hiking = plamenu_db::account::find_local_by_username(&pool, "hiking")
        .await
        .unwrap()
        .unwrap();
    let gid = hiking.id;
    assert!(hiking.locked, "approval groups start locked");

    // The owner reaches the settings page with every tab (approval + owner).
    let manage = get(&app, &format!("/groups/{gid}/manage"), Some(&cookie)).await;
    assert_eq!(manage.status, StatusCode::OK);
    assert!(manage.body.contains("Members &amp; bans"));
    assert!(manage.body.contains("Join requests"));
    assert!(manage.body.contains("Moderators"));
    // The section selector is the shared `tab_strip` disclosure used across the
    // rest of the client, not a bespoke unstyled strip.
    assert!(manage.body.contains("nav-select"));
    assert!(manage.body.contains(r#"aria-label="Group moderation""#));
    assert!(manage.body.contains("Reports"));

    // The console is reached from the privileged-tools shield in the group's
    // profile card — shown to the owner, absent for a logged-out visitor.
    let profile = get(&app, "/@hiking", Some(&cookie)).await;
    assert_eq!(profile.status, StatusCode::OK);
    assert!(profile.body.contains("Manage group"));
    assert!(profile.body.contains("data-privileged-menu"));
    assert!(profile.body.contains(&format!("/groups/{gid}/manage")));
    let anon = get(&app, "/@hiking", None).await;
    assert!(!anon.body.contains("Manage group"));

    // Members, requests, moderators and reports pages all load for the owner.
    for tab in ["members", "requests", "moderators", "reports"] {
        let page = get(&app, &format!("/groups/{gid}/{tab}"), Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK, "{tab} page loads");
    }

    // Switching to an open, mods-only community persists and clears `locked`.
    let csrf = csrf_of(&manage.body);
    // The settings form carries uploads (avatar/banner), so it submits as
    // multipart — like the profile editor it shares its write path with.
    let saved = post_multipart(
        &app,
        &format!("/web/groups/{gid}/settings"),
        &cookie,
        &[
            ("csrf", &csrf),
            ("display_name", "Trails"),
            ("note", "Hiking chat"),
            ("membership_policy", "open"),
            ("posting_policy", "mods"),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    let group = plamenu_db::group::find(&pool, gid).await.unwrap().unwrap();
    assert!(group.posting_restricted_to_mods());
    assert_eq!(
        group.posting_policy(),
        plamenu_db::group::PostingPolicy::Mods
    );
    assert_eq!(
        group.membership_policy(),
        plamenu_db::group::MembershipPolicy::Open
    );
    let refreshed = plamenu_db::account::find_by_id(&pool, gid)
        .await
        .unwrap()
        .unwrap();
    assert!(!refreshed.locked, "an open group manually-approves nobody");

    // A group that doesn't exist 404s rather than leaking a management shell.
    let missing = get(&app, "/groups/999999/manage", Some(&cookie)).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_groups_console_lists_transfers_and_deletes(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    let app = build_router(state.clone());
    let cookie = login(&app).await;

    // A group alice owns, with bob as a second local member.
    let (group_account, _) = plamenu::groups::create_group(
        &state,
        plamenu::groups::CreateGroupParams {
            name: "hiking",
            display_name: "Hiking",
            membership_policy: plamenu_db::group::MembershipPolicy::Open,
            posting_policy: plamenu_db::group::PostingPolicy::Members,
            created_by: alice.id,
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    .unwrap();
    let gid = group_account.id;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    follow::create(&pool, bob.id, gid, None).await.unwrap();

    // The list requires MANAGE_GROUPS and surfaces the group.
    let list = get(&app, "/admin/groups", Some(&cookie)).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.body.contains("@hiking"));
    assert!(
        list.body.contains("href=\"/admin/groups\""),
        "the Groups tab is visible to staff"
    );

    // The detail page shows the owner and yields a CSRF token.
    let detail = get(&app, &format!("/admin/groups/{gid}"), Some(&cookie)).await;
    assert_eq!(detail.status, StatusCode::OK);
    assert!(detail.body.contains("@alice"));
    let csrf = csrf_of(&detail.body);

    // Transfer ownership to bob (a member); alice is demoted to moderator.
    let transferred = post_form(
        &app,
        &format!("/web/admin/groups/{gid}/transfer"),
        &cookie,
        &[("csrf", &csrf), ("handle", "@bob")],
    )
    .await;
    assert_eq!(transferred.status, StatusCode::SEE_OTHER);
    assert_eq!(
        plamenu_db::group::affiliation_of(&pool, gid, bob.id)
            .await
            .unwrap(),
        Some(plamenu_db::group::Affiliation::Owner)
    );

    // Suspend then unsuspend.
    let op = |op: &'static str, csrf: String| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            post_form(
                &app,
                &format!("/web/admin/groups/{gid}/op"),
                &cookie,
                &[("csrf", &csrf), ("op", op)],
            )
            .await
        }
    };
    op("suspend", csrf.clone()).await;
    assert!(
        account::find_by_id(&pool, gid)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );
    op("unsuspend", csrf.clone()).await;
    assert!(
        !account::find_by_id(&pool, gid)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );

    // Delete tombstones the group.
    let deleted = op("delete", csrf.clone()).await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert!(account::is_deleted(&pool, gid).await.unwrap());

    // The lifecycle landed in the M34 audit log against the Group target.
    let logs = admin_action_log::list(
        &pool,
        &admin_action_log::LogFilter {
            limit: 100,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        logs.iter()
            .any(|l| l.target_type == "Group" && l.action == "delete"),
        "delete recorded against a Group target"
    );
}

// ---- Private mentions (conversations) -----------------------------------

/// Signs in, composes a direct status, and returns its thread path.
async fn compose_direct(app: &Router, cookie: &str, text: &str) -> String {
    let csrf = csrf_of(&get(app, "/", Some(cookie)).await.body);
    let posted = post_multipart(
        app,
        "/web/compose",
        cookie,
        &[("csrf", &csrf), ("status", text), ("visibility", "direct")],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    posted.location.expect("redirect to the new post")
}

#[sqlx::test(migrations = "../db/migrations")]
async fn conversations_page_tracks_direct_threads(pool: PgPool) {
    seed_user(&pool, "alice", EMAIL, PASSWORD).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool);

    let alice = login(&app).await;
    // An empty inbox educates instead of listing.
    let page = get(&app, "/conversations", Some(&alice)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Nothing here yet"), "{}", page.body);
    assert!(!page.body.contains("data-conversation-id"));

    let bob = login_as(&app, "bob@example.com", PASSWORD).await;
    let thread = compose_direct(&app, &bob, "@alice psst, tuesday?").await;

    // Alice's inbox: one unread row with the sender and the message, and the
    // drawer burger announces it (the conversations-specific dot).
    let page = get(&app, "/conversations", Some(&alice)).await;
    assert!(page.body.contains("data-conversation-id"));
    assert!(page.body.contains("is-unread"));
    assert!(page.body.contains("psst, tuesday?"));
    assert!(page.body.contains("Menu (new private mentions)"));

    // Opening the thread reads it — the GET side effect, no JS involved.
    assert_eq!(
        get(&app, &thread, Some(&alice)).await.status,
        StatusCode::OK
    );
    let page = get(&app, "/conversations", Some(&alice)).await;
    assert!(page.body.contains("data-conversation-id"));
    assert!(!page.body.contains("is-unread"));
    assert!(!page.body.contains("Menu (new private mentions)"));

    // The sender's own row was never unread (own sends don't self-flag).
    let page = get(&app, "/conversations", Some(&bob)).await;
    assert!(page.body.contains("data-conversation-id"));
    assert!(!page.body.contains("is-unread"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn conversation_row_verbs(pool: PgPool) {
    seed_user(&pool, "alice", EMAIL, PASSWORD).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool);
    let bob = login_as(&app, "bob@example.com", PASSWORD).await;
    compose_direct(&app, &bob, "@alice psst").await;

    let alice = login(&app).await;
    let page = get(&app, "/conversations", Some(&alice)).await;
    let marker = "data-conversation-id=\"";
    let start = page.body.find(marker).expect("a conversation row") + marker.len();
    let row_id = page.body[start..].split('"').next().unwrap().to_owned();
    let csrf = csrf_of(&page.body);

    // Read, then unread again, through the row's menu forms.
    let read = post_form(
        &app,
        &format!("/web/conversations/{row_id}/read"),
        &alice,
        &[("csrf", &csrf), ("return_to", "/conversations")],
    )
    .await;
    assert_eq!(read.status, StatusCode::SEE_OTHER);
    assert_eq!(read.location.as_deref(), Some("/conversations"));
    let page = get(&app, "/conversations", Some(&alice)).await;
    assert!(!page.body.contains("is-unread"));
    let unread = post_form(
        &app,
        &format!("/web/conversations/{row_id}/unread"),
        &alice,
        &[("csrf", &csrf), ("return_to", "/conversations")],
    )
    .await;
    assert_eq!(unread.status, StatusCode::SEE_OTHER);
    let page = get(&app, "/conversations", Some(&alice)).await;
    assert!(page.body.contains("is-unread"));

    // A bad token is rejected; someone else's row id is nobody's business.
    let forged = post_form(
        &app,
        &format!("/web/conversations/{row_id}/read"),
        &alice,
        &[("csrf", "nope"), ("return_to", "/conversations")],
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let bob_csrf = csrf_of(&get(&app, "/conversations", Some(&bob)).await.body);
    let foreign = post_form(
        &app,
        &format!("/web/conversations/{row_id}/read"),
        &bob,
        &[("csrf", &bob_csrf), ("return_to", "/conversations")],
    )
    .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);

    // Remove hides the thread from the inbox (statuses untouched).
    let removed = post_form(
        &app,
        &format!("/web/conversations/{row_id}/remove"),
        &alice,
        &[("csrf", &csrf), ("return_to", "/conversations")],
    )
    .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    let page = get(&app, "/conversations", Some(&alice)).await;
    assert!(!page.body.contains("data-conversation-id"));
    assert!(page.body.contains("Nothing here yet"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn direct_reply_composer_is_locked(pool: PgPool) {
    seed_user(&pool, "alice", EMAIL, PASSWORD).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = common::test_app(pool);
    let bob = login_as(&app, "bob@example.com", PASSWORD).await;
    let thread = compose_direct(&app, &bob, "@alice psst").await;
    let status_id = thread.rsplit('/').next().unwrap().to_owned();

    // The thread page's inline composer is locked to a private mention and
    // says who will receive the reply.
    let alice = login(&app).await;
    let page = get(&app, &thread, Some(&alice)).await;
    assert!(page.body.contains("compose--direct"), "{thread}");
    assert!(page.body.contains(r#"name="visibility" value="direct""#));
    assert!(!page.body.contains(r#"<select name="visibility""#));
    assert!(page.body.contains("Will be seen by @bob"));

    // The deep-linked reply composer (the inbox row's Reply) is locked too.
    let compose_page = get(
        &app,
        &format!("/compose?reply={status_id}&visibility=direct"),
        Some(&alice),
    )
    .await;
    assert!(compose_page.body.contains("compose--direct"));
    assert!(
        compose_page
            .body
            .contains(r#"name="visibility" value="direct""#)
    );
    assert!(!compose_page.body.contains(r#"<select name="visibility""#));

    // A public thread keeps the visibility selector.
    let public = compose(&app, &bob, "hello world").await;
    let page = get(&app, &public, Some(&alice)).await;
    assert!(!page.body.contains("compose--direct"));
    assert!(page.body.contains(r#"<select name="visibility""#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn role_form_round_trips_manage_groups(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    make_staff(&pool, alice.id).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/admin/roles", Some(&cookie)).await.body);

    // Creating a role with the groups permission actually grants it…
    let resp = post_form(
        &app,
        "/web/admin/roles",
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "Groupkeeper"),
            ("position", "5"),
            ("manage_groups", "1"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let keeper = role::find_by_name(&pool, "Groupkeeper")
        .await
        .unwrap()
        .unwrap();
    assert!(keeper.can(role::permission::MANAGE_GROUPS));

    // …and an edit that keeps the box checked keeps the bit. The historical
    // bug rewrote the bitmask without it, silently revoking the permission.
    let resp = post_form(
        &app,
        &format!("/web/admin/roles/{}/update", keeper.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "Groupkeeper"),
            ("position", "5"),
            ("manage_groups", "1"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let kept = role::find_by_id(&pool, keeper.id).await.unwrap().unwrap();
    assert!(kept.can(role::permission::MANAGE_GROUPS));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn relationships_requests_view_accepts_and_rejects(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    follow::create_request(&pool, bob.id, alice.id, None)
        .await
        .unwrap();
    follow::create_request(&pool, carol.id, alice.id, None)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // The requests view lists both requesters, shows the tab badge, and
    // offers the accept/reject pair.
    let page = get(&app, "/settings/relationships?rel=requests", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("bob"));
    assert!(page.body.contains("carol"));
    assert!(page.body.contains("Accept selected"));
    assert!(page.body.contains("Reject selected"));
    assert!(page.body.contains("Received requests (2)"));
    let csrf = csrf_of(&page.body);

    // Accepting bob activates the follow.
    let resp = post_form(
        &app,
        "/web/settings/relationships",
        &cookie,
        &[
            ("csrf", &csrf),
            ("action", "authorize"),
            ("rel", "requests"),
            ("ids", &bob.id.to_string()),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let edge = follow::find(&pool, bob.id, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!edge.pending);

    // Rejecting carol removes the request outright.
    let resp = post_form(
        &app,
        "/web/settings/relationships",
        &cookie,
        &[
            ("csrf", &csrf),
            ("action", "reject"),
            ("rel", "requests"),
            ("ids", &carol.id.to_string()),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        follow::find(&pool, carol.id, alice.id)
            .await
            .unwrap()
            .is_none()
    );

    // A double submit against the already-resolved request is tolerated.
    let resp = post_form(
        &app,
        "/web/settings/relationships",
        &cookie,
        &[
            ("csrf", &csrf),
            ("action", "reject"),
            ("rel", "requests"),
            ("ids", &carol.id.to_string()),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    // The queue is empty again.
    let page = get(&app, "/settings/relationships?rel=requests", Some(&cookie)).await;
    assert!(page.body.contains("No pending follow requests."));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn settings_filters_crud(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    let page = get(&app, "/settings/filters", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("New filter"));
    let csrf = csrf_of(&page.body);

    // Create with two contexts and an initial whole-word keyword.
    let resp = post_form(
        &app,
        "/web/settings/filters",
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "Spoilers"),
            ("context", "home"),
            ("context", "public"),
            ("action", "warn"),
            ("expires_in", ""),
            ("new_keyword", "finale"),
            ("new_whole_word", "false"),
            ("new_whole_word", "true"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let account = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let filters = custom_filter::owned_by(&pool, account.id).await.unwrap();
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].title, "Spoilers");
    assert_eq!(filters[0].context, ["home", "public"]);
    assert!(filters[0].expires_at.is_none());
    let keywords = custom_filter::keywords_for(&pool, filters[0].id)
        .await
        .unwrap();
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].keyword, "finale");
    assert!(keywords[0].whole_word);

    // The edit page renders; updating rewrites attributes, edits the keyword
    // in place and adds a second one.
    let edit = get(
        &app,
        &format!("/settings/filters/{}", filters[0].id),
        Some(&cookie),
    )
    .await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(edit.body.contains("Spoilers"));
    let kw_text = format!("keywords[{}][keyword]", keywords[0].id);
    let kw_whole = format!("keywords[{}][whole_word]", keywords[0].id);
    let resp = post_form(
        &app,
        &format!("/web/settings/filters/{}", filters[0].id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "Season spoilers"),
            ("context", "home"),
            ("action", "hide"),
            ("expires_in", "3600"),
            (&kw_text, "ending"),
            (&kw_whole, "false"),
            ("new_keyword", "season finale"),
            ("new_whole_word", "false"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let updated = custom_filter::find_owned(&pool, account.id, filters[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.title, "Season spoilers");
    assert_eq!(updated.context, ["home"]);
    assert_eq!(updated.action, "hide");
    assert!(updated.expires_at.is_some());
    let keywords = custom_filter::keywords_for(&pool, updated.id)
        .await
        .unwrap();
    assert_eq!(keywords.len(), 2);
    assert_eq!(keywords[0].keyword, "ending");
    assert!(!keywords[0].whole_word);

    // Clearing a keyword's text removes it; "keep" preserves the expiry.
    let resp = post_form(
        &app,
        &format!("/web/settings/filters/{}", updated.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "Season spoilers"),
            ("context", "home"),
            ("action", "hide"),
            ("expires_in", "keep"),
            (&kw_text, ""),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let kept = custom_filter::find_owned(&pool, account.id, updated.id)
        .await
        .unwrap()
        .unwrap();
    assert!(kept.expires_at.is_some());
    let keywords = custom_filter::keywords_for(&pool, updated.id)
        .await
        .unwrap();
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].keyword, "season finale");

    // Delete removes the filter entirely.
    let resp = post_form(
        &app,
        &format!("/web/settings/filters/{}/delete", updated.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        custom_filter::owned_by(&pool, account.id)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The web filter form enforces the same per-account cardinality cap as the
/// REST API (audit #58): at the limit a create is bounced back to the form with
/// an error instead of persisting another filter.
#[sqlx::test(migrations = "../db/migrations")]
async fn settings_filters_enforce_account_cap(pool: PgPool) {
    use plamenu_db::custom_filter::MAX_FILTERS_PER_ACCOUNT;

    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let account = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    for i in 0..MAX_FILTERS_PER_ACCOUNT {
        custom_filter::create(
            &pool,
            account.id,
            &format!("f{i}"),
            "warn",
            &["home".to_owned()],
            None,
            &[],
        )
        .await
        .unwrap();
    }

    let csrf = csrf_of(&get(&app, "/settings/filters/new", Some(&cookie)).await.body);
    let resp = post_form(
        &app,
        "/web/settings/filters",
        &cookie,
        &[
            ("csrf", &csrf),
            ("title", "one too many"),
            ("context", "home"),
            ("action", "warn"),
            ("expires_in", ""),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        resp.location
            .as_deref()
            .unwrap_or_default()
            .contains("error"),
        "over-limit create redirects back with an error: {:?}",
        resp.location
    );
    assert_eq!(
        custom_filter::count_owned(&pool, account.id).await.unwrap(),
        i64::try_from(MAX_FILTERS_PER_ACCOUNT).unwrap(),
        "no extra filter was persisted"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn privacy_saves_all_notification_policy_categories(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let page = get(&app, "/settings/privacy", Some(&cookie)).await;
    assert!(page.body.contains("not_following_policy"));
    assert!(page.body.contains("limited_accounts_policy"));
    let csrf = csrf_of(&page.body);

    let resp = post_form(
        &app,
        "/web/settings/privacy",
        &cookie,
        &[
            ("csrf", &csrf),
            ("not_following_policy", "filter"),
            ("not_followers_policy", "drop"),
            ("new_accounts_policy", "filter"),
            ("private_mentions_policy", "accept"),
            ("limited_accounts_policy", "drop"),
            ("bots_policy", "filter"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let account = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let policy = notification_policy::get_or_default(&pool, account.id)
        .await
        .unwrap();
    assert_eq!(policy.for_not_following.as_str(), "filter");
    assert_eq!(policy.for_not_followers.as_str(), "drop");
    assert_eq!(policy.for_new_accounts.as_str(), "filter");
    assert_eq!(policy.for_private_mentions.as_str(), "accept");
    assert_eq!(policy.for_limited_accounts.as_str(), "drop");
    assert_eq!(policy.for_bots.as_str(), "filter");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notification_requests_page_accepts_and_dismisses(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let carol = seed_user(&pool, "carol", "carol@example.com", PASSWORD).await;
    notification_request::record_filtered(&pool, alice.id, bob.id, "mention", None)
        .await
        .unwrap();
    notification_request::record_filtered(&pool, alice.id, carol.id, "mention", None)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/notifications/requests", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("bob"));
    assert!(page.body.contains("carol"));
    let csrf = csrf_of(&page.body);

    let requests = notification_request::list(&pool, alice.id, None, None, None, 10)
        .await
        .unwrap();
    let bob_req = requests
        .iter()
        .find(|r| r.from_account_id == bob.id)
        .unwrap();
    let carol_req = requests
        .iter()
        .find(|r| r.from_account_id == carol.id)
        .unwrap();

    // Accept releases bob's request…
    let resp = post_form(
        &app,
        &format!("/web/notifications/requests/{}/accept", bob_req.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        notification_request::find(&pool, alice.id, bob_req.id)
            .await
            .unwrap()
            .is_none()
    );

    // …dismiss discards carol's, and a repeat submit is tolerated.
    for _ in 0..2 {
        let resp = post_form(
            &app,
            &format!("/web/notifications/requests/{}/dismiss", carol_req.id),
            &cookie,
            &[("csrf", &csrf)],
        )
        .await;
        assert_eq!(resp.status, StatusCode::SEE_OTHER);
    }

    let page = get(&app, "/notifications/requests", Some(&cookie)).await;
    assert!(page.body.contains("No filtered notifications"));
}

// ---- Scheduled posts ----------------------------------------------------

/// Choosing Article publishes a titled `ActivityPub` Article.
#[sqlx::test(migrations = "../db/migrations")]
async fn composer_publishes_a_long_form_post(pool: PgPool) {
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    seed_alice(&state.pool).await;
    let app = build_router(state.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/compose", Some(&cookie)).await;
    assert!(page.body.contains("data-compose-kind"));
    assert!(page.body.contains(r#"value="article">Article</option>"#));
    assert!(!page.body.contains("compose-long-form-kind"));

    let csrf = csrf_of(&page.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "A body worth reading, at length."),
            ("visibility", "public"),
            ("post_kind", "article"),
            ("title", "On long-form"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER, "{}", posted.body);

    let status_id = posted
        .location
        .expect("redirect to the new post")
        .rsplit('/')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let article = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert_eq!(article.object_type.as_deref(), Some("Article"));
    assert_eq!(article.title.as_deref(), Some("On long-form"));
}

/// A malformed request with conflicting post types is refused.
#[sqlx::test(migrations = "../db/migrations")]
async fn composer_refuses_two_post_kinds_at_once(pool: PgPool) {
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    seed_alice(&state.pool).await;
    let app = build_router(state.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);

    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "both at once"),
            ("visibility", "public"),
            ("post_kind", "article"),
            ("post_kind", "event"),
            ("title", "Ambiguous"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "re-rendered, not redirected");
    assert!(
        resp.body.contains("Pick one post kind"),
        "the banner says which choice is missing"
    );
}

/// toolbar toggle; the group composer drops both (group posts can't be
/// scheduled).
#[sqlx::test(migrations = "../db/migrations")]
async fn composer_offers_a_schedule_field_except_for_groups(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool).await;
    let cookie = login(&app).await;
    let page = get(&app, "/compose", Some(&cookie)).await;
    assert!(
        page.body.contains(r#"data-compose-section="schedule""#),
        "schedule section"
    );
    assert!(page.body.contains(r#"name="scheduled_at""#), "input");
    assert!(page.body.contains("Times are in UTC"), "zone hint");

    let group_page = get(&app, &format!("/compose?group={group_id}"), Some(&cookie)).await;
    assert!(
        !group_page.body.contains(r#"name="scheduled_at""#),
        "no schedule field on the group composer"
    );
}

/// A filled Schedule field queues the draft instead of publishing: the row
/// lands in `scheduled_statuses` (interpreted in the viewer's zone), no status
/// is created, and the composer redirects to the scheduled listing.
/// TZ slice 1: every human-facing timestamp renders as a `<time>` whose body
/// is the viewer's wall-clock reading, whose `datetime` stays RFC 3339 UTC for
/// machines (R4), and whose tooltip carries *both* readings so UTC is always
/// one hover away (R3). Seeded near a UTC midnight so the two readings fall on
/// different calendar days — the case a single-reading tooltip gets wrong.
#[sqlx::test(migrations = "../db/migrations")]
async fn timestamps_render_in_the_viewer_zone_with_a_dual_tooltip(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    sqlx::query("UPDATE users SET time_zone = 'Europe/Berlin' WHERE account_id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let permalink = compose(&app, &cookie, "what time is it").await;

    // 22:30Z on a summer day is 00:30 the *next* day in Berlin (UTC+02:00).
    sqlx::query(
        "UPDATE statuses SET created_at = '2026-07-03T22:30:00Z' \
         WHERE account_id = $1",
    )
    .bind(alice.id)
    .execute(&pool)
    .await
    .unwrap();

    let thread = get(&app, &permalink, Some(&cookie)).await;
    let meta = thread
        .body
        .split_once("status__detail-meta")
        .expect("detail meta")
        .1;
    let cell = &meta[..meta.find("</span>").expect("detail time cell")];

    // R4: machines still get UTC.
    assert!(
        cell.contains(r#"datetime="2026-07-03T22:30:00Z""#),
        "{cell}"
    );
    // R1: the body is the Berlin wall clock — the next calendar day.
    assert!(cell.contains("Jul 4, 2026, 00:30"), "{cell}");
    // R3: the tooltip carries the zone, the delta in force at that instant,
    // and the UTC reading dated, because the two days differ.
    assert!(cell.contains("Europe/Berlin (UTC+02:00)"), "{cell}");
    assert!(cell.contains("Jul 3, 2026, 22:30"), "{cell}");
    assert!(cell.contains("UTC\""), "{cell}");

    // A UTC viewer sees the same instant as its UTC reading, with no second
    // reading bolted on.
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let bob_cookie = login_as(&app, "bob@example.com", PASSWORD).await;
    let as_bob = get(&app, &permalink, Some(&bob_cookie)).await;
    let meta = as_bob
        .body
        .split_once("status__detail-meta")
        .expect("detail meta")
        .1;
    let cell = &meta[..meta.find("</span>").expect("detail time cell")];
    assert!(cell.contains("Jul 3, 2026, 22:30"), "{cell}");
    assert!(!cell.contains("Europe/Berlin"), "{cell}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scheduling_a_post_queues_it_instead_of_publishing(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    // Berlin winter is UTC+1, so 12:00 wall clock must store as 11:00 UTC.
    sqlx::query("UPDATE users SET time_zone = 'Europe/Berlin' WHERE account_id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);

    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "see you in the future"),
            ("visibility", "public"),
            ("scheduled_at", "2030-01-15T12:00"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    assert_eq!(
        posted.location.as_deref(),
        Some("/settings/scheduled?saved=scheduled")
    );

    let rows = scheduled_status::list_for_account(&pool, alice.id, None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one queued row");
    let row = &rows[0];
    assert_eq!(row.text, "see you in the future");
    assert_eq!(row.visibility, "public");
    assert!(row.application_id.is_none(), "web posts carry no app");
    let expected = time::macros::datetime!(2030-01-15 11:00 UTC);
    assert_eq!(row.scheduled_at, expected, "Berlin noon is 11:00 UTC");
    assert!(
        status::home_timeline(&pool, alice.id, user::TimelineOrder::Published, None, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing was published"
    );
}

/// Scheduling honours the API's minimum offset: a time in the past re-renders
/// the composer with the validation banner and the draft (text and chosen
/// time) intact.
#[sqlx::test(migrations = "../db/migrations")]
async fn scheduling_in_the_past_reprompts_with_the_draft_kept(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "too late"),
            ("visibility", "public"),
            ("scheduled_at", "2001-01-01T00:00"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.body.contains("Scheduled at must be in the future"),
        "validation banner"
    );
    assert!(resp.body.contains("too late"), "draft text kept");
    assert!(
        resp.body.contains(r#"value="2001-01-01T00:00""#),
        "chosen time kept"
    );
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert!(
        scheduled_status::list_for_account(&pool, alice.id, None, None, None, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing queued"
    );
}

/// A crafted group submission with a schedule is refused server-side (the
/// group composer hides the field, but the guard must hold regardless).
#[sqlx::test(migrations = "../db/migrations")]
async fn group_posts_cannot_be_scheduled(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool).await;
    let cookie = login(&app).await;
    let gid = group_id.to_string();
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let resp = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "group trip"),
            ("group_id", &gid),
            ("scheduled_at", "2030-01-15T12:00"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("Group posts can't be scheduled."));
}

/// The scheduled listing shows the queue in the viewer's zone and its
/// reschedule / cancel verbs work (and are keyed to the owner's session).
#[sqlx::test(migrations = "../db/migrations")]
async fn scheduled_page_lists_reschedules_and_cancels(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let posted = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("status", "queued entry"),
            ("visibility", "unlisted"),
            ("scheduled_at", "2030-03-01T08:30"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);

    let page = get(&app, "/settings/scheduled?saved=scheduled", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Post scheduled."), "flash");
    assert!(page.body.contains("Mar 1, 2030, 08:30"), "local label");
    assert!(page.body.contains("queued entry"), "excerpt");
    assert!(page.body.contains("Unlisted"), "visibility label");
    assert!(page.body.contains(r#"value="2030-03-01T08:30""#), "prefill");

    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let row_id = scheduled_status::list_for_account(&pool, alice.id, None, None, None, 10)
        .await
        .unwrap()[0]
        .id;

    // Reschedule to a later slot…
    let resp = post_form(
        &app,
        &format!("/web/settings/scheduled/{row_id}/reschedule"),
        &cookie,
        &[("csrf", &csrf), ("scheduled_at", "2030-04-02T10:00")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let row = scheduled_status::find_for_account(&pool, alice.id, row_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.scheduled_at,
        time::macros::datetime!(2030-04-02 10:00 UTC)
    );

    // …a nonsense time is refused with the error flash…
    let resp = post_form(
        &app,
        &format!("/web/settings/scheduled/{row_id}/reschedule"),
        &cookie,
        &[("csrf", &csrf), ("scheduled_at", "not-a-time")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        resp.location.unwrap().contains("error="),
        "error flash redirect"
    );

    // …and cancel drops the row.
    let resp = post_form(
        &app,
        &format!("/web/settings/scheduled/{row_id}/cancel"),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        scheduled_status::find_for_account(&pool, alice.id, row_id)
            .await
            .unwrap()
            .is_none()
    );
    let page = get(&app, "/settings/scheduled", Some(&cookie)).await;
    assert!(page.body.contains("no scheduled posts"));
}

// ---- Announcements, user side -------------------------------------------

/// An unread announcement banners the home timeline with its cited posts
/// embedded and the shared progressively enhanced reaction picker; reacting
/// toggles a chip and dismissing collapses the banner to the "Announcements"
/// link, with the full page still listing it as read.
#[sqlx::test(migrations = "../db/migrations")]
async fn announcements_banner_react_and_dismiss(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let cited = status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>cited-in-announcement</p>", "public", None),
    )
    .await
    .unwrap();
    let ann = announcement::create(
        &pool,
        plamenu_db::announcement::NewAnnouncement {
            text: "Scheduled maintenance on Friday",
            status_ids: Some(&[cited.id]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(ann.published, "publishes on create");
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let home = get(&app, "/", Some(&cookie)).await;
    assert!(
        home.body.contains("Scheduled maintenance on Friday"),
        "banner content"
    );
    assert!(home.body.contains("Mark as read"), "dismiss verb");
    assert!(
        home.body.contains("data-announcement-dismiss"),
        "dismiss verb is eligible for in-place enhancement"
    );
    // The home timeline shows the post too, so pin the embed to the card
    // markup: no quotes exist in this test, only the announcement's citation.
    assert!(
        home.body.contains("quote-card") && home.body.contains("cited-in-announcement"),
        "cited post embeds under the content"
    );
    assert!(
        home.body.contains(&format!(
            r#"data-react-base="/web/announcements/{}""#,
            ann.id
        )),
        "the shared reaction picker, keyed to the announcement"
    );
    assert!(home.body.contains(&format!(
        r#"href="/web/announcements/{}/reaction?return_to=%2F""#,
        ann.id
    )));
    let csrf = csrf_of(&home.body);

    // The plain link opens the same complete picker as status cards, and its
    // path-less POST applies the clicked submit button.
    let picker = get(
        &app,
        &format!("/web/announcements/{}/reaction?return_to=%2F", ann.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(picker.status, StatusCode::OK);
    assert!(picker.body.contains("distorted face"));
    assert!(
        picker
            .body
            .contains(&format!(r#"action="/web/announcements/{}/react""#, ann.id))
    );
    let resp = post_form(
        &app,
        &format!("/web/announcements/{}/react", ann.id),
        &cookie,
        &[
            ("csrf", &csrf_of(&picker.body)),
            ("return_to", "/"),
            ("emoji", "👍"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(home.body.contains("👍"), "reaction chip");
    assert!(home.body.contains(r#"aria-pressed="true""#), "own reaction");

    // The fragment the script swaps in serves the same chip row.
    let frag = get(
        &app,
        &format!("/web/announcements/{}/reactions?return_to=/", ann.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(frag.status, StatusCode::OK);
    assert!(
        frag.body.contains(&format!(
            r#"data-reactions="/web/announcements/{}""#,
            ann.id
        )),
        "fragment row keyed to the announcement"
    );
    assert!(frag.body.contains("👍"), "fragment carries the chip");

    // Toggling it off through the chip's unreact path removes it.
    let resp = post_form(
        &app,
        &format!("/web/announcements/{}/unreact/%F0%9F%91%8D", ann.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", "/")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(!home.body.contains(r#"aria-pressed="true""#), "chip gone");

    // Dismiss: the banner collapses to the link, the page shows it read.
    let resp = post_form(
        &app,
        &format!("/web/announcements/{}/dismiss", ann.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", "/")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(
        !home.body.contains("Scheduled maintenance on Friday"),
        "banner gone once read"
    );
    assert!(
        home.body.contains(r#"href="/announcements""#),
        "collapsed to the listing link"
    );
    let page = get(&app, "/announcements", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Scheduled maintenance on Friday"));
    assert!(!page.body.contains("Mark as read"), "already read");
}

/// A home timeline with no announcements at all renders neither banner nor
/// link — the common case stays clean.
#[sqlx::test(migrations = "../db/migrations")]
async fn home_without_announcements_shows_nothing(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert!(!home.body.contains("announcements-banner"));
    assert!(!home.body.contains(r#"href="/announcements""#));
}

// ---------------------------------------------------------------------------
// Followed hashtags

/// The hashtag page's follow control follows and unfollows the tag, and the
/// relationships manager's Hashtags tab lists and bulk-unfollows it.
#[sqlx::test(migrations = "../db/migrations")]
async fn hashtag_follow_control_and_relationships_tab(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    // A never-used tag renders with a Follow control.
    let page = get(&app, "/tags/rustacean", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains("/web/tags/rustacean/follow"),
        "offers Follow"
    );
    let csrf = csrf_of(&page.body);

    // Following flips the button and creates the (local-only) follow.
    let resp = post_form(
        &app,
        "/web/tags/rustacean/follow",
        &cookie,
        &[("csrf", &csrf), ("return_to", "/tags/rustacean")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    // The follow handler honours `return_to` in its redirect.
    assert_eq!(resp.location.as_deref(), Some("/tags/rustacean"));
    let page = get(&app, "/tags/rustacean", Some(&cookie)).await;
    assert!(
        page.body.contains("/web/tags/rustacean/unfollow"),
        "offers Unfollow"
    );

    // The Hashtags tab lists the followed tag, linked to its timeline.
    let tab = get(&app, "/settings/relationships?rel=hashtags", Some(&cookie)).await;
    assert_eq!(tab.status, StatusCode::OK);
    assert!(tab.body.contains("#rustacean"), "tab lists the tag");
    assert!(tab.body.contains(r#"href="/tags/rustacean""#));

    // Bulk unfollow from the tab empties it and the button flips back.
    let resp = post_form(
        &app,
        "/web/settings/relationships",
        &cookie,
        &[
            ("csrf", &csrf),
            ("action", "unfollow_tag"),
            ("rel", "hashtags"),
            ("tags", "rustacean"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let tab = get(&app, "/settings/relationships?rel=hashtags", Some(&cookie)).await;
    assert!(tab.body.contains("You aren't following any hashtags."));
    let page = get(&app, "/tags/rustacean", Some(&cookie)).await;
    assert!(
        page.body.contains("/web/tags/rustacean/follow"),
        "back to Follow"
    );
}

// ---------------------------------------------------------------------------
// Private notes on profiles

/// The profile's private-note disclosure saves a note, shows it back open on
/// the next visit, and a whitespace-only save clears it. One's own profile
/// never offers the form.
#[sqlx::test(migrations = "../db/migrations")]
async fn private_note_saves_and_clears_on_the_profile(pool: PgPool) {
    let bob = create_local_account(&pool, "bob", "bob").await;
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;

    let page = get(&app, "/@bob", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Private note"), "offers the note form");
    assert!(
        page.body
            .contains(&format!("/web/accounts/{}/note", bob.id))
    );
    let csrf = csrf_of(&page.body);

    let resp = post_form(
        &app,
        &format!("/web/accounts/{}/note", bob.id),
        &cookie,
        &[
            ("csrf", &csrf),
            ("return_to", "/@bob"),
            ("comment", "met at the fediverse meetup"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let page = get(&app, "/@bob", Some(&cookie)).await;
    assert!(
        page.body.contains("met at the fediverse meetup"),
        "note round-trips"
    );
    assert!(
        page.body
            .contains(r#"class="follow-settings account-note" open"#),
        "a saved note renders the disclosure open"
    );

    // A whitespace-only save clears the note (API semantics).
    let resp = post_form(
        &app,
        &format!("/web/accounts/{}/note", bob.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", "/@bob"), ("comment", "   ")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let page = get(&app, "/@bob", Some(&cookie)).await;
    assert!(
        !page.body.contains("met at the fediverse meetup"),
        "note cleared"
    );

    // Never offered on one's own profile.
    let own = get(&app, "/@alice", Some(&cookie)).await;
    assert!(!own.body.contains("Private note"));
}

// ---------------------------------------------------------------------------
// Status translation on the thread page

/// The thread page offers Translate on a foreign-language focus post,
/// `?translate=1` renders the translated content with attribution and a
/// Show-original link, and a backend failure falls back with a notice.
#[sqlx::test(migrations = "../db/migrations")]
async fn thread_translate_link_translates_the_focus_post(pool: PgPool) {
    const ENDPOINT: &str = "http://libretranslate.test";
    seed_alice(&pool).await;
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let fed: std::sync::Arc<StubFederation> = std::sync::Arc::default();
    fed.serve_service(
        &format!("{ENDPOINT}/languages"),
        200,
        &serde_json::json!([
            { "code": "en", "name": "English", "targets": ["es"] },
            { "code": "es", "name": "Spanish", "targets": ["en"] },
        ])
        .to_string(),
    );
    fed.serve_service(
        &format!("{ENDPOINT}/translate"),
        200,
        &serde_json::json!({
            "translatedText": ["<p>Hello world</p>"],
            "detectedLanguage": [{ "confidence": 100, "language": "es" }],
        })
        .to_string(),
    );
    let app = build_router(common::test_state_translation(
        pool.clone(),
        fed.clone(),
        plamenu::config::TranslationConfig::LibreTranslate {
            endpoint: ENDPOINT.to_owned(),
            api_key: None,
        },
    ));
    let cookie = login(&app).await;

    let spanish = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>Hola mundo</p>",
            language: Some("es"),
            ..status::NewLocalStatus::new(alice.id, "<p>Hola mundo</p>", "public", None)
        },
    )
    .await
    .unwrap();

    // The focus post carries the Translate link; the untranslated content shows.
    let page = get(&app, &format!("/@alice/{}", spanish.id), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("data-translate"), "offers Translate");
    assert!(
        page.body
            .contains(&format!("/web/statuses/{}/translate", spanish.id)),
        "form posts to the translate endpoint"
    );
    assert!(page.body.contains("Hola mundo"));

    // `?translate=1` swaps the content and renders the attribution line.
    let translated = get(
        &app,
        &format!("/@alice/{}?translate=1", spanish.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(translated.status, StatusCode::OK);
    assert!(
        translated.body.contains("Hello world"),
        "translated content shows"
    );
    assert!(!translated.body.contains("Hola mundo"), "original replaced");
    assert!(
        translated.body.contains("Translated from"),
        "attribution shows"
    );
    assert!(translated.body.contains("LibreTranslate"));
    assert!(translated.body.contains("Show original"));

    // A post already in the viewer's language gets no offer.
    let english = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>plain english</p>",
            language: Some("en"),
            ..status::NewLocalStatus::new(alice.id, "<p>plain english</p>", "public", None)
        },
    )
    .await
    .unwrap();
    let page = get(&app, &format!("/@alice/{}", english.id), Some(&cookie)).await;
    assert!(
        !page.body.contains("data-translate"),
        "no offer on same-language post"
    );

    // A language the backend doesn't list gets no offer either — a
    // confident-but-wrong translation is worse than none.
    let finnish = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>Sää on tänään kamala</p>",
            language: Some("fi"),
            ..status::NewLocalStatus::new(alice.id, "<p>Sää on tänään kamala</p>", "public", None)
        },
    )
    .await
    .unwrap();
    let page = get(&app, &format!("/@alice/{}", finnish.id), Some(&cookie)).await;
    assert!(
        !page.body.contains("data-translate"),
        "no offer on an unsupported-language post"
    );

    // A failing backend degrades to the original with a notice.
    fed.serve_service(&format!("{ENDPOINT}/translate"), 500, "boom");
    let fresh = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>Otra cosa</p>",
            language: Some("es"),
            ..status::NewLocalStatus::new(alice.id, "<p>Otra cosa</p>", "public", None)
        },
    )
    .await
    .unwrap();
    let failed = get(
        &app,
        &format!("/@alice/{}?translate=1", fresh.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(failed.status, StatusCode::OK);
    assert!(
        failed
            .body
            .contains("The translation service is unavailable."),
        "specific backend-failure message shows"
    );
    assert!(failed.body.contains("Otra cosa"), "original still shows");
}

/// The per-post Translate form: a fetch POST returns the translation as JSON
/// for the in-place swap; a plain submit 303s to the translated permalink.
#[sqlx::test(migrations = "../db/migrations")]
async fn translate_button_posts_json_in_place(pool: PgPool) {
    const ENDPOINT: &str = "http://libretranslate.test";
    seed_alice(&pool).await;
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let fed: std::sync::Arc<StubFederation> = std::sync::Arc::default();
    fed.serve_service(
        &format!("{ENDPOINT}/languages"),
        200,
        &serde_json::json!([
            { "code": "en", "name": "English", "targets": ["es"] },
            { "code": "es", "name": "Spanish", "targets": ["en"] },
        ])
        .to_string(),
    );
    fed.serve_service(
        &format!("{ENDPOINT}/translate"),
        200,
        &serde_json::json!({
            "translatedText": ["<p>Hello world</p>"],
            "detectedLanguage": [{ "confidence": 100, "language": "es" }],
        })
        .to_string(),
    );
    let app = build_router(common::test_state_translation(
        pool.clone(),
        fed,
        plamenu::config::TranslationConfig::LibreTranslate {
            endpoint: ENDPOINT.to_owned(),
            api_key: None,
        },
    ));
    let cookie = login(&app).await;
    let spanish = status::create_local(
        &pool,
        status::NewLocalStatus {
            content: "<p>Hola mundo</p>",
            language: Some("es"),
            ..status::NewLocalStatus::new(alice.id, "<p>Hola mundo</p>", "public", None)
        },
    )
    .await
    .unwrap();
    let csrf = csrf_of(&get(&app, "/", Some(&cookie)).await.body);

    // JS path: X-Requested-With gets JSON for the in-place swap.
    let request = Request::builder()
        .method("POST")
        .uri(format!("/web/statuses/{}/translate", spanish.id))
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("x-requested-with", "fetch")
        .body(Body::from(format!("csrf={csrf}")))
        .unwrap();
    let response = send(&app, request).await;
    assert_eq!(response.status, StatusCode::OK);
    let data: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(data["content"], "<p>Hello world</p>");
    assert!(
        data["attribution"]
            .as_str()
            .unwrap()
            .contains("Translated from"),
        "{data}"
    );
    assert!(
        data["attribution"]
            .as_str()
            .unwrap()
            .contains("LibreTranslate")
    );

    // No-JS path: the same POST without the header redirects to the
    // server-rendered translated permalink.
    let posted = post_form(
        &app,
        &format!("/web/statuses/{}/translate", spanish.id),
        &cookie,
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    assert_eq!(
        posted.location.as_deref(),
        Some(format!("/@alice/{}?translate=1", spanish.id).as_str())
    );
}

/// The dedicated translate-to preference: stores, round-trips through the
/// form, and empty falls back to the posting language.
#[sqlx::test(migrations = "../db/migrations")]
async fn translate_language_preference_round_trips(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let page = get(&app, "/settings/languages", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("reading_translate_language"));
    assert!(page.body.contains("Same as default posting language"));

    let csrf = csrf_of(&page.body);
    let posted = post_form(
        &app,
        "/web/settings/languages",
        &cookie,
        &[
            ("csrf", &csrf),
            ("posting_default_language", "en"),
            ("locale", "en"),
            ("reading_translate_language", "de"),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);

    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let settings = plamenu_db::user::settings_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settings.reading_translate_language.as_deref(), Some("de"));
    assert_eq!(settings.translate_language(), "de");

    // Clearing it (empty submit) reverts to following the posting language.
    let posted = post_form(
        &app,
        "/web/settings/languages",
        &cookie,
        &[
            ("csrf", &csrf),
            ("posting_default_language", "en"),
            ("locale", "en"),
            ("reading_translate_language", ""),
        ],
    )
    .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    let settings = plamenu_db::user::settings_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settings.reading_translate_language, None);
    assert_eq!(settings.translate_language(), "en");
}

// ---- Explore: trends, suggestions, directory ---------------------------

/// Seeds an allowed trending post, hashtag (two users talking today) and
/// link, bypassing the ranking engine — the pages read the trend tables the
/// same way the API routes do.
async fn seed_explore_trends(pool: &PgPool) -> i64 {
    let today = time::OffsetDateTime::now_utc().date();
    let author = create_local_account(pool, "author", "Author").await;
    let post = status::create_local(
        pool,
        status::NewLocalStatus {
            language: Some("en"),
            ..status::NewLocalStatus::new(author.id, "<p>hot take</p>", "public", None)
        },
    )
    .await
    .unwrap();
    status_trend::upsert(pool, post.id, author.id, 16.0, Some("en"), true)
        .await
        .unwrap();

    let tag_id = tag::ensure(pool, "plamenu").await.unwrap();
    for i in 0..2 {
        let fan = create_local_account(pool, &format!("tagfan{i}"), "fan").await;
        let tagged = status::create_local(
            pool,
            status::NewLocalStatus::new(fan.id, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();
        tag::attach(pool, tagged.id, tag_id).await.unwrap();
        tag::record_uses(pool, tagged.id, today).await.unwrap();
    }
    tag_trend::upsert(pool, tag_id, 25.0, true).await.unwrap();

    let card = preview_card::upsert(
        pool,
        preview_card::NewPreviewCard {
            url: "https://news.example/story",
            title: "Big News",
            description: "Something happened",
            kind: "link",
            provider_name: "Example News",
            language: Some("en"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    preview_card_trend::upsert(pool, card.id, 9.0, Some("en"), true)
        .await
        .unwrap();
    tag_id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explore_lists_trending_posts_tags_and_links(pool: PgPool) {
    seed_alice(&pool).await;
    seed_explore_trends(&pool).await;
    common::open_previews(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let posts = get(&app, "/explore", Some(&cookie)).await;
    assert_eq!(posts.status, StatusCode::OK);
    assert!(posts.body.contains("hot take"), "trending post renders");
    // The section selector reaches every tab; People moved to its own page.
    for href in ["/explore/hashtags", "/explore/links"] {
        assert!(posts.body.contains(href), "tab link {href}");
    }
    assert!(
        !posts.body.contains("/explore/people"),
        "People is no longer a Trending tab"
    );

    let tags = get(&app, "/explore/hashtags", Some(&cookie)).await;
    assert_eq!(tags.status, StatusCode::OK);
    assert!(tags.body.contains("#plamenu"));
    assert!(tags.body.contains("2 people in the past 2 days"));
    assert!(
        tags.body.contains("/web/tags/plamenu/follow"),
        "signed-in viewers get a follow control"
    );

    let links = get(&app, "/explore/links", Some(&cookie)).await;
    assert_eq!(links.status, StatusCode::OK);
    assert!(links.body.contains("Big News"));
    assert!(links.body.contains("Example News"));

    // Anonymous visitors browse the same pages while previews are open.
    let anon = get(&app, "/explore", None).await;
    assert_eq!(anon.status, StatusCode::OK);
    assert!(anon.body.contains("hot take"));
    let anon_tags = get(&app, "/explore/hashtags", None).await;
    assert!(
        !anon_tags.body.contains("/web/tags/plamenu/follow"),
        "no follow control when signed out"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explore_trends_disabled_shows_hint(pool: PgPool) {
    seed_alice(&pool).await;
    seed_explore_trends(&pool).await;
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    let mut update = current.as_update();
    update.trends_enabled = false;
    plamenu_db::instance_settings::save(&pool, update)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    for uri in ["/explore", "/explore/hashtags", "/explore/links"] {
        let page = get(&app, uri, Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK);
        assert!(
            page.body.contains("Trends are disabled on this server."),
            "{uri} explains itself"
        );
        assert!(!page.body.contains("hot take"), "{uri} hides trend rows");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explore_people_suggests_follows_and_lists_directory(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    sqlx::query!(
        "UPDATE accounts SET discoverable = true, note = '<p>I build things</p>' WHERE id = $1",
        bob.id
    )
    .execute(&pool)
    .await
    .unwrap();
    // alice → carol → bob: bob becomes a friends-of-friends suggestion.
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();
    follow::create(&pool, carol.id, bob.id, None).await.unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/people", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Suggested for you"));
    assert!(page.body.contains("Followed by people you follow"));
    assert!(
        page.body
            .contains(&format!("/web/accounts/{}/follow", bob.id)),
        "suggestion offers Follow"
    );
    assert!(
        page.body
            .contains(&format!("/web/suggestions/{}/dismiss", bob.id)),
        "suggestion offers Dismiss"
    );
    // The directory below lists discoverable accounts with their bio. Order
    // and scope fold into one selector for a signed-in viewer.
    assert!(page.body.contains("I build things"));
    for control in [
        "Recently active — everywhere",
        "New arrivals — everywhere",
        "Recently active — this server",
        "New arrivals — this server",
    ] {
        assert!(page.body.contains(control), "directory control {control}");
    }

    // Dismissing bob suppresses him permanently; with no candidate left the
    // block disappears while the directory stays.
    let csrf = csrf_of(&page.body);
    let dismissed = post_form(
        &app,
        &format!("/web/suggestions/{}/dismiss", bob.id),
        &cookie,
        &[("csrf", &csrf), ("return_to", "/people")],
    )
    .await;
    assert_eq!(dismissed.status, StatusCode::SEE_OTHER);
    let after = get(&app, "/people", Some(&cookie)).await;
    assert!(!after.body.contains("Suggested for you"));
    assert!(
        after.body.contains("I build things"),
        "directory unaffected"
    );

    // The local-only scope still lists local bob.
    let local = get(&app, "/people?scope=local", Some(&cookie)).await;
    assert!(local.body.contains("I build things"));

    // The old Explore tab URL redirects permanently, keeping its query.
    let legacy = get(&app, "/explore/people?scope=local", Some(&cookie)).await;
    assert_eq!(legacy.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(legacy.location.as_deref(), Some("/people?scope=local"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explore_directory_disabled_shows_hint(pool: PgPool) {
    seed_alice(&pool).await;
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    let mut update = current.as_update();
    update.profile_directory = false;
    plamenu_db::instance_settings::save(&pool, update)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/people", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains("The profile directory is disabled on this server.")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn discovery_pages_gated_for_anonymous_when_knobs_off(pool: PgPool) {
    seed_alice(&pool).await;
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            anon_trends: false,
            anon_directory: false,
            anon_groups: false,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    let app = common::test_app_private(pool.clone());
    for uri in [
        "/explore",
        "/explore/hashtags",
        "/explore/links",
        "/people",
        "/groups",
    ] {
        let resp = get(&app, uri, None).await;
        assert_eq!(resp.status, StatusCode::SEE_OTHER, "{uri} bounces");
        assert_eq!(resp.location.as_deref(), Some("/login"), "{uri} to login");
    }
}

/// POSTs a urlencoded form with no session — the remote-interaction
/// interstitial is an anonymous flow.
async fn post_anon_form(app: &Router, uri: &str, fields: &[(&str, &str)]) -> Resp {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_interaction_interstitial_flow(pool: PgPool) {
    seed_alice(&pool).await;
    let stub = StubFederation::with_users(&[]);
    stub.webfinger.lock().unwrap().insert(
        "carol@remote.example".to_owned(),
        vec![plamenu_federation::WebfingerCandidate {
            actor_uri: "https://remote.example/users/carol".to_owned(),
            advertised_type: None,
        }],
    );
    stub.subscribe_templates.lock().unwrap().insert(
        "carol@remote.example".to_owned(),
        "https://remote.example/authorize_interaction?uri={uri}".to_owned(),
    );
    let app = common::test_app_with(pool.clone(), stub);
    let alice_uri = "https://plamenu.test/users/alice";
    let interact_path = format!(
        "/interact?{}",
        serde_urlencoded::to_string([("uri", alice_uri)]).unwrap()
    );

    // The anonymous profile page offers the interstitial Follow affordance.
    let profile = get(&app, "/@alice", None).await;
    assert_eq!(profile.status, StatusCode::OK);
    assert!(profile.body.contains("/interact?uri="), "follow affordance");

    // The interstitial names the subject and carries the handle form.
    let page = get(&app, &interact_path, None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Continue on your own server"));
    assert!(page.body.contains("@alice"), "subject lead-in");

    // A handle without a domain re-renders with an explanation.
    let bad = post_anon_form(
        &app,
        "/interact",
        &[("uri", alice_uri), ("handle", "carol")],
    )
    .await;
    assert_eq!(bad.status, StatusCode::OK);
    assert!(bad.body.contains("does not look like a full handle"));

    // A handle from this very server just needs a session.
    let local = post_anon_form(
        &app,
        "/interact",
        &[("uri", alice_uri), ("handle", "@bob@plamenu.test")],
    )
    .await;
    assert_eq!(local.status, StatusCode::SEE_OTHER);
    assert_eq!(local.location.as_deref(), Some("/login"));

    // A resolvable remote handle is sent home with the URI substituted in.
    let remote = post_anon_form(
        &app,
        "/interact",
        &[("uri", alice_uri), ("handle", "carol@remote.example")],
    )
    .await;
    assert_eq!(remote.status, StatusCode::SEE_OTHER);
    assert_eq!(
        remote.location.as_deref(),
        Some(
            "https://remote.example/authorize_interaction?uri=https%3A%2F%2Fplamenu.test%2Fusers%2Falice"
        )
    );

    // A home server without the subscribe rel gets a graceful explanation.
    let stub2 = StubFederation::with_users(&[]);
    stub2.webfinger.lock().unwrap().insert(
        "dave@bare.example".to_owned(),
        vec![plamenu_federation::WebfingerCandidate {
            actor_uri: "https://bare.example/users/dave".to_owned(),
            advertised_type: None,
        }],
    );
    let app2 = common::test_app_with(pool.clone(), stub2);
    let no_template = post_anon_form(
        &app2,
        "/interact",
        &[("uri", alice_uri), ("handle", "dave@bare.example")],
    )
    .await;
    assert_eq!(no_template.status, StatusCode::OK);
    assert!(
        no_template
            .body
            .contains("does not advertise a remote-interaction endpoint")
    );

    // A signed-in local skips the interstitial and lands on the local view.
    let cookie = login(&app).await;
    let signed_in = get(&app, &interact_path, Some(&cookie)).await;
    assert_eq!(signed_in.status, StatusCode::SEE_OTHER);
    assert_eq!(signed_in.location.as_deref(), Some("/@alice"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn groups_directory_readable_anonymously(pool: PgPool) {
    let app = app_with_alice(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/groups/new", Some(&cookie)).await.body);
    let created = post_form(
        &app,
        "/web/groups",
        &cookie,
        &[
            ("csrf", &csrf),
            ("name", "hiking"),
            ("display_name", "Hiking"),
            ("membership_policy", "open"),
            ("posting_policy", "members"),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);

    // The directory reads anonymously under the default anon_groups posture
    // without the signed-in blocks.
    let anon = get(&app, "/groups", None).await;
    assert_eq!(anon.status, StatusCode::OK);
    assert!(anon.body.contains("hiking"), "directory lists the group");
    assert!(!anon.body.contains("Your groups"));
    assert!(!anon.body.contains("Create group"));

    // An empty later page says so instead of repeating the first.
    let paged = get(&app, "/groups?offset=100", None).await;
    assert_eq!(paged.status, StatusCode::OK);
    assert!(paged.body.contains("No more groups."));

    // With the groups knob off the directory is sign-in-gated.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            anon_groups: false,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    let private = common::test_app_private(pool.clone());
    let bounced = get(&private, "/groups", None).await;
    assert_eq!(bounced.status, StatusCode::SEE_OTHER);
    assert_eq!(bounced.location.as_deref(), Some("/login"));
}

/// The reply composer opens with the thread's handles pasted in (Mastodon's
/// `statusToTextMentions` convention): the parent's author plus its active
/// mentions, minus the viewer. Without them the reply carries no Mention tags
/// and Mastodon-lineage recipients are never notified of it.
#[sqlx::test(migrations = "../db/migrations")]
async fn reply_composer_prefills_thread_mentions(pool: PgPool) {
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    create_local_account(&pool, "carol", "Carol").await;
    let app = build_router(state.clone());

    // Bob's post mentions carol: alice's reply must open addressing both.
    let (parent, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "bob",
            text: "hello @carol",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let cookie = login(&app).await;
    let thread = get(&app, &format!("/@bob/{}", parent.id), Some(&cookie)).await;
    assert_eq!(thread.status, StatusCode::OK);
    assert!(
        thread.body.contains("@bob @carol </textarea>"),
        "inline reply prefills author + thread mentions"
    );

    // The full composer (`/compose?reply=`) prefills the same way…
    let composer = get(
        &app,
        &format!("/compose?reply={}", parent.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(composer.status, StatusCode::OK);
    for page in [&thread, &composer] {
        assert!(!page.body.contains("data-compose-kind"));
        assert!(!page.body.contains("compose__compatibility"));
        assert!(!page.body.contains("name=\"event_start\""));
        assert!(
            page.body
                .contains(r#"type="hidden" name="post_kind" value="note""#)
        );
    }
    let csrf = csrf_of(&composer.body);
    for kind in ["article", "event"] {
        let refused = post_multipart(
            &app,
            "/web/compose",
            &cookie,
            &[
                ("csrf", &csrf),
                ("post_kind", kind),
                ("in_reply_to_id", &parent.id.to_string()),
                ("title", "Reply title"),
                ("status", "Keep this reply"),
            ],
        )
        .await;
        assert_eq!(refused.status, StatusCode::OK);
        assert!(refused.body.contains("Replies use the Note post type."));
        assert!(refused.body.contains("Keep this reply</textarea>"));
        assert!(!refused.body.contains("data-compose-kind"));
    }
    assert!(
        composer.body.contains("@bob @carol </textarea>"),
        "full reply composer prefills author + thread mentions"
    );

    // …unless a redraft brought its own text, which wins.
    let redraft = get(
        &app,
        &format!("/compose?reply={}&text=my+own+words", parent.id),
        Some(&cookie),
    )
    .await;
    assert!(redraft.body.contains("my own words</textarea>"));
    assert!(!redraft.body.contains("@bob @carol"));

    // Replying to your own post prefills nobody (the composer's one textarea
    // is empty), rather than pasting your own handle.
    let (own, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "talking to myself",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let own_thread = get(&app, &format!("/@alice/{}", own.id), Some(&cookie)).await;
    assert!(
        own_thread.body.contains("></textarea>"),
        "self-reply prefills nothing"
    );
}

/// Both reply entry points inherit a declared language from the post being
/// answered instead of retaining the viewer's default posting language.
#[sqlx::test(migrations = "../db/migrations")]
async fn reply_composers_inherit_the_parent_language(pool: PgPool) {
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = build_router(state.clone());

    // Alice's posting default is English; the French parent should displace it.
    let (parent, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "bob",
            text: "bonjour",
            visibility: "public",
            language: Some("fr"),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let cookie = login(&app).await;
    let selected = r#"<option value="fr" selected>Français (French)</option>"#;

    let thread = get(&app, &format!("/@bob/{}", parent.id), Some(&cookie)).await;
    assert_eq!(thread.status, StatusCode::OK);
    assert!(
        thread.body.contains(selected),
        "inline reply composer should inherit the parent's language"
    );

    let composer = get(
        &app,
        &format!("/compose?reply={}", parent.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(composer.status, StatusCode::OK);
    assert!(
        composer.body.contains(selected),
        "full reply composer should inherit the parent's language"
    );
}

/// Both reply entry points enable and prefill the parent's content warning.
/// The value remains an ordinary editable field, so the author can change or
/// clear it before posting.
#[sqlx::test(migrations = "../db/migrations")]
async fn reply_composers_inherit_the_parent_content_warning(pool: PgPool) {
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    seed_alice(&pool).await;
    seed_user(&pool, "bob", "bob@example.com", PASSWORD).await;
    let app = build_router(state.clone());

    let (parent, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "bob",
            text: "the ending is discussed here",
            visibility: "public",
            spoiler_text: "Ending spoilers",
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let cookie = login(&app).await;
    let expected = r#"name="spoiler_text" data-compose-spoiler value="Ending spoilers""#;

    let thread = get(&app, &format!("/@bob/{}", parent.id), Some(&cookie)).await;
    assert_eq!(thread.status, StatusCode::OK);
    assert!(
        thread.body.contains(expected),
        "inline reply composer should inherit the parent's content warning"
    );

    let composer = get(
        &app,
        &format!("/compose?reply={}", parent.id),
        Some(&cookie),
    )
    .await;
    assert_eq!(composer.status, StatusCode::OK);
    assert!(
        composer.body.contains(expected),
        "full reply composer should inherit the parent's content warning"
    );

    // A redraft's explicit CW remains authoritative even when it is also a
    // reply, matching the existing text and visibility precedence rules.
    let redraft = get(
        &app,
        &format!("/compose?reply={}&cw=Replacement+warning", parent.id),
        Some(&cookie),
    )
    .await;
    assert!(
        redraft
            .body
            .contains(r#"name="spoiler_text" data-compose-spoiler value="Replacement warning""#)
    );
}

/// The RSVP cluster on an event post (E2), rendered rather than merely stored.
///
/// Every state here is a *sentence* the viewer has to be able to act on, and
/// three of them carry no button at all — an invite-only event, a full one and
/// one happening on the origin's own site each send the viewer somewhere
/// different, so they must not collapse into one greyed-out control.
#[sqlx::test(migrations = "../db/migrations")]
async fn event_posts_render_an_rsvp_cluster_per_join_mode(pool: PgPool) {
    seed_alice(&pool).await;
    let organizer = create_local_account(&pool, "grace", "Grace").await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // One event per join mode, each rendered on alice's home timeline.
    let mut paths = Vec::new();
    let mut free_id = None;
    for (mode, capacity) in [
        ("free", None),
        ("restricted", None),
        ("invite", None),
        ("external", None),
        ("free", Some(0_i32)),
    ] {
        // `object_type` matters: a status carrying an event sidecar must be typed
        // `Event`, and both production paths (local authoring and inbound ingest)
        // set it. The render path relies on that to skip the sidecar query for the
        // ordinary timeline.
        let post = status::create_local(
            &pool,
            status::NewLocalStatus {
                object_type: Some("Event"),
                ..status::NewLocalStatus::new(
                    organizer.id,
                    &format!("<p>event {mode}</p>"),
                    "public",
                    None,
                )
            },
        )
        .await
        .unwrap();
        let mut sidecar = plamenu_db::status_event::StatusEvent::empty(post.id);
        sidecar.join_mode = Some(mode.to_owned());
        sidecar.max_attendees = capacity;
        if capacity.is_some() {
            sidecar.remaining_attendees = Some(0);
        }
        if mode == "external" {
            sidecar.external_participation_url = Some("https://tickets.example/e/1".to_owned());
        }
        plamenu_db::status_event::upsert(&pool, &sidecar)
            .await
            .unwrap();
        if mode == "free" && capacity.is_none() {
            free_id = Some(post.id);
        }
        // The canonical thread path — `/web/statuses/{id}` only redirects here.
        paths.push((mode, capacity.is_some(), format!("/@grace/{}", post.id)));
    }

    for (mode, full, path) in paths {
        let body = get(&app, &path, Some(&cookie)).await.body;
        let has_attend_form = body.contains("/participate\"");
        match (mode, full) {
            ("free" | "restricted", false) => {
                assert!(has_attend_form, "{mode} must offer an RSVP form:\n{body}");
            }
            ("invite", _) => {
                assert!(!has_attend_form, "an uninvited viewer gets no button");
                assert!(body.contains("By invitation only"), "{body}");
            }
            ("external", _) => {
                assert!(!has_attend_form, "external attendance sends no activity");
                assert!(
                    body.contains("https://tickets.example/e/1"),
                    "the external RSVP link is offered instead:\n{body}"
                );
            }
            (_, true) => {
                assert!(!has_attend_form, "a full event offers no RSVP");
                assert!(body.contains("This event is full"), "{body}");
            }
            _ => unreachable!(),
        }
    }

    // The JS path POSTs once and refreshes just the RSVP cluster. A free local
    // event resolves immediately to Going, with a cancel control in the fresh
    // fragment and no surrounding page shell.
    let free_id = free_id.expect("free event");
    let path = format!("/@grace/{free_id}");
    let thread = get(&app, &path, Some(&cookie)).await;
    let csrf = csrf_of(&thread.body);
    let response = post_form(
        &app,
        &format!("/web/statuses/{free_id}/participate"),
        &cookie,
        &[("csrf", &csrf), ("return_to", &path)],
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let fragment = get(
        &app,
        &format!("/web/statuses/{free_id}/rsvp?return_to={path}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(fragment.status, StatusCode::OK);
    assert!(fragment.body.contains("data-rsvp="));
    assert!(fragment.body.contains("You're going"), "{}", fragment.body);
    assert!(fragment.body.contains("/unparticipate"));
    assert!(
        !fragment.body.contains("<title"),
        "fragment leaked page shell"
    );
}

/// A consumed community's own rules drive its page: a mods-only Lemmy
/// community offers no composer to a stranger, states what it published about
/// itself, and lists the moderators we mirrored from its collection.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_community_facts_drive_the_group_page(pool: PgPool) {
    let state = common::test_state_with(pool, StubFederation::with_actors([]));
    let alice = seed_alice(&state.pool).await;
    let mut group = RemoteUser::new("groups.example", "rustlang");
    "Group".clone_into(&mut group.actor.kind);
    group.actor.sensitive = Some(true);
    group.actor.posting_restricted_to_mods = Some(true);
    let community = remote::store_remote_actor(&state.pool, &group.actor)
        .await
        .unwrap();
    let bob = RemoteUser::new("groups.example", "bob");
    let bob_id = remote::store_remote_actor(&state.pool, &bob.actor)
        .await
        .unwrap()
        .id;
    plamenu_db::remote_group::set_moderators(&state.pool, community.id, &[bob_id])
        .await
        .unwrap();
    let app = build_router(state.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/@rustlang@groups.example", Some(&cookie)).await;
    assert!(
        !page.body.contains("/compose?group="),
        "a mods-only community offers no composer to a non-moderator:\n{}",
        page.body
    );
    assert!(
        page.body.contains("Only moderators can start threads."),
        "the published restriction is stated, not silently applied:\n{}",
        page.body
    );
    assert!(
        page.body.contains("Marked sensitive by its moderators."),
        "the community-wide NSFW flag is surfaced:\n{}",
        page.body
    );
    assert!(
        page.body.contains("Moderators (1)") && page.body.contains("bob@groups.example"),
        "the mirrored roster is shown:\n{}",
        page.body
    );

    // The same page for someone the community *does* list as a moderator.
    plamenu_db::remote_group::set_moderators(&state.pool, community.id, &[bob_id, alice.id])
        .await
        .unwrap();
    let page = get(&app, "/@rustlang@groups.example", Some(&cookie)).await;
    assert!(
        page.body.contains("/compose?group="),
        "a moderator of the community gets the composer:\n{}",
        page.body
    );
}

/// The members-only policy only a Plamenu peer can state: a follower may post,
/// a stranger may not. Lemmy's boolean cannot carry this, which is why the
/// actor also publishes our own `postingPolicy` term.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_members_only_community_offers_the_composer_to_members(pool: PgPool) {
    let state = common::test_state_with(pool, StubFederation::with_actors([]));
    let alice = seed_alice(&state.pool).await;
    let mut group = RemoteUser::new("plamenu2.example", "hiking");
    "Group".clone_into(&mut group.actor.kind);
    group.actor.posting_restricted_to_mods = Some(false);
    group.actor.posting_policy = Some("members".to_owned());
    let community = remote::store_remote_actor(&state.pool, &group.actor)
        .await
        .unwrap();
    let app = build_router(state.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/@hiking@plamenu2.example", Some(&cookie)).await;
    assert!(
        !page.body.contains("/compose?group="),
        "a members-only community offers no composer to a non-member:\n{}",
        page.body
    );

    // Membership is our accepted follow of the community, exactly as for a
    // hosted group.
    follow::create(&state.pool, alice.id, community.id, None)
        .await
        .unwrap();
    follow::mark_accepted(&state.pool, alice.id, community.id)
        .await
        .unwrap();
    let page = get(&app, "/@hiking@plamenu2.example", Some(&cookie)).await;
    assert!(
        page.body.contains("/compose?group="),
        "a member gets the composer:\n{}",
        page.body
    );
}

/// The group console can set what every other server reads off a community's
/// profile: avatar, banner (with alt text) and metadata fields, plus a
/// sidebar-length description. A group is a local account with no `users` row,
/// so `update_credentials` can never reach it — this form is the only path.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_console_edits_the_communitys_profile(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool.clone()).await;
    let cookie = login(&app).await;
    let manage = get(&app, &format!("/groups/{group_id}/manage"), Some(&cookie)).await;
    assert!(
        manage.body.contains(r#"enctype="multipart/form-data""#),
        "the settings form must carry uploads:\n{}",
        manage.body
    );
    let csrf = csrf_of(&manage.body);
    let rules = "Be kind.\nNo spam.";

    let saved = post_multipart_file(
        &app,
        &format!("/web/groups/{group_id}/settings"),
        &cookie,
        &[
            ("csrf", &csrf),
            ("display_name", "Hiking & trails"),
            ("note", rules),
            ("avatar_description", "a purple square"),
            ("membership_policy", "open"),
            ("posting_policy", "members"),
            ("discoverable", "1"),
            ("fields_attributes[0][name]", "Rules"),
            ("fields_attributes[0][value]", "https://example.com/rules"),
        ],
        ("avatar", "square.png", "image/png", &sample_png_bytes()),
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);

    let group = account::find_by_id(&pool, group_id).await.unwrap().unwrap();
    assert!(
        group.avatar_file_name.is_some(),
        "the uploaded avatar is stored on the group account"
    );
    assert_eq!(group.avatar_description, "a purple square");
    assert_eq!(group.note_source, rules);

    // And it reaches the wire, which is the whole point: a peer fetching the
    // community sees the avatar, the alt text and the fields.
    let actor = get_ap(&app, "/users/hiking").await;
    let doc: serde_json::Value = serde_json::from_str(&actor.body).unwrap();
    assert_eq!(doc["type"], "Group");
    assert!(
        doc["icon"]["url"]
            .as_str()
            .is_some_and(|u| u.contains("/media/")),
        "icon published: {}",
        doc["icon"]
    );
    assert_eq!(doc["icon"]["summary"], "a purple square");
    assert_eq!(doc["attachment"][0]["name"], "Rules");
    assert_eq!(doc["attachment"][0]["type"], "PropertyValue");
    // The members-only policy Lemmy's boolean cannot express.
    assert_eq!(doc["postingRestrictedToMods"], false);
    assert_eq!(doc["postingPolicy"], "members");
}

/// The description is a community sidebar, so it takes far more than a bio —
/// but not without limit.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_description_is_capped_not_unbounded(pool: PgPool) {
    let (app, group_id) = app_with_alice_owning_group(pool.clone()).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(
        &get(&app, &format!("/groups/{group_id}/manage"), Some(&cookie))
            .await
            .body,
    );
    let post = async |note: &str| {
        post_multipart(
            &app,
            &format!("/web/groups/{group_id}/settings"),
            &cookie,
            &[
                ("csrf", &csrf),
                ("display_name", "Hiking"),
                ("note", note),
                ("membership_policy", "open"),
                ("posting_policy", "members"),
            ],
        )
        .await
    };

    // A real sidebar (well past a 500-character bio) is accepted.
    let sidebar = "rule ".repeat(300);
    assert_eq!(post(&sidebar).await.status, StatusCode::SEE_OTHER);
    let group = account::find_by_id(&pool, group_id).await.unwrap().unwrap();
    assert_eq!(group.note_source, sidebar.trim());

    // Past the cap it is refused with a readable flash, and nothing is written.
    let flood = "x".repeat(2001);
    let refused = post(&flood).await;
    assert_eq!(refused.status, StatusCode::SEE_OTHER);
    assert!(
        refused
            .location
            .as_deref()
            .is_some_and(|l| l.contains("error=")),
        "an over-long description comes back as a flash, not a 500"
    );
    let group = account::find_by_id(&pool, group_id).await.unwrap().unwrap();
    assert_eq!(
        group.note_source,
        sidebar.trim(),
        "the stored text is intact"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn composer_event_uses_selected_zone_and_retains_draft(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    sqlx::query("UPDATE users SET time_zone = 'Asia/Tbilisi' WHERE account_id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let page = get(&app, "/compose", Some(&cookie)).await;
    assert!(page.body.contains(r#"value="Asia/Tbilisi" selected"#));
    let csrf = csrf_of(&page.body);
    let mut fields = vec![
        ("csrf", csrf.as_str()),
        ("op", "preview"),
        ("post_kind", "event"),
        ("title", "Winter meeting"),
        ("status", "Bring your questions."),
        ("visibility", "public"),
        ("event_start", "2030-01-15T18:30"),
        ("event_end", "2030-01-15T20:00"),
        ("event_timezone", "Europe/Berlin"),
        ("event_join_mode", "external"),
        ("event_external_url", "https://example.com/attend"),
        ("event_capacity", "42"),
        ("event_status", "TENTATIVE"),
        ("event_online", "true"),
        ("event_location", "Community Hall"),
        ("event_street", "Main Street 12"),
        ("event_locality", "Berlin"),
        ("event_region", "Berlin"),
        ("event_country", "Germany"),
        ("event_postal_code", "10115"),
    ];
    let preview = post_multipart(&app, "/web/compose", &cookie, &fields).await;
    assert_eq!(preview.status, StatusCode::OK);
    assert!(
        preview.body.contains("compose__preview-heading"),
        "{}",
        preview.body
    );
    assert!(preview.body.contains(r#"value="event" selected"#));
    assert!(
        preview
            .body
            .contains(r#"value="Europe/Berlin" selected>(UTC+01:00) Europe/Berlin"#)
    );
    assert!(
        preview.body.contains("2030-01-15T17:30:00Z"),
        "preview uses selected zone"
    );
    for (name, value) in &fields {
        if name.starts_with("event_")
            && !matches!(
                *name,
                "event_online" | "event_timezone" | "event_status" | "event_join_mode"
            )
        {
            assert!(
                preview
                    .body
                    .contains(&format!(r#"name="{name}" value="{value}""#)),
                "lost {name}"
            );
        }
    }
    assert!(
        preview
            .body
            .contains(r#"name="event_online" value="true" checked"#)
    );
    assert!(preview.body.contains(r#"value="TENTATIVE" selected"#));
    assert!(preview.body.contains(r#"value="external" selected"#));
    // A failed publish must preserve the same complete draft.
    fields[1] = ("op", "post");
    fields[7] = ("event_end", "2030-01-15T17:00");
    let failed = post_multipart(&app, "/web/compose", &cookie, &fields).await;
    assert_eq!(failed.status, StatusCode::OK);
    assert!(
        failed
            .body
            .contains("Event end time cannot be before its start time")
    );
    assert!(failed.body.contains(r#"value="event" selected"#));
    assert!(
        failed
            .body
            .contains(r#"name="event_location" value="Community Hall""#)
    );
    fields[7] = ("event_end", "2030-01-15T20:00");
    let posted = post_multipart(&app, "/web/compose", &cookie, &fields).await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER, "{}", posted.body);
    let id: i64 = posted
        .location
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let row = plamenu_db::status_event::find(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.timezone.as_deref(), Some("Europe/Berlin"));
    assert_eq!(row.start_time.unwrap().hour(), 17);
    assert_eq!(row.start_time.unwrap().minute(), 30);
    assert_eq!(row.end_time.unwrap().hour(), 19);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn composer_event_rejects_bad_zone_and_dst_times(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    for (zone, start, message) in [
        ("Mars/Olympus", "2030-01-15T18:30", "Select a time zone"),
        (
            "Europe/Berlin",
            "2030-03-31T02:30",
            "skipped or occurs twice",
        ),
        (
            "Europe/Berlin",
            "2030-10-27T02:30",
            "skipped or occurs twice",
        ),
    ] {
        let response = post_multipart(
            &app,
            "/web/compose",
            &cookie,
            &[
                ("csrf", &csrf),
                ("post_kind", "event"),
                ("title", "Clock test"),
                ("status", "Description"),
                ("event_start", start),
                ("event_timezone", zone),
            ],
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(
            response.body.contains(message),
            "{zone} {start}: {}",
            response.body
        );
        assert!(response.body.contains(r#"value="event" selected"#));
        assert!(
            response
                .body
                .contains(&format!(r#"name="event_start" value="{start}""#))
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn composer_type_change_and_article_preview_keep_title_and_body(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    for op in ["change_kind", "preview"] {
        let response = post_multipart(
            &app,
            "/web/compose",
            &cookie,
            &[
                ("csrf", &csrf),
                ("op", op),
                ("post_kind", "article"),
                ("title", "Saved headline"),
                ("status", "Saved article body"),
            ],
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(response.body.contains(r#"value="article" selected"#));
        assert!(
            response
                .body
                .contains(r#"name="title" value="Saved headline""#)
        );
        assert!(response.body.contains("Saved article body"));
        assert!(
            response
                .body
                .contains("data-compose-note-only hidden disabled")
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn composer_type_fallback_renders_matching_help_and_placeholder(pool: PgPool) {
    let app = app_with_alice(pool).await;
    let cookie = login(&app).await;
    let page = get(&app, "/compose", Some(&cookie)).await;
    let csrf = csrf_of(&page.body);
    for (kind, placeholder) in [
        ("article", "What do you want to write about?"),
        ("event", "Why are we meeting?"),
        ("note", "What's on your mind?"),
    ] {
        let response = post_multipart(
            &app,
            "/web/compose",
            &cookie,
            &[("csrf", &csrf), ("op", "change_kind"), ("post_kind", kind)],
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(
            response
                .body
                .contains(r#"<details class="compose__compatibility"><summary>"#)
        );
        assert!(
            response
                .body
                .contains(&format!(r#"data-compose-compatibility="{kind}">"#))
        );
        assert!(
            response
                .body
                .contains(&format!(r#"placeholder="{placeholder}""#)),
            "{kind}: missing placeholder"
        );
        assert_eq!(
            response
                .body
                .contains("data-compose-schedulable hidden disabled"),
            kind == "event"
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn composer_schedules_an_article_and_publishes_its_title_and_body(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    let app = build_router(state.clone());
    let cookie = login(&app).await;
    let csrf = csrf_of(&get(&app, "/compose", Some(&cookie)).await.body);
    let body = "A long article paragraph. ".repeat(60);
    let response = post_multipart(
        &app,
        "/web/compose",
        &cookie,
        &[
            ("csrf", &csrf),
            ("post_kind", "article"),
            ("title", "Future headline"),
            ("status", &body),
            ("scheduled_at", "2030-01-15T12:00"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        response.location.as_deref(),
        Some("/settings/scheduled?saved=scheduled")
    );
    let rows = scheduled_status::list_for_account(&pool, alice.id, None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].object_type, "Article");
    assert_eq!(rows[0].title.as_deref(), Some("Future headline"));
    assert_eq!(rows[0].text.trim(), body.trim());
    let queue = get(&app, "/settings/scheduled", Some(&cookie)).await;
    assert!(queue.body.contains("Future headline"));
    assert!(queue.body.contains("Article"));
    scheduled_status::update_scheduled_at(
        &pool,
        alice.id,
        rows[0].id,
        time::OffsetDateTime::now_utc() - time::Duration::seconds(1),
    )
    .await
    .unwrap();
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 1);
    let posts = status::home_timeline(&pool, alice.id, user::TimelineOrder::Published, None, 10)
        .await
        .unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].object_type.as_deref(), Some("Article"));
    assert_eq!(posts[0].title.as_deref(), Some("Future headline"));
    assert!(posts[0].content.contains(body.trim()));
    assert!(
        scheduled_status::find_for_account(&pool, alice.id, rows[0].id)
            .await
            .unwrap()
            .is_none()
    );
}
