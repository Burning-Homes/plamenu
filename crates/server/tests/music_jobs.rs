mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    auth::{generate_secret, hash_password, hash_secret},
    music_jobs::{self, Reference},
};
use plamenu_db::{PgPool, id, oauth, role, user};
use serde_json::{Value, json};
use tower::ServiceExt;
struct Fixture {
    state: AppState,
    admin: i64,
    uid: i64,
    owner: i64,
    rendition: i64,
    export: i64,
    output: i64,
    folder: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let admin = create_local_account(&pool, "admin", "Admin").await.id;
    let owner = create_local_account(&pool, "owner", "Owner").await.id;
    let uid = user::create(
        &pool,
        admin,
        Some("admin@example.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap()
    .id;
    user::create(
        &pool,
        owner,
        Some("owner@example.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let privileged = role::find_by_name(&pool, "Admin").await.unwrap().unwrap();
    role::assign_to_account(&pool, admin, Some(privileged.id))
        .await
        .unwrap();
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Conversion artist')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration) VALUES($1,$2,$3,'Work',$4,$5,'mp3',1,1,$6,1)").bind(asset).bind(owner).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind("0".repeat(64)).execute(&state.pool).await.unwrap();
    let rendition = id::next();
    let output = id::next();
    let collection = id::next();
    let folder = id::next();
    sqlx::query("INSERT INTO music_renditions(id,asset_id,owner_id,source_hash,format,bitrate,requester_id) VALUES($1,$2,$3,$4,'mp3',192,$3)").bind(rendition).bind(asset).bind(owner).bind("0".repeat(64)).execute(&state.pool).await.unwrap();
    let export = plamenu::music_downloads::create(
        &state,
        owner,
        &plamenu::music_downloads::Request {
            sources: vec![plamenu::music_queues::Source {
                asset_id: Some(asset.to_string()),
                remote_id: None,
                status_id: None,
                episode_id: None,
            }],
            kind: None,
            id: None,
            label: Some("Export".into()),
        },
    )
    .await
    .unwrap()
    .id;
    sqlx::query("INSERT INTO music_outputs(id,owner_id,mount,name,format,bitrate,device,transport,worker_lease,lease_until,listen_token_hash) VALUES($1,$2,'operator-test','Output','mp3',192,'test-player','playing',123,now()+interval '10 seconds','fixture-private-key')").bind(output).bind(owner).execute(&state.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO music_collections(id,owner_id,name,kind) VALUES($1,$2,'Folder','library')",
    )
    .bind(collection)
    .bind(owner)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO music_local_folders(id,owner_id,collection_id,root_id,root_device,root_inode,state,lease,lease_until) VALUES($1,$2,$3,'test',1,1,'scanning','lease',now()+interval '2 minutes')").bind(folder).bind(owner).bind(collection).execute(&state.pool).await.unwrap();
    Fixture {
        state,
        admin,
        uid,
        owner,
        rendition,
        export,
        output,
        folder,
    }
}
async fn token(f: &Fixture, scope: &str) -> String {
    let app = oauth::create_app(
        &f.state.pool,
        oauth::NewApp {
            name: "Jobs test",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: "admin:read admin:write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        &f.state.pool,
        &hash_secret(&token),
        app.id,
        Some(f.uid),
        scope,
    )
    .await
    .unwrap();
    token
}
#[sqlx::test(migrations = "../db/migrations")]
async fn operator_cancellation_is_audited_fences_workers_and_preserves_saved_configuration(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let page = music_jobs::page(&f.state, &music_jobs::Query::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4);
    for (kind, id) in [
        ("rendition", f.rendition),
        ("export", f.export),
        ("output", f.output),
        ("folder", f.folder),
    ] {
        music_jobs::cancel(
            &f.state,
            f.admin,
            &Reference {
                kind: kind.into(),
                id: id.to_string(),
            },
        )
        .await
        .unwrap();
    }
    assert!(
        music_jobs::page(&f.state, &music_jobs::Query::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let output: (String, i64, Option<i64>) =
        sqlx::query_as("SELECT transport,generation,worker_lease FROM music_outputs WHERE id=$1")
            .bind(f.output)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(output, ("stopped".into(), 2, None));
    let folder: (String, i64, Option<String>, bool) =
        sqlx::query_as("SELECT state,revision,lease,watch FROM music_local_folders WHERE id=$1")
            .bind(f.folder)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(folder, ("idle".into(), 2, None, true));
    let audits:i64=sqlx::query_scalar("SELECT count(*) FROM admin_action_logs WHERE account_id=$1 AND target_type='MusicJob' AND action='cancel'").bind(f.admin).fetch_one(&f.state.pool).await.unwrap();
    assert_eq!(audits, 4);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn completed_work_and_durable_file_recovery_are_not_terminated(pool: PgPool) {
    let f = fixture(pool).await;
    sqlx::query("UPDATE music_renditions SET state='failed' WHERE id=$1")
        .bind(f.rendition)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_jobs::cancel(
            &f.state,
            f.admin,
            &Reference {
                kind: "rendition".into(),
                id: f.rendition.to_string()
            }
        )
        .await
        .is_err()
    );
    let recovery = id::next();
    let intent = json!({
        "root_id":"fixture", "mount":{"device":"0","inode":"0"},
        "parent":{"device":"0","inode":"0"}, "source":"source.mp3",
        "target":"quarantine.mp3", "operation":"delete",
        "before":{"device":"0","inode":"0","bytes":"1","modified_seconds":0,"modified_nanos":0}
    });
    sqlx::query("INSERT INTO music_local_mutations(id,folder_id,file_id,owner_id,intent) VALUES($1,$2,$3,$4,jsonb_populate_record(NULL::music_local_mutation_intent,$5))")
        .bind(recovery).bind(f.folder).bind(id::next()).bind(f.owner).bind(intent)
        .execute(&f.state.pool).await.unwrap();
    assert!(
        music_jobs::cancel(
            &f.state,
            f.admin,
            &Reference {
                kind: "recovery".into(),
                id: recovery.to_string()
            }
        )
        .await
        .is_err()
    );
    assert!(
        music_jobs::cancel(
            &f.state,
            f.admin,
            &Reference {
                kind: "folder".into(),
                id: f.folder.to_string()
            }
        )
        .await
        .is_err()
    );
    let page = music_jobs::page(&f.state, &music_jobs::Query::default())
        .await
        .unwrap();
    assert!(page.items.iter().any(|j| j.kind == "recovery"));
    assert!(!page.items.iter().any(|j| j.kind == "folder"));
}
#[sqlx::test(migrations = "../db/migrations")]
async fn job_api_requires_role_and_admin_scopes_and_native_cancellation_requires_csrf(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let read = token(&f, "admin:read").await;
    let write = token(&f, "admin:write").await;
    let narrow = token(&f, "read").await;
    let app = plamenu::build_router(f.state.clone());
    for (token, expected) in [(&read, StatusCode::OK), (&narrow, StatusCode::FORBIDDEN)] {
        let r = app
            .clone()
            .oneshot(
                Request::get("/api/v1/admin/music/jobs")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), expected);
    }
    let payload = json!({"kind":"export","id":f.export.to_string()});
    for (token, expected) in [
        (&read, StatusCode::FORBIDDEN),
        (&write, StatusCode::NO_CONTENT),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::post("/api/v1/admin/music/jobs/cancel")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), expected);
    }
    let login = app
        .clone()
        .oneshot(
            Request::post("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "email=admin%40example.test&password=fixture-password",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let r = app
        .clone()
        .oneshot(
            Request::get("/admin/music/jobs")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let html =
        String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(html.contains("Stop current run"));
    assert!(!html.contains("source_hash"));
    let request = |csrf: &str| {
        Request::post("/web/admin/music/jobs/cancel")
            .header("cookie", &cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(
                serde_urlencoded::to_string([
                    ("csrf", csrf),
                    ("kind", "output"),
                    ("id", &f.output.to_string()),
                ])
                .unwrap(),
            ))
            .unwrap()
    };
    let r = app.clone().oneshot(request("wrong")).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let csrf = hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let r = app.oneshot(request(&csrf)).await.unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn batch_cancellation_validates_before_changes_and_reports_only_stopped_runs(pool: PgPool) {
    let f = fixture(pool).await;
    let invalid = [
        Reference {
            kind: "export".into(),
            id: f.export.to_string(),
        },
        Reference {
            kind: "recovery".into(),
            id: "1".into(),
        },
    ];
    assert!(
        music_jobs::cancel_many(&f.state, f.admin, &invalid)
            .await
            .is_err()
    );
    assert_eq!(
        music_jobs::page(&f.state, &music_jobs::Query::default())
            .await
            .unwrap()
            .items
            .len(),
        4
    );
    let write = token(&f, "admin:write").await;
    let app = plamenu::build_router(f.state.clone());
    let payload = json!({"jobs":[{"kind":"export","id":f.export.to_string()},{"kind":"output","id":f.output.to_string()},{"kind":"export","id":id::next().to_string()}]});
    let r = app
        .oneshot(
            Request::post("/api/v1/admin/music/jobs/cancel")
                .header("authorization", format!("Bearer {write}"))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["cancelled"].as_array().unwrap().len(), 2);
}
