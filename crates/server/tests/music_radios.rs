mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState,
    error::ApiError,
    music_radios::{self, New},
};
use plamenu_db::{PgPool, account};
async fn fixture(pool: PgPool) -> (AppState, i64, i64) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    (
        test_state_with(pool, StubFederation::with_actors([])),
        owner,
        viewer,
    )
}
fn config(visibility: &str) -> New {
    New {
        name: "Evening station".into(),
        description: "An <escaped> description".into(),
        config: "{\"genre\":\"ambient\",\"private_setting\":\"test-only\"}".into(),
        visibility: visibility.into(),
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn private_station_config_never_appears_in_public_or_granted_views(pool: PgPool) {
    let (state, owner, viewer) = fixture(pool).await;
    let radio = music_radios::create(&state, owner, &config("private"))
        .await
        .unwrap();
    assert!(matches!(
        music_radios::get(&state, radio.id, None).await,
        Err(ApiError::NotFound)
    ));
    assert!(matches!(
        music_radios::get(&state, radio.id, Some(viewer)).await,
        Err(ApiError::NotFound)
    ));
    music_radios::grant(&state, owner, radio.id, viewer, true)
        .await
        .unwrap();
    let granted = music_radios::get(&state, radio.id, Some(viewer))
        .await
        .unwrap()
        .entity(Some(viewer));
    assert!(granted.get("config").is_none());
    assert!(granted.get("owner_id").is_none());
    assert_eq!(radio.entity(Some(owner))["config"], radio.config);
    assert!(
        music_radios::list(&state, Some(viewer), true, None)
            .await
            .unwrap()
            .is_empty()
    );
    let updated = music_radios::update(&state, owner, radio.id, radio.revision, &config("public"))
        .await
        .unwrap();
    assert!(updated.entity(None).get("config").is_none());
    assert_eq!(
        music_radios::list(&state, None, false, None)
            .await
            .unwrap()
            .len(),
        1
    );
    music_radios::grant(&state, owner, radio.id, viewer, false)
        .await
        .unwrap();
    music_radios::update(&state, owner, radio.id, updated.revision, &config("local"))
        .await
        .unwrap();
    assert!(matches!(
        music_radios::get(&state, radio.id, None).await,
        Err(ApiError::NotFound)
    ));
    music_radios::get(&state, radio.id, Some(viewer))
        .await
        .unwrap();
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn current_blocks_mutes_and_retirement_override_station_grants(pool: PgPool) {
    let (state, owner, viewer) = fixture(pool).await;
    let radio = music_radios::create(&state, owner, &config("public"))
        .await
        .unwrap();
    music_radios::grant(&state, owner, radio.id, viewer, true)
        .await
        .unwrap();
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        music_radios::list(&state, Some(viewer), false, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        music_radios::get(&state, radio.id, Some(viewer)).await,
        Err(ApiError::NotFound)
    ));
    plamenu_db::block::delete(&state.pool, viewer, owner)
        .await
        .unwrap();
    plamenu_db::mute::upsert(&state.pool, viewer, owner, true, None)
        .await
        .unwrap();
    assert!(
        music_radios::list(&state, Some(viewer), false, None)
            .await
            .unwrap()
            .is_empty()
    );
    music_radios::get(&state, radio.id, Some(viewer))
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_radios::get(&state, radio.id, Some(owner)).await,
        Err(ApiError::NotFound)
    ));
    account::mark_deleted(&state.pool, owner).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_radios")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_radio_grants")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(matches!(
        music_radios::create(&state, owner, &config("private")).await,
        Err(ApiError::NotFound)
    ));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn revision_checks_serialize_competing_edits_and_refuse_foreign_deletion(pool: PgPool) {
    let (state, owner, viewer) = fixture(pool).await;
    let radio = music_radios::create(&state, owner, &config("private"))
        .await
        .unwrap();
    let input = config("public");
    let (first, second) = tokio::join!(
        music_radios::update(&state, owner, radio.id, radio.revision, &input),
        music_radios::update(&state, owner, radio.id, radio.revision, &input)
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(
        matches!(first, Err(ApiError::Conflict(_))) || matches!(second, Err(ApiError::Conflict(_)))
    );
    let current = music_radios::get(&state, radio.id, Some(owner))
        .await
        .unwrap();
    assert_eq!(current.revision, radio.revision + 1);
    assert!(matches!(
        music_radios::delete(&state, viewer, radio.id, current.revision).await,
        Err(ApiError::NotFound)
    ));
    assert!(matches!(
        music_radios::delete(&state, owner, radio.id, radio.revision).await,
        Err(ApiError::Conflict(_))
    ));
    music_radios::delete(&state, owner, radio.id, current.revision)
        .await
        .unwrap();
    assert!(matches!(
        music_radios::get(&state, radio.id, None).await,
        Err(ApiError::NotFound)
    ));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn station_limits_are_owned_and_configuration_remains_bounded_opaque_text(pool: PgPool) {
    let (state, owner, viewer) = fixture(pool).await;
    let mut input = config("private");
    input.config = "provider-neutral station settings".into();
    music_radios::create(&state, owner, &input).await.unwrap();
    for field in ["name", "description", "config", "visibility"] {
        let mut invalid = config("private");
        match field {
            "name" => invalid.name = " ".into(),
            "description" => invalid.description = "x".repeat(4001),
            "config" => invalid.config = "x".repeat(50001),
            _ => invalid.visibility = "followers".into(),
        }
        assert!(matches!(
            music_radios::create(&state, owner, &invalid).await,
            Err(ApiError::Unprocessable(_))
        ));
    }
    let ids = (0..99).map(|_| plamenu_db::id::next()).collect::<Vec<_>>();
    sqlx::query("INSERT INTO music_radios(id,owner_id,name) SELECT n,$2,'Station '||n FROM unnest($1::bigint[]) n").bind(&ids).bind(owner).execute(&state.pool).await.unwrap();
    assert!(matches!(
        music_radios::create(&state, owner, &input).await,
        Err(ApiError::Unprocessable(_))
    ));
    music_radios::create(&state, viewer, &input).await.unwrap();
    let page = music_radios::list(&state, Some(owner), true, None)
        .await
        .unwrap();
    assert_eq!(page.len(), 51);
    let second = music_radios::list(&state, Some(owner), true, Some(page[49].id))
        .await
        .unwrap();
    assert_eq!(second.len(), 50);
    assert!(second.iter().all(|r| r.id < page[49].id));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn account_purge_cleans_station_settings_and_shared_memberships(pool: PgPool) {
    let (state, owner, viewer) = fixture(pool).await;
    let radio = music_radios::create(&state, owner, &config("private"))
        .await
        .unwrap();
    music_radios::grant(&state, owner, radio.id, viewer, true)
        .await
        .unwrap();
    let shared = music_radios::create(&state, viewer, &config("private"))
        .await
        .unwrap();
    music_radios::grant(&state, viewer, shared.id, owner, true)
        .await
        .unwrap();
    let mut tx = state.pool.begin().await.unwrap();
    account::suspend_conn(&mut tx, owner, "local")
        .await
        .unwrap();
    account::purge_local_data_conn(&mut tx, owner)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(!account::is_deleted(&state.pool, owner).await.unwrap());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_radios WHERE owner_id=$1")
            .bind(owner)
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_radio_grants WHERE account_id=$1")
            .bind(owner)
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    music_radios::get(&state, shared.id, Some(viewer))
        .await
        .unwrap();
    account::delete_by_id(&state.pool, viewer).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_radios")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn radio_api_scopes_and_owner_config_redaction_remain_independent(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    use tower::ServiceExt;
    let (state, owner, viewer) = fixture(pool).await;
    let radio = music_radios::create(&state, owner, &config("public"))
        .await
        .unwrap();
    let mut tokens = Vec::new();
    for (id, email) in [(owner, "alice@plamenu.test"), (viewer, "bob@plamenu.test")] {
        let u = user::create(
            &state.pool,
            id,
            Some(email),
            &hash_password("fixture-password").unwrap(),
        )
        .await
        .unwrap();
        let app = oauth::create_app(
            &state.pool,
            oauth::NewApp {
                name: "radio-tests",
                website: None,
                client_id: &generate_secret(),
                client_secret_hash: &hash_secret(&generate_secret()),
                redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".into()],
                scopes: "read write",
            },
        )
        .await
        .unwrap();
        for scope in ["read:statuses", "write:statuses"] {
            let token = generate_secret();
            oauth::create_token(&state.pool, &hash_secret(&token), app.id, Some(u.id), scope)
                .await
                .unwrap();
            tokens.push(token);
        }
    }
    let app = plamenu::build_router(state.clone());
    let path = format!("/api/v1/music/radios/{}", radio.id);
    for (token, status, owns) in [
        (&tokens[0], StatusCode::OK, true),
        (&tokens[1], StatusCode::FORBIDDEN, true),
        (&tokens[2], StatusCode::OK, false),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
            let value: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(value.get("config").is_some(), owns);
        }
    }
    for (token, status) in [
        (&tokens[0], StatusCode::FORBIDDEN),
        (&tokens[3], StatusCode::NOT_FOUND),
        (&tokens[1], StatusCode::OK),
    ] {
        let response=app.clone().oneshot(Request::builder().method("PUT").uri(&path).header("Authorization",format!("Bearer {token}")).header("Content-Type","application/json").body(Body::from(serde_json::json!({"revision":radio.revision.to_string(),"name":"Edited station","visibility":"private","config":"private settings"}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), status);
    }
    assert_eq!(
        music_radios::get(&state, radio.id, Some(owner))
            .await
            .unwrap()
            .config,
        "private settings"
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/music/radios")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn anonymous_catalogue_respects_preview_policy_and_never_grants_private_access(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use plamenu_db::instance_settings::{self, SettingsUpdate};
    use tower::ServiceExt;
    let (state, owner, _) = fixture(pool).await;
    let radio = music_radios::create(&state, owner, &config("public"))
        .await
        .unwrap();
    for preview in [false, true] {
        let settings = instance_settings::get(&state.pool).await.unwrap();
        instance_settings::save(
            &state.pool,
            SettingsUpdate {
                timeline_preview_federated: preview,
                ..settings.as_update()
            },
        )
        .await
        .unwrap();
        let current = test_state_with(state.pool.clone(), StubFederation::with_actors([]));
        let app = plamenu::build_router(current);
        let catalogue = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/music/radios")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            catalogue.status(),
            if preview {
                StatusCode::OK
            } else {
                StatusCode::SEE_OTHER
            }
        );
        if !preview {
            assert_eq!(catalogue.headers()["Location"], "/login");
        }
        let body = String::from_utf8(
            catalogue
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert_eq!(body.contains("Evening station"), preview);
        assert!(!body.contains("private_setting"));
        let page = app
            .oneshot(
                Request::builder()
                    .uri(format!("/music/radios/{}", radio.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(page.headers()["Cache-Control"], "private, no-store");
        let body = String::from_utf8(
            page.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("An &lt;escaped&gt; description"));
        assert!(!body.contains("private_setting"));
    }
    music_radios::update(&state, owner, radio.id, radio.revision, &config("private"))
        .await
        .unwrap();
    let app = plamenu::build_router(test_state_with(
        state.pool.clone(),
        StubFederation::with_actors([]),
    ));
    let page = app
        .oneshot(
            Request::builder()
                .uri(format!("/music/radios/{}", radio.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn station_search_filters_before_paging_and_never_searches_hidden_configuration(
    pool: PgPool,
) {
    let (state, owner, viewer) = fixture(pool).await;
    let target = music_radios::create(&state, owner, &config("public"))
        .await
        .unwrap();
    for _ in 0..52 {
        let mut station = config("public");
        station.name = "Other station".into();
        station.description = "Other description".into();
        music_radios::create(&state, owner, &station).await.unwrap();
    }
    let mut private = config("private");
    private.name = "Evening private".into();
    music_radios::create(&state, owner, &private).await.unwrap();
    let matches = music_radios::search(&state, None, false, None, "EVENING")
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].id, target.id);
    assert!(
        music_radios::search(&state, None, false, None, "private_setting")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        music_radios::search(&state, Some(owner), true, None, "evening")
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(
        music_radios::search(&state, None, false, Some(0), "")
            .await
            .is_err()
    );
    assert!(
        music_radios::search(&state, None, false, None, &"x".repeat(201))
            .await
            .is_err()
    );
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        music_radios::search(&state, Some(viewer), false, None, "evening")
            .await
            .unwrap()
            .is_empty()
    );
}
