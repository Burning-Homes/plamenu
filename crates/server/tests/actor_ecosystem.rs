//! Authority and notification journeys across people and hosted actors.
mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu_db::{PgPool, account::NewLocalAccount, group, notification, report};
use serde_json::Value;
use tower::ServiceExt;

async fn token(state: &plamenu::AppState, account: i64) -> String {
    let user = plamenu_db::user::set_credentials(
        &state.pool,
        account,
        Some(&format!("fixture-{account}@example.test")),
        "unused",
    )
    .await
    .unwrap();
    let app = plamenu::web::session::ensure_web_app(state).await.unwrap();
    let raw = format!("ecosystem-{}", plamenu_db::id::next());
    plamenu_db::oauth::create_token(
        &state.pool,
        &plamenu::auth::hash_secret(&raw),
        app.id,
        Some(user.id),
        "read write",
    )
    .await
    .unwrap();
    raw
}

async fn get(state: &plamenu::AppState, token: &str, path: &str) -> (StatusCode, Value) {
    let response = plamenu::build_router(state.clone())
        .oneshot(
            Request::builder()
                .uri(path)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_group_ban_prevents_refollow_and_expires_normally(pool: PgPool) {
    let owner = create_local_account(&pool, "owner", "Owner").await;
    let member = create_local_account(&pool, "member", "Member").await;
    let (community, _) = group::create(
        &pool,
        group::NewLocalGroup {
            account: NewLocalAccount {
                username: "community",
                display_name: "Community",
                note: "",
                public_key_pem: "fixture",
            },
            membership_policy: group::MembershipPolicy::Open,
            posting_policy: group::PostingPolicy::Members,
            created_by: owner.id,
        },
    )
    .await
    .unwrap();
    group::set_affiliation(
        &pool,
        community.id,
        member.id,
        group::Affiliation::Outcast,
        None,
    )
    .await
    .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let result = plamenu::actions::follow_account(&state, &member, &community).await;
    let follows = group::is_member(&pool, community.id, member.id)
        .await
        .unwrap();
    assert!(matches!(
        result,
        Err(plamenu::error::ApiError::Forbidden(_))
    ));
    assert!(!follows);
    sqlx::query("UPDATE group_affiliations SET expires_at=now()-interval '1 second' WHERE group_account_id=$1 AND account_id=$2")
        .bind(community.id).bind(member.id).execute(&pool).await.unwrap();
    plamenu::actions::follow_account(&state, &member, &community)
        .await
        .unwrap();
    assert!(
        group::is_member(&pool, community.id, member.id)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn group_report_notifications_expose_only_public_accounts_and_current_authority(
    pool: PgPool,
) {
    let owner = create_local_account(&pool, "owner", "Owner").await;
    let moderator = create_local_account(&pool, "moderator", "Moderator").await;
    let reporter = create_local_account(&pool, "reporter", "Reporter").await;
    let target = create_local_account(&pool, "target", "Target").await;
    plamenu_db::user::set_credentials(
        &pool,
        target.id,
        Some("private-fixture@example.test"),
        "unused",
    )
    .await
    .unwrap();
    let (community, _) = group::create(
        &pool,
        group::NewLocalGroup {
            account: NewLocalAccount {
                username: "community",
                display_name: "Community",
                note: "",
                public_key_pem: "fixture",
            },
            membership_policy: group::MembershipPolicy::Open,
            posting_policy: group::PostingPolicy::Members,
            created_by: owner.id,
        },
    )
    .await
    .unwrap();
    group::set_affiliation(
        &pool,
        community.id,
        moderator.id,
        group::Affiliation::Moderator,
        None,
    )
    .await
    .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let moderator_token = token(&state, moderator.id).await;
    let report = report::create_scoped(
        &pool,
        report::NewReport {
            account_id: reporter.id,
            target_account_id: target.id,
            status_ids: &[],
            comment: "Private fixture moderation report",
            category: "spam",
            forwarded: Some(false),
            rule_ids: None,
            uri: None,
        },
        Some(community.id),
    )
    .await
    .unwrap();
    plamenu::actions::notify_staff_about_report(&state, &report)
        .await
        .unwrap();
    let id: i64 =
        sqlx::query_scalar("SELECT id FROM notifications WHERE account_id=$1 AND report_id=$2")
            .bind(moderator.id)
            .bind(report.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let item = notification::find_by_id(&pool, moderator.id, id)
        .await
        .unwrap()
        .unwrap();
    let before = plamenu::entities::render_notifications(
        &pool,
        &state.config.domain,
        std::slice::from_ref(&item),
        moderator.id,
    )
    .await
    .unwrap();
    let private_email_exposed = before[0]
        .pointer("/report/target_account/email")
        .and_then(serde_json::Value::as_str)
        .is_some();
    assert!(!private_email_exposed);
    assert_eq!(
        before[0]["report"]["target_account"]["id"],
        target.id.to_string()
    );
    for party in ["account", "target_account"] {
        for key in [
            "email",
            "ip",
            "ips",
            "locale",
            "invite_request",
            "role",
            "confirmed",
            "approved",
            "disabled",
        ] {
            assert!(
                before[0]["report"][party].get(key).is_none(),
                "private field {party}.{key}"
            );
        }
    }
    assert_eq!(
        before[0]["report"]["action_url"],
        format!(
            "https://{}/groups/{}/reports",
            state.config.domain, community.id
        )
    );
    let (code, page) = get(&state, &moderator_token, "/api/v2/notifications").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        page["notification_groups"][0]["report"],
        before[0]["report"]
    );

    group::remove_affiliation(&pool, community.id, moderator.id)
        .await
        .unwrap();
    assert!(
        notification::find_by_id(&pool, moderator.id, id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        notification::find(&pool, id).await.unwrap().is_none(),
        "queued push must recheck authority"
    );
    let after =
        plamenu::entities::render_notifications(&pool, &state.config.domain, &[item], moderator.id)
            .await
            .unwrap();
    assert!(
        after.is_empty(),
        "a previously loaded row must not retain access"
    );
    let (_, page) = get(&state, &moderator_token, "/api/v1/notifications").await;
    assert_eq!(page, serde_json::json!([]));
    let (_, page) = get(&state, &moderator_token, "/api/v2/notifications").await;
    assert_eq!(page["notification_groups"], serde_json::json!([]));
    assert_eq!(
        get(
            &state,
            &moderator_token,
            &format!("/api/v1/notifications/{id}")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        notification::unread_count(
            &pool,
            moderator.id,
            None,
            notification::NotificationFilter::default(),
            100
        )
        .await
        .unwrap(),
        0
    );
}

use plamenu::actions::{self, PostParams};
use plamenu_db::{account::Account, media, status};

struct Ecosystem {
    state: plamenu::AppState,
    owner: Account,
    editor: Account,
    publisher: Account,
    pending: Account,
    channel: Account,
    video: status::Status,
    owner_token: String,
    editor_token: String,
}
async fn change(
    e: &Ecosystem,
    actor: &Account,
    operation: &str,
    username: Option<&str>,
    role: Option<&str>,
) {
    plamenu::video_channels::change(
        &e.state,
        actor,
        e.channel.id,
        &plamenu::video_channels::Change {
            operation: operation.into(),
            username: username.map(str::to_owned),
            role: role.map(str::to_owned),
            confirm: Some(e.channel.username.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}
async fn ecosystem(pool: PgPool) -> Ecosystem {
    let owner = create_local_account(&pool, "owner", "Owner").await;
    let editor = create_local_account(&pool, "editor", "Editor").await;
    let publisher = create_local_account(&pool, "publisher", "Publisher").await;
    let pending = create_local_account(&pool, "pending", "Pending").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let owner_token = token(&state, owner.id).await;
    let editor_token = token(&state, editor.id).await;
    token(&state, publisher.id).await;
    token(&state, pending.id).await;
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films <test>")
        .await
        .unwrap();
    for (person, role, accepted) in [
        (&editor, "editor", true),
        (&publisher, "publisher", true),
        (&pending, "editor", false),
    ] {
        sqlx::query("INSERT INTO video_channel_grants(channel_id,account_id,role,accepted) VALUES($1,$2,$3,$4)")
            .bind(channel.id).bind(person.id).bind(role).bind(accepted).execute(&pool).await.unwrap();
    }
    let film = media::create_local(
        &pool,
        media::NewLocalMedia {
            kind: "video",
            width: Some(320),
            height: Some(240),
            duration: Some(5.1),
            small: Some(media::SmallStyle {
                file_name: "video.jpg",
                width: 320,
                height: 180,
            }),
            ..media::NewLocalMedia::new(owner.id, plamenu_db::id::next(), "video.mp4", "video/mp4")
        },
    )
    .await
    .unwrap();
    let (video, _) = actions::post_video(
        &state,
        PostParams {
            username: "owner",
            text: "Video",
            title: Some("Video"),
            media_ids: &[film.id],
            visibility: "public",
            kind: plamenu_ap::activity::PostKind::Video,
            ..Default::default()
        },
        channel.id,
    )
    .await
    .unwrap();
    notification::clear(&pool, editor.id).await.unwrap();
    Ecosystem {
        state,
        owner,
        editor,
        publisher,
        pending,
        channel,
        video,
        owner_token,
        editor_token,
    }
}
async fn inbox(e: &Ecosystem, person: i64) -> Vec<notification::Notification> {
    notification::list(
        &e.state.pool,
        person,
        None,
        None,
        None,
        notification::NotificationFilter::default(),
        100,
    )
    .await
    .unwrap()
}
async fn web(e: &Ecosystem, raw: &str, path: &str, form: Option<String>) -> (StatusCode, String) {
    let request = if let Some(form) = form {
        Request::post(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
    } else {
        Request::get(path).body(Body::empty())
    };
    let mut request = request.unwrap();
    request.headers_mut().insert(
        "cookie",
        format!("__Host-plamenu_session={raw}").parse().unwrap(),
    );
    let response = plamenu::build_router(e.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let code = response.status();
    (
        code,
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap(),
    )
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_comments_reach_owner_and_editors_once_with_existing_client_fallback(pool: PgPool) {
    let e = ecosystem(pool).await;
    let commenter = create_local_account(&e.state.pool, "commenter", "Commenter").await;
    let (reply, _) = actions::post_status(
        &e.state,
        PostParams {
            username: "commenter",
            text: "@owner Great video",
            visibility: "public",
            in_reply_to_id: Some(e.video.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for person in [&e.owner, &e.editor] {
        let rows = inbox(&e, person.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "plamenu:actor.comment");
        assert_eq!(rows[0].status_id, Some(reply.id));
        let contexts = plamenu_db::actor_notification::load(&e.state.pool, &[rows[0].id])
            .await
            .unwrap();
        assert_eq!(contexts[&rows[0].id][0].actor_account_id, e.channel.id);
    }
    assert!(inbox(&e, e.publisher.id).await.is_empty());
    assert!(inbox(&e, e.pending.id).await.is_empty());
    let mentions = plamenu_db::mention::for_statuses(&e.state.pool, &[reply.id], false)
        .await
        .unwrap();
    assert!(
        mentions
            .values()
            .flatten()
            .all(|a| a.id != e.editor.id && a.id != e.channel.id)
    );
    let (_, v1) = get(
        &e.state,
        &e.owner_token,
        "/api/v1/notifications?supported_types[]=mention&exclude_types[]=follow_request",
    )
    .await;
    assert_eq!(v1.as_array().unwrap().len(), 1);
    assert_eq!(v1[0]["type"], "plamenu:actor.comment");
    assert_eq!(v1[0]["status"]["id"], reply.id.to_string());
    assert!(
        v1[0]["fallback"]["title"]
            .as_str()
            .unwrap()
            .contains("Films &lt;test&gt;")
    );
    let (_, v2) = get(
        &e.state,
        &e.owner_token,
        "/api/v2/notifications?supported_types[]=mention",
    )
    .await;
    assert_eq!(v2["notification_groups"][0]["fallback"], v1[0]["fallback"]);
    assert_eq!(
        v2["notification_groups"][0]["status_id"],
        reply.id.to_string()
    );
    let note = status::create_local(
        &e.state.pool,
        status::NewLocalStatus::new(e.owner.id, "Ordinary note", "public", None),
    )
    .await
    .unwrap();
    actions::post_status(
        &e.state,
        PostParams {
            username: "commenter",
            text: "An untagged ordinary reply",
            visibility: "public",
            in_reply_to_id: Some(note.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(inbox(&e, e.owner.id).await.len(), 1);
    assert_eq!(commenter.id, reply.account_id);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scoped_follow_actions_preserve_personal_edges_and_retract_every_controller_copy(
    pool: PgPool,
) {
    let e = ecosystem(pool).await;
    let fan = create_local_account(&e.state.pool, "fan", "Fan").await;
    sqlx::query("UPDATE accounts SET locked=true WHERE id=$1")
        .bind(e.channel.id)
        .execute(&e.state.pool)
        .await
        .unwrap();
    let channel = plamenu_db::account::find_by_id(&e.state.pool, e.channel.id)
        .await
        .unwrap()
        .unwrap();
    actions::follow_account(&e.state, &fan, &e.owner)
        .await
        .unwrap();
    actions::follow_account(&e.state, &fan, &channel)
        .await
        .unwrap();
    assert_eq!(inbox(&e, e.owner.id).await.len(), 2);
    let rows = inbox(&e, e.editor.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "plamenu:actor.follow_request");
    let (_, entity) = get(
        &e.state,
        &e.editor_token,
        "/api/v1/notifications?exclude_types[]=follow_request",
    )
    .await;
    assert_eq!(entity.as_array().unwrap().len(), 1);
    assert!(
        entity[0]["fallback"]["summary"]
            .as_str()
            .unwrap()
            .contains(&format!("/videos/channels/{}/requests", channel.id))
    );
    let path = format!("/videos/channels/{}/requests", channel.id);
    let (code, page) = web(&e, &e.editor_token, &path, None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(page.contains("Fan"));
    assert!(page.contains("name=\"operation\" value=\"approve\""));
    let action = format!("/web/video-channels/{}/requests", channel.id);
    let form = format!("operation=approve&account_id={}&csrf=bad", fan.id);
    assert_eq!(
        web(&e, &e.editor_token, &action, Some(form)).await.0,
        StatusCode::FORBIDDEN
    );
    let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{}", e.editor_token));
    let form = format!("operation=approve&account_id={}&csrf={csrf}", fan.id);
    assert_eq!(
        web(&e, &e.editor_token, &action, Some(form)).await.0,
        StatusCode::SEE_OTHER
    );
    assert!(
        !plamenu_db::follow::find(&e.state.pool, fan.id, channel.id)
            .await
            .unwrap()
            .unwrap()
            .pending
    );
    assert!(
        inbox(&e, e.editor.id)
            .await
            .iter()
            .all(|n| n.kind == "plamenu:actor.follow")
    );
    actions::unfollow_account(&e.state, &fan, &channel)
        .await
        .unwrap();
    assert!(inbox(&e, e.editor.id).await.is_empty());
    let rows = inbox(&e, e.owner.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "follow");
    assert!(
        plamenu_db::follow::find(&e.state.pool, fan.id, e.owner.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_authority_profiles_consent_and_revocation_work_without_javascript(pool: PgPool) {
    let e = ecosystem(pool).await;
    let (code, html) = web(&e, &e.owner_token, "/@films", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(html.contains("Controllers"));
    assert!(html.contains("href=\"/@editor\""));
    assert!(html.contains("href=\"/@publisher\""));
    assert!(!html.contains("href=\"/@pending\""));
    assert!(html.contains(&format!("href=\"/videos/channels/{}\"", e.channel.id)));
    let (_, html) = web(&e, &e.owner_token, "/@owner", None).await;
    assert!(html.contains("Channels &amp; groups"));
    assert!(html.contains("href=\"/@films\""));
    let newcomer = create_local_account(&e.state.pool, "newcomer", "Newcomer").await;
    let newcomer_token = token(&e.state, newcomer.id).await;
    change(&e, &e.owner, "invite", Some("newcomer"), Some("editor")).await;
    change(&e, &e.owner, "invite", Some("newcomer"), Some("editor")).await;
    let rows = inbox(&e, newcomer.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "plamenu:actor.invitation");
    assert_eq!(
        web(
            &e,
            &newcomer_token,
            &format!("/videos/channels/{}", e.channel.id),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    change(&e, &newcomer, "accept", None, None).await;
    assert!(inbox(&e, newcomer.id).await.is_empty());
    change(&e, &e.owner, "transfer", Some("newcomer"), None).await;
    assert_eq!(
        inbox(&e, newcomer.id).await[0].kind,
        "plamenu:actor.transfer"
    );
    change(&e, &newcomer, "accept_transfer", None, None).await;
    assert!(inbox(&e, newcomer.id).await.is_empty());
    assert!(
        plamenu_db::video_channel_access::role(&e.state.pool, e.channel.id, e.editor.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        web(
            &e,
            &e.editor_token,
            &format!("/videos/channels/{}/requests", e.channel.id),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, html) = web(&e, &e.owner_token, "/@owner", None).await;
    assert!(!html.contains("actor-connections"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn direct_channel_reply_does_not_grant_other_editors_content_access(pool: PgPool) {
    let e = ecosystem(pool).await;
    let writer = create_local_account(&e.state.pool, "writer", "Writer").await;
    let (reply, _) = actions::post_status(
        &e.state,
        PostParams {
            username: "writer",
            text: "@owner Private comment",
            visibility: "direct",
            in_reply_to_id: Some(e.video.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(inbox(&e, e.owner.id).await.len(), 1);
    assert!(inbox(&e, e.editor.id).await.is_empty());
    assert!(
        !plamenu::entities::can_view(&e.state.pool, &reply, Some(e.editor.id))
            .await
            .unwrap()
    );
    let mentions = plamenu_db::mention::for_statuses(&e.state.pool, &[reply.id], false)
        .await
        .unwrap();
    assert!(
        mentions
            .values()
            .flatten()
            .all(|a| a.id == e.owner.id || a.id == writer.id)
    );
    // A lost audience entitlement must also remove the old notification from
    // list, count, background delivery and already-loaded serialization.
    let rows = inbox(&e, e.owner.id).await;
    sqlx::query("DELETE FROM status_mentions WHERE status_id=$1")
        .bind(reply.id)
        .execute(&e.state.pool)
        .await
        .unwrap();
    assert!(inbox(&e, e.owner.id).await.is_empty());
    assert!(
        notification::find(&e.state.pool, rows[0].id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        plamenu::entities::render_notifications(
            &e.state.pool,
            &e.state.config.domain,
            &rows,
            e.owner.id
        )
        .await
        .unwrap()
        .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_history_migration_keeps_ids_and_merges_personal_post_reasons(pool: PgPool) {
    let e = ecosystem(pool).await;
    let other = create_local_account(&e.state.pool, "other", "Other").await;
    let reply = status::create_local(
        &e.state.pool,
        status::NewLocalStatus::new(other.id, "Hello", "public", Some(e.video.id)),
    )
    .await
    .unwrap();
    notification::create(
        &e.state.pool,
        e.owner.id,
        other.id,
        "mention",
        Some(reply.id),
    )
    .await
    .unwrap();
    let personal = inbox(&e, e.owner.id).await[0].id;
    let orphan = plamenu_db::id::next();
    let orphan_follow = plamenu_db::id::next();
    sqlx::query("INSERT INTO notifications(id,account_id,from_account_id,kind,status_id,reasons) VALUES($1,$2,$3,'mention',$4,ARRAY['mention']),($5,$2,$3,'follow',NULL,ARRAY['follow'])")
        .bind(orphan).bind(e.channel.id).bind(other.id).bind(reply.id).bind(orphan_follow).execute(&e.state.pool).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../../db/migrations/0183_actor_notification_history.sql"
    ))
    .execute(&e.state.pool)
    .await
    .unwrap();
    let rows = inbox(&e, e.owner.id).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|n| n.id == orphan_follow && n.kind == "plamenu:actor.follow")
    );
    assert!(
        rows.iter()
            .any(|n| n.id == personal && n.kind == "plamenu:actor.mention")
    );
    assert!(inbox(&e, e.editor.id).await.is_empty());
    assert!(
        notification::find(&e.state.pool, orphan)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_delivery_notifies_once_but_history_and_audience_alone_do_not(pool: PgPool) {
    let e = ecosystem(pool).await;
    let remote = common::RemoteUser::new("video-peer.test", "viewer");
    let writer = plamenu::remote::store_remote_actor(&e.state.pool, &remote.actor)
        .await
        .unwrap();
    let object = |suffix: &str| {
        serde_json::json!({
            "id":format!("https://video-peer.test/comments/{suffix}"),"type":"Note",
            "attributedTo":writer.uri,"content":"Untagged comment","published":"2026-10-14T10:00:00Z",
            "inReplyTo":format!("https://plamenu.test/users/owner/statuses/{}",e.video.id),
            "to":["https://www.w3.org/ns/activitystreams#Public"],"cc":[e.owner.uri,e.channel.uri],"tag":[]
        })
    };
    let note = object("new");
    let delivered = plamenu::ingest::ingest_remote_note_delivery(&e.state, &writer, &note)
        .await
        .unwrap();
    assert!(delivered.delivery_effects);
    for person in [&e.owner, &e.editor] {
        assert_eq!(inbox(&e, person.id).await.len(), 1);
    }
    sqlx::query("UPDATE accounts SET url='javascript:alert(1)' WHERE id=$1")
        .bind(writer.id)
        .execute(&e.state.pool)
        .await
        .unwrap();
    let (_, rendered) = get(&e.state, &e.owner_token, "/api/v1/notifications").await;
    let title = rendered[0]["fallback"]["title"].as_str().unwrap();
    assert!(!title.contains("javascript:"));
    assert!(title.contains("https://plamenu.test/@viewer@video-peer.test"));
    let replay = plamenu::ingest::ingest_remote_note_delivery(&e.state, &writer, &note)
        .await
        .unwrap();
    assert!(!replay.delivery_effects);
    assert_eq!(inbox(&e, e.owner.id).await.len(), 1);
    let cold = plamenu::ingest::ingest_remote_note_in_context(
        &e.state,
        &writer,
        &object("old"),
        plamenu::ingest::RemoteIngestContext::History,
    )
    .await
    .unwrap();
    assert_eq!(cold.in_reply_to_id, Some(e.video.id));
    assert_eq!(inbox(&e, e.owner.id).await.len(), 1);
    let mut unrelated = object("audience-only");
    unrelated["inReplyTo"] = Value::Null;
    plamenu::ingest::ingest_remote_note_delivery(&e.state, &writer, &unrelated)
        .await
        .unwrap();
    assert_eq!(inbox(&e, e.owner.id).await.len(), 1);
    let mentions = plamenu_db::mention::for_statuses(&e.state.pool, &[delivered.status.id], true)
        .await
        .unwrap();
    assert!(mentions.values().flatten().next().is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_reactions_are_idempotent_and_undo_preserves_personal_notifications(pool: PgPool) {
    let e = ecosystem(pool).await;
    let fan = create_local_account(&e.state.pool, "fan", "Fan").await;
    let personal = status::create_local(
        &e.state.pool,
        status::NewLocalStatus::new(e.owner.id, "Personal", "public", None),
    )
    .await
    .unwrap();
    actions::favourite_status(&e.state, &fan, personal.id)
        .await
        .unwrap();
    actions::favourite_status(&e.state, &fan, e.video.id)
        .await
        .unwrap();
    actions::favourite_status(&e.state, &fan, e.video.id)
        .await
        .unwrap();
    assert_eq!(inbox(&e, e.owner.id).await.len(), 2);
    let rows = inbox(&e, e.editor.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "plamenu:actor.favourite");
    assert!(inbox(&e, e.publisher.id).await.is_empty());
    actions::unfavourite_status(&e.state, &fan, e.video.id)
        .await
        .unwrap();
    assert!(inbox(&e, e.editor.id).await.is_empty());
    let rows = inbox(&e, e.owner.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_id, Some(personal.id));
    plamenu_db::custom_emoji::create_local(
        &e.state.pool,
        "party",
        "party.png",
        "image/png",
        1,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    for emoji in ["party", "🎉"] {
        actions::react_with_emoji(&e.state, &fan, e.video.id, emoji)
            .await
            .unwrap();
    }
    for token in [&e.owner_token, &e.editor_token] {
        let (_, v1) = get(&e.state, token, "/api/v1/notifications").await;
        let (_, v2) = get(&e.state, token, "/api/v2/notifications").await;
        assert_eq!(
            v1.as_array()
                .unwrap()
                .iter()
                .filter(|n| n["type"] == "plamenu:actor.emoji_reaction")
                .count(),
            2
        );
        for reaction in v1
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["type"] == "plamenu:actor.emoji_reaction")
        {
            let title = reaction["fallback"]["title"].as_str().unwrap();
            if reaction["emoji"] == "🎉" {
                assert!(title.contains("🎉"));
            } else {
                assert!(title.contains("<img class=\"shortcode-emoji emoji\""));
                assert!(title.contains("width=\"16\" height=\"16\""));
                assert!(title.contains(reaction["emoji_url"].as_str().unwrap()));
            }
            let group = v2["notification_groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["most_recent_notification_id"] == reaction["id"])
                .unwrap();
            assert_eq!(group["fallback"], reaction["fallback"]);
        }
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_request_loses_access_when_editor_revoked(pool: PgPool) {
    let e = ecosystem(pool).await;
    let fan = create_local_account(&e.state.pool, "fan", "Fan").await;
    sqlx::query("UPDATE accounts SET locked=true WHERE id=$1")
        .bind(e.channel.id)
        .execute(&e.state.pool)
        .await
        .unwrap();
    let channel = plamenu_db::account::find_by_id(&e.state.pool, e.channel.id)
        .await
        .unwrap()
        .unwrap();
    actions::follow_account(&e.state, &fan, &channel)
        .await
        .unwrap();
    let rows = inbox(&e, e.editor.id).await;
    assert_eq!(rows.len(), 1);
    change(&e, &e.owner, "revoke", Some("editor"), None).await;
    assert!(
        notification::find(&e.state.pool, rows[0].id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        plamenu::entities::render_notifications(
            &e.state.pool,
            &e.state.config.domain,
            &rows,
            e.editor.id
        )
        .await
        .unwrap()
        .is_empty()
    );
    let (code, body) = get(
        &e.state,
        &e.editor_token,
        "/api/v1/notifications/unread_count",
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["count"], 0);
    let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{}", e.editor_token));
    let form = format!("operation=approve&account_id={}&csrf={csrf}", fan.id);
    assert_eq!(
        web(
            &e,
            &e.editor_token,
            &format!("/web/video-channels/{}/requests", channel.id),
            Some(form)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert!(
        plamenu_db::follow::find(&e.state.pool, fan.id, channel.id)
            .await
            .unwrap()
            .unwrap()
            .pending
    );
    assert_eq!(inbox(&e, e.owner.id).await.len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_invitation_and_notification_commit_together(pool: PgPool) {
    let e = ecosystem(pool).await;
    let person = create_local_account(&e.state.pool, "invitee", "Invitee").await;
    token(&e.state, person.id).await;
    sqlx::raw_sql("CREATE FUNCTION fail_actor_offer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='plamenu:actor.invitation' THEN RAISE EXCEPTION 'injected notification failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_actor_offer BEFORE INSERT ON notifications FOR EACH ROW EXECUTE FUNCTION fail_actor_offer();").execute(&e.state.pool).await.unwrap();
    let result = plamenu::video_channels::change(
        &e.state,
        &e.owner,
        e.channel.id,
        &plamenu::video_channels::Change {
            operation: "invite".into(),
            username: Some("invitee".into()),
            role: Some("editor".into()),
            ..Default::default()
        },
    )
    .await;
    assert!(result.is_err());
    assert!(
        plamenu_db::video_channel_access::invitations(&e.state.pool, person.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(inbox(&e, person.id).await.is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn group_request_targets_group_and_is_removed_for_all_moderators(pool: PgPool) {
    let e = ecosystem(pool).await;
    let (community, _) = group::create(
        &e.state.pool,
        group::NewLocalGroup {
            account: NewLocalAccount {
                username: "community",
                display_name: "Community",
                note: "",
                public_key_pem: "fixture",
            },
            membership_policy: group::MembershipPolicy::Approval,
            posting_policy: group::PostingPolicy::Members,
            created_by: e.owner.id,
        },
    )
    .await
    .unwrap();
    group::set_affiliation(
        &e.state.pool,
        community.id,
        e.editor.id,
        group::Affiliation::Moderator,
        None,
    )
    .await
    .unwrap();
    let fan = create_local_account(&e.state.pool, "fan", "Fan").await;
    actions::follow_account(&e.state, &fan, &community)
        .await
        .unwrap();
    for person in [&e.owner, &e.editor] {
        let rows = inbox(&e, person.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "plamenu:actor.follow_request");
        let rendered = plamenu::entities::render_notifications(
            &e.state.pool,
            &e.state.config.domain,
            &rows,
            person.id,
        )
        .await
        .unwrap();
        assert!(
            rendered[0]["fallback"]["summary"]
                .as_str()
                .unwrap()
                .contains(&format!("/groups/{}/requests", community.id))
        );
    }
    actions::reject_follow_request(&e.state, &community, &fan)
        .await
        .unwrap();
    assert!(inbox(&e, e.owner.id).await.is_empty());
    assert!(inbox(&e, e.editor.id).await.is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn accepting_filtered_channel_comment_preserves_context_and_canonical_id(pool: PgPool) {
    use plamenu_db::notification_policy::{self, Disposition, Policy};
    let e = ecosystem(pool).await;
    let author = create_local_account(&e.state.pool, "author", "Author").await;
    notification_policy::upsert(
        &e.state.pool,
        e.owner.id,
        Policy {
            for_not_following: Disposition::Filter,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let reply = status::create_local(
        &e.state.pool,
        status::NewLocalStatus::new(author.id, "Comment", "public", Some(e.video.id)),
    )
    .await
    .unwrap();
    notification::create_post_notifications_for_delivery(
        &e.state.pool,
        &[notification::PostNotification::status(e.owner.id)],
        author.id,
        reply.id,
    )
    .await
    .unwrap();
    let initial = inbox(&e, e.owner.id).await;
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].kind, "status");
    let requests =
        plamenu_db::notification_request::list(&e.state.pool, e.owner.id, None, None, None, 10)
            .await
            .unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].notifications_count, 1);
    plamenu_db::notification_request::accept(&e.state.pool, e.owner.id, author.id)
        .await
        .unwrap();
    let final_rows = inbox(&e, e.owner.id).await;
    assert_eq!(final_rows.len(), 1);
    assert_eq!(final_rows[0].id, initial[0].id);
    assert_eq!(final_rows[0].kind, "plamenu:actor.comment");
    let contexts = plamenu_db::actor_notification::load(&e.state.pool, &[initial[0].id])
        .await
        .unwrap();
    assert_eq!(contexts[&initial[0].id][0].actor_account_id, e.channel.id);
    let (_, entity) = get(&e.state, &e.owner_token, "/api/v1/notifications").await;
    assert!(
        entity[0]["fallback"]["title"]
            .as_str()
            .unwrap()
            .contains("Films")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn publisher_personal_mention_keeps_channel_context_without_subscribing_to_all_comments(
    pool: PgPool,
) {
    let e = ecosystem(pool).await;
    let media = media::create_local(
        &e.state.pool,
        media::NewLocalMedia {
            kind: "video",
            width: Some(320),
            height: Some(240),
            duration: Some(2.0),
            small: Some(media::SmallStyle {
                file_name: "poster.jpg",
                width: 320,
                height: 180,
            }),
            ..media::NewLocalMedia::new(
                e.publisher.id,
                plamenu_db::id::next(),
                "publisher.mp4",
                "video/mp4",
            )
        },
    )
    .await
    .unwrap();
    let (video, _) = actions::post_video(
        &e.state,
        PostParams {
            username: "publisher",
            title: Some("Publisher video"),
            text: "Video",
            visibility: "public",
            media_ids: &[media.id],
            kind: plamenu_ap::activity::PostKind::Video,
            ..Default::default()
        },
        e.channel.id,
    )
    .await
    .unwrap();
    let commenter = create_local_account(&e.state.pool, "commenter", "Commenter").await;
    let (reply, _) = actions::post_status(
        &e.state,
        PostParams {
            username: "commenter",
            text: "@publisher Thanks",
            visibility: "public",
            in_reply_to_id: Some(video.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let rows = inbox(&e, e.publisher.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "plamenu:actor.comment");
    assert_eq!(rows[0].status_id, Some(reply.id));
    actions::post_status(
        &e.state,
        PostParams {
            username: "commenter",
            text: "Untagged",
            visibility: "public",
            in_reply_to_id: Some(video.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(inbox(&e, e.publisher.id).await.len(), 1);
    assert_eq!(commenter.id, reply.account_id);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signed_out_channel_review_resumes_after_login_without_open_redirect(pool: PgPool) {
    let e = ecosystem(pool).await;
    plamenu_db::user::set_credentials(
        &e.state.pool,
        e.owner.id,
        Some("owner@example.test"),
        &plamenu::auth::hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let app = plamenu::build_router(e.state.clone());
    let path = format!("/videos/channels/{}/requests", e.channel.id);
    let response = app
        .clone()
        .oneshot(Request::get(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/login");
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert!(cookie.ends_with(&path));
    for (cookie, expected) in [
        (cookie.to_owned(), path.as_str()),
        ("__Host-plamenu_actor_return=//evil.example".into(), "/"),
        (
            "__Host-plamenu_actor_return=/groups/1/reports?next=https://evil.example".into(),
            "/",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header("cookie", cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "email=owner%40example.test&password=fixture-password",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], expected);
        assert!(response.headers().get_all("set-cookie").iter().any(|v| {
            v.to_str()
                .unwrap()
                .starts_with("__Host-plamenu_actor_return=;")
        }));
    }
}
