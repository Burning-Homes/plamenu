//! Music erasure must work without relying on deletion of the actor row.
mod common;
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use plamenu::{AppState, entities, moderation, music_favourites, music_queues::Source};
use plamenu_db::{
    PgPool,
    account::{self, Account},
    id,
};
use serde_json::Value;

struct Fixture {
    state: AppState,
    alice: Account,
    bob: Account,
    owned: i64,
    foreign: i64,
}
async fn subscribe(state: &AppState, follower: i64, owner: i64, target: &str, kind: &str) {
    sqlx::query("INSERT INTO music_follows(follower_id,owner_id,target_uri,target_kind,target_name,local_target,follow_uri,pending) VALUES($1,$2,$3,$4,'Subscribed music',true,$5,false)")
        .bind(follower).bind(owner).bind(target).bind(kind).bind(format!("https://songhive.test/follow/{}",id::next())).execute(&state.pool).await.unwrap();
}
async fn fixture(pool: PgPool) -> Fixture {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let mut assets = Vec::new();
    for member in [&alice, &bob] {
        let artist = id::next();
        let album = id::next();
        let asset = id::next();
        let collection = id::next();
        let radio = id::next();
        sqlx::query("INSERT INTO music_artists(id,owner_id,name,description) VALUES($1,$2,'Artist','Private biography')").bind(artist).bind(member.id).execute(&state.pool).await.unwrap();
        sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,description) VALUES($1,$2,$3,'Album','Private notes')").bind(album).bind(member.id).bind(artist).execute(&state.pool).await.unwrap();
        sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Track',$3,$4,'public',$5,$6,100,200,$7,10,'mp3')").bind(asset).bind(member.id).bind(artist).bind(album).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
        // Empty metadata survives source cascades and must be erased explicitly.
        let empty_artist = id::next();
        sqlx::query("INSERT INTO music_artists(id,owner_id,name,description) VALUES($1,$2,'Empty artist','Unpublished biography')").bind(empty_artist).bind(member.id).execute(&state.pool).await.unwrap();
        sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,description) VALUES($1,$2,$3,'Empty album','Unpublished notes')").bind(id::next()).bind(member.id).bind(empty_artist).execute(&state.pool).await.unwrap();
        sqlx::query("INSERT INTO music_collections(id,owner_id,name,description,kind) VALUES($1,$2,'Playlist','Private description','playlist')").bind(collection).bind(member.id).execute(&state.pool).await.unwrap();
        sqlx::query("INSERT INTO music_radios(id,owner_id,name,config) VALUES($1,$2,'Radio','Private station preferences')").bind(radio).bind(member.id).execute(&state.pool).await.unwrap();
        let recipient = if member.id == alice.id {
            bob.id
        } else {
            alice.id
        };
        for query in [
            "INSERT INTO music_asset_grants(asset_id,account_id) VALUES($1,$2)",
            "INSERT INTO music_collection_grants(collection_id,account_id) VALUES($1,$2)",
            "INSERT INTO music_radio_grants(radio_id,account_id) VALUES($1,$2)",
        ]
        .into_iter()
        .zip([asset, collection, radio])
        {
            sqlx::query(query.0)
                .bind(query.1)
                .bind(recipient)
                .execute(&state.pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO music_artworks(id,owner_id,artist_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,'image',$4,'image/png',50,1,1,$5)").bind(id::next()).bind(member.id).bind(artist).bind(format!("musicart{}.png", member.id)).bind(id::next()).execute(&state.pool).await.unwrap();
        assets.push((asset, collection));
    }
    // Private references to someone else's surviving source do not cascade.
    for (member, source, collection) in [
        (alice.id, assets[1].0, assets[0].1),
        (bob.id, assets[1].0, assets[1].1),
    ] {
        sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) VALUES($1,$2,$3,1)").bind(id::next()).bind(collection).bind(source).execute(&state.pool).await.unwrap();
        let queue = id::next();
        sqlx::query("INSERT INTO music_queues(id,account_id,device_key) VALUES($1,$2,'browser')")
            .bind(queue)
            .bind(member)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO music_queue_entries(id,queue_id,asset_id,position) VALUES($1,$2,$3,1)",
        )
        .bind(id::next())
        .bind(queue)
        .bind(source)
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO music_listening_preferences(account_id,minimum_seconds) VALUES($1,1)",
        )
        .bind(member)
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO music_listen_sessions(id,token,account_id,request_key,asset_id,duration,threshold_seconds,counted_at,history_id) VALUES($1,$2,$3,'played',$4,10,1,now(),$5)").bind(id::next()).bind(format!("{member:064x}")).bind(member).bind(source).bind(id::next()).execute(&state.pool).await.unwrap();
        music_favourites::save(
            &state,
            member,
            &Source {
                remote_id: None,
                episode_id: None,
                asset_id: Some(source.to_string()),
                status_id: None,
            },
        )
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO music_follows(follower_id,owner_id,target_uri,target_kind,target_name,local_target,follow_uri,pending) VALUES($1,$2,$3,'Library','Saved library',true,$4,false)").bind(alice.id).bind(bob.id).bind(plamenu::music_follows::library_uri(TEST_DOMAIN, &bob)).bind(format!("https://{TEST_DOMAIN}/follow/{}",id::next())).execute(&state.pool).await.unwrap();
    Fixture {
        state,
        alice,
        bob,
        owned: assets[0].0,
        foreign: assets[1].0,
    }
}
// SQL identifiers below are exclusively from these constant test inventories.
// Account ids remain bound parameters; no request data supplies an identifier.
async fn snapshot(pool: &PgPool) -> Value {
    let mut snapshot = serde_json::Map::new();
    for table in [
        "accounts",
        "account_deletion_requests",
        "actor_keys",
        "statuses",
        "status_music",
        "media_attachments",
        "music_publications",
        "users",
        "music_artists",
        "music_albums",
        "music_assets",
        "music_artworks",
        "music_collections",
        "music_collection_entries",
        "music_radios",
        "music_radio_grants",
        "music_asset_grants",
        "music_album_grants",
        "music_collection_grants",
        "music_follows",
        "music_queues",
        "music_queue_entries",
        "music_listen_sessions",
        "music_listening_preferences",
        "music_favourites",
        "music_saved_resources",
        "notifications",
        "music_storage_debt",
        "media_cleanup_jobs",
        "delivery_jobs",
    ] {
        let rows:Value=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY to_jsonb(r)::text),'[]'::jsonb) FROM {table} r"))).fetch_one(pool).await.unwrap();
        snapshot.insert(table.into(), rows);
    }
    Value::Object(snapshot)
}
async fn assert_erased(f: &Fixture) {
    for (table, column) in [
        ("music_artists", "owner_id"),
        ("music_albums", "owner_id"),
        ("music_assets", "owner_id"),
        ("music_artworks", "owner_id"),
        ("music_collections", "owner_id"),
        ("music_radios", "owner_id"),
        ("music_queues", "account_id"),
        ("music_listen_sessions", "account_id"),
        ("music_listening_preferences", "account_id"),
        ("music_favourites", "account_id"),
        ("music_saved_resources", "account_id"),
        ("music_asset_grants", "account_id"),
        ("music_album_grants", "account_id"),
        ("music_collection_grants", "account_id"),
        ("music_radio_grants", "account_id"),
        ("music_follows", "follower_id"),
        ("music_follows", "owner_id"),
    ] {
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE {column}=$1"
        )))
        .bind(f.alice.id)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "retained account data in {table}.{column}");
    }
    for table in [
        "music_artists",
        "music_albums",
        "music_assets",
        "music_artworks",
        "music_collections",
        "music_radios",
    ] {
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE owner_id=$1"
        )))
        .bind(f.bob.id)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
        assert!(count > 0, "foreign resources removed from {table}");
    }
    assert!(
        plamenu::music_assets::get(&f.state, f.foreign, Some(f.bob.id))
            .await
            .is_ok()
    );
    let debt: i64 = sqlx::query_scalar(
        "SELECT sum(size_bytes)::bigint FROM music_storage_debt WHERE owner_id=$1",
    )
    .bind(f.alice.id)
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(debt, 350);
    for key in [
        format!("music{}.orig", f.owned),
        format!("music{}.mp3", f.owned),
        format!("musicart{}.png", f.alice.id),
    ] {
        let queued: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM media_cleanup_jobs WHERE file_name=$1)",
        )
        .bind(key)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
        assert!(queued);
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_queue_entries")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_listen_sessions")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        1
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn purge_erases_empty_metadata_and_foreign_references_without_deleted_marker(pool: PgPool) {
    let f = fixture(pool).await;
    let before = snapshot(&f.state.pool).await;
    let mut tx = f.state.pool.begin().await.unwrap();
    account::purge_local_data_conn(&mut tx, f.alice.id)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(snapshot(&f.state.pool).await, before);
    account::purge_local_data(&f.state.pool, f.alice.id)
        .await
        .unwrap();
    assert!(
        !account::is_deleted(&f.state.pool, f.alice.id)
            .await
            .unwrap()
    );
    assert_erased(&f).await;
    let tombstone = account::find_by_id(&f.state.pool, f.alice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tombstone.username, f.alice.username);
    assert_eq!(tombstone.public_key, f.alice.public_key);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn self_delete_retracts_scoped_only_subscribers_and_rolls_back_at_outbox(pool: PgPool) {
    let f = fixture(pool).await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let subscriber = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    for uri in [
        plamenu::music_follows::library_uri(TEST_DOMAIN, &f.alice),
        plamenu::music_entities::uri(
            TEST_DOMAIN,
            plamenu::music_entities::Kind::Artist,
            sqlx::query_scalar("SELECT artist_id FROM music_assets WHERE id=$1")
                .bind(f.owned)
                .fetch_one(&f.state.pool)
                .await
                .unwrap(),
        ),
    ] {
        let kind = if uri.ends_with("/library") {
            "Library"
        } else {
            "Artist"
        };
        subscribe(&f.state, subscriber.id, f.alice.id, &uri, kind).await;
    }
    assert!(
        plamenu_db::follow::find(&f.state.pool, subscriber.id, f.alice.id)
            .await
            .unwrap()
            .is_none()
    );
    let before = snapshot(&f.state.pool).await;
    sqlx::query("CREATE FUNCTION reject_music_delete_delivery() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END; $$ LANGUAGE plpgsql").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_music_delete_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_music_delete_delivery()").execute(&f.state.pool).await.unwrap();
    assert!(
        moderation::self_delete_account(&f.state, &f.alice)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f.state.pool).await, before);
    sqlx::query("DROP TRIGGER reject_music_delete_delivery ON delivery_jobs")
        .execute(&f.state.pool)
        .await
        .unwrap();
    moderation::self_delete_account(&f.state, &f.alice)
        .await
        .unwrap();
    assert_erased(&f).await;
    assert!(
        account::is_deleted(&f.state.pool, f.alice.id)
            .await
            .unwrap()
    );
    let deliveries: Vec<Value> = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE account_id=$1 AND activity->>'type'='Delete'",
    )
    .bind(f.alice.id)
    .fetch_all(&f.state.pool)
    .await
    .unwrap();
    let actor_uri = entities::account_uri(TEST_DOMAIN, &f.alice);
    let actor_deletes = deliveries
        .iter()
        .filter(|a| a["object"]["id"] == actor_uri)
        .collect::<Vec<_>>();
    assert_eq!(
        actor_deletes.len(),
        1,
        "duplicate resource subscriptions share one actor retraction"
    );
    assert_eq!(
        actor_deletes[0]["actor"],
        entities::account_uri(TEST_DOMAIN, &f.alice)
    );
    assert_eq!(
        actor_deletes[0]["object"]["id"],
        entities::account_uri(TEST_DOMAIN, &f.alice)
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn profile_and_suspension_updates_reach_music_subscribers_without_social_posts(pool: PgPool) {
    let f = fixture(pool).await;
    let library = plamenu::music_follows::library_uri(TEST_DOMAIN, &f.alice);
    for (domain, blocked, pending, suspended) in [
        ("allowed.test", false, false, false),
        ("blocked.test", true, false, false),
        ("pending.test", false, true, false),
        ("suspended.test", false, false, true),
    ] {
        let remote = RemoteUser::new(domain, "listener");
        let listener = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
            .await
            .unwrap();
        subscribe(&f.state, listener.id, f.alice.id, &library, "Library").await;
        if blocked {
            plamenu_db::block::create(&f.state.pool, listener.id, f.alice.id, None)
                .await
                .unwrap();
        }
        if pending {
            sqlx::query("UPDATE music_follows SET pending=true WHERE follower_id=$1")
                .bind(listener.id)
                .execute(&f.state.pool)
                .await
                .unwrap();
        }
        if suspended {
            account::suspend(&f.state.pool, listener.id, "local")
                .await
                .unwrap();
        }
    }
    plamenu::profile::update_profile(
        &f.state,
        &f.alice,
        plamenu::profile::ProfileChanges {
            display_name: Some("Changed performer".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let updates: Vec<(String, Value)> = sqlx::query_as(
        "SELECT inbox_url,activity FROM delivery_jobs WHERE activity->>'type'='Update'",
    )
    .fetch_all(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(updates.len(), 1);
    assert!(updates[0].0.starts_with("https://allowed.test/"));
    assert_eq!(updates[0].1["object"]["name"], "Changed performer");
    let (_, reach) = plamenu::actions::post_status(
        &f.state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "An unrelated social post",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(reach, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Create'"
        )
        .fetch_one(&f.state.pool)
        .await
        .unwrap(),
        0
    );
    account::suspend(&f.state.pool, f.alice.id, "local")
        .await
        .unwrap();
    let suspended = account::find_by_id(&f.state.pool, f.alice.id)
        .await
        .unwrap()
        .unwrap();
    plamenu::profile::fan_out_actor_update(&f.state, &suspended)
        .await
        .unwrap();
    let blanked:i64=sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE activity->'object'->>'suspended'='true' AND inbox_url LIKE 'https://allowed.test/%'").fetch_one(&f.state.pool).await.unwrap();
    assert_eq!(blanked, 1);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cached_audio_retractions_reach_each_scope_and_preserve_foreign_sources(pool: PgPool) {
    let f = fixture(pool).await;
    let artist: i64 = sqlx::query_scalar("SELECT artist_id FROM music_assets WHERE id=$1")
        .bind(f.owned)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    let album: i64 = sqlx::query_scalar("SELECT album_id FROM music_assets WHERE id=$1")
        .bind(f.owned)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    let library:i64=sqlx::query_scalar("UPDATE music_collections SET kind='library',visibility='public' WHERE owner_id=$1 RETURNING id").bind(f.alice.id).fetch_one(&f.state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) VALUES($1,$2,$3,2)").bind(id::next()).bind(library).bind(f.owned).execute(&f.state.pool).await.unwrap();
    let targets = [
        plamenu::music_follows::library_uri(TEST_DOMAIN, &f.alice),
        plamenu::music_entities::uri(TEST_DOMAIN, plamenu::music_entities::Kind::Artist, artist),
        plamenu::music_library_federation::uri(TEST_DOMAIN, library),
    ];
    for ((domain, kind), target) in [
        ("implicit.test", "Library"),
        ("artist.test", "Artist"),
        ("named.test", "Library"),
    ]
    .into_iter()
    .zip(&targets)
    {
        let remote = RemoteUser::new(domain, "listener");
        let listener = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
            .await
            .unwrap();
        subscribe(&f.state, listener.id, f.alice.id, target, kind).await;
    }
    let published = plamenu::music_publications::publish(&f.state, &f.alice, f.owned)
        .await
        .unwrap();
    let foreign = plamenu::music_publications::publish(&f.state, &f.bob, f.foreign)
        .await
        .unwrap();
    let mut hidden = Vec::new();
    for visibility in ["private", "unlisted"] {
        let media = id::next();
        let mut input =
            plamenu_db::media::NewLocalMedia::new(f.alice.id, media, "tone.mp3", "audio/mpeg");
        input.kind = "audio";
        input.duration = Some(3.0);
        plamenu_db::media::create_local(&f.state.pool, input)
            .await
            .unwrap();
        hidden.push(
            plamenu::actions::post_status(
                &f.state,
                plamenu::actions::PostParams {
                    username: "alice",
                    title: Some("A hidden recording"),
                    visibility,
                    media_ids: &[media],
                    kind: plamenu_ap::activity::PostKind::Audio,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .0,
        );
    }
    sqlx::query("DELETE FROM delivery_jobs")
        .execute(&f.state.pool)
        .await
        .unwrap();
    // Administrative permanent deletion starts from a suspended controller.
    account::suspend(&f.state.pool, f.alice.id, "local")
        .await
        .unwrap();
    let suspended = account::find_by_id(&f.state.pool, f.alice.id)
        .await
        .unwrap()
        .unwrap();
    moderation::destroy_suspended_account(&f.state, f.bob.id, &suspended)
        .await
        .unwrap();
    let deliveries: Vec<(String, Value)> = sqlx::query_as(
        "SELECT inbox_url,activity FROM delivery_jobs WHERE activity->>'type'='Delete'",
    )
    .fetch_all(&f.state.pool)
    .await
    .unwrap();
    let root = entities::status_uri_for_account(TEST_DOMAIN, &published, &f.alice);
    let audio = deliveries
        .iter()
        .filter(|(_, a)| a["object"]["id"] == root)
        .collect::<Vec<_>>();
    assert_eq!(audio.len(), 3);
    for prefix in [
        "https://implicit.test/",
        "https://artist.test/",
        "https://named.test/",
    ] {
        assert!(audio.iter().any(|(inbox, _)| inbox.starts_with(prefix)));
    }
    for target in [
        format!("{root}/track"),
        plamenu::music_entities::uri(TEST_DOMAIN, plamenu::music_entities::Kind::Artist, artist),
        plamenu::music_entities::uri(TEST_DOMAIN, plamenu::music_entities::Kind::Album, album),
    ] {
        assert!(
            deliveries.iter().any(|(_, a)| a["object"]["id"] == target),
            "missing metadata retraction {target}"
        );
    }
    for status in hidden.iter().chain(std::iter::once(&foreign)) {
        let author = if status.account_id == f.alice.id {
            &f.alice
        } else {
            &f.bob
        };
        let uri = entities::status_uri_for_account(TEST_DOMAIN, status, author);
        assert!(
            deliveries.iter().all(|(_, a)| a["object"]["id"] != uri),
            "retracted private, unrelated unlisted or foreign music {uri}"
        );
    }
    assert_eq!(
        plamenu::music_publications::published(&f.state, f.bob.id, f.foreign)
            .await
            .unwrap()
            .unwrap()
            .id,
        foreign.id
    );
    assert_erased(&f).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn a_follow_resolved_before_retirement_cannot_recreate_erased_subscriptions(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let library:i64=sqlx::query_scalar("UPDATE music_collections SET kind='library',visibility='public' WHERE owner_id=$1 RETURNING id").bind(f.alice.id).fetch_one(&pool).await.unwrap();
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let sender = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    let worker_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let worker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&worker_pool)
        .await
        .unwrap();
    let worker_state = test_state_with(worker_pool, StubFederation::with_actors([]));
    let mut retirement = pool.begin().await.unwrap();
    let retiring_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *retirement)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR UPDATE")
        .bind(f.alice.id)
        .execute(&mut *retirement)
        .await
        .unwrap();
    let follow = serde_json::json!({"id":"https://songhive.test/follow/racing","type":"Follow","actor":remote.actor.id,"object":plamenu::music_library_federation::uri(TEST_DOMAIN,library)});
    let following = tokio::spawn(async move {
        plamenu::music_follows::incoming(&worker_state, &sender, &follow).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $2=ANY(pg_blocking_pids($1))")
                .bind(worker_pid)
                .bind(retiring_pid)
                .fetch_one(&pool)
                .await
                .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Follow must reach the retiring controller lock");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        moderation::self_delete_account_conn(&f.state, &mut retirement, &f.alice),
    )
    .await
    .expect("retirement must not wait on the followed collection")
    .unwrap();
    retirement.commit().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), following)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_follows WHERE owner_id=$1")
            .bind(f.alice.id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Accept'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Reject'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn grace_period_expiry_keeps_music_until_scoped_retractions_commit(pool: PgPool) {
    let f = fixture(pool).await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let listener = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    subscribe(
        &f.state,
        listener.id,
        f.alice.id,
        &plamenu::music_follows::library_uri(TEST_DOMAIN, &f.alice),
        "Library",
    )
    .await;
    let published = plamenu::music_publications::publish(&f.state, &f.alice, f.owned)
        .await
        .unwrap();
    sqlx::query("DELETE FROM delivery_jobs")
        .execute(&f.state.pool)
        .await
        .unwrap();
    account::suspend(&f.state.pool, f.alice.id, "local")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE account_deletion_requests SET due_at=now()-interval '1 day' WHERE account_id=$1",
    )
    .bind(f.alice.id)
    .execute(&f.state.pool)
    .await
    .unwrap();
    let before = snapshot(&f.state.pool).await;
    sqlx::query("CREATE FUNCTION reject_expiry_delivery() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected expiry outbox failure'; END; $$ LANGUAGE plpgsql").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_expiry_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_expiry_delivery()").execute(&f.state.pool).await.unwrap();
    plamenu::maintenance::run_daily(&f.state).await;
    assert!(
        !account::is_deleted(&f.state.pool, f.alice.id)
            .await
            .unwrap(),
        "an outbox failure must retain the reversible suspended account"
    );
    assert_eq!(snapshot(&f.state.pool).await, before);
    sqlx::query("DROP TRIGGER reject_expiry_delivery ON delivery_jobs")
        .execute(&f.state.pool)
        .await
        .unwrap();
    plamenu::maintenance::run_daily(&f.state).await;
    assert!(
        account::is_deleted(&f.state.pool, f.alice.id)
            .await
            .unwrap()
    );
    assert_erased(&f).await;
    let actor = entities::account_uri(TEST_DOMAIN, &f.alice);
    let audio = entities::status_uri_for_account(TEST_DOMAIN, &published, &f.alice);
    for target in [actor, audio] {
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE account_id=$1 AND activity->>'type'='Delete' AND activity->'object'->>'id'=$2").bind(f.alice.id).bind(target).fetch_one(&f.state.pool).await.unwrap();
        assert_eq!(
            count, 1,
            "one scoped subscriber must receive its controller and Audio retractions"
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn expiry_ignores_future_deadlines_and_restored_accounts_without_outbox(pool: PgPool) {
    let f = fixture(pool).await;
    account::suspend(&f.state.pool, f.alice.id, "local")
        .await
        .unwrap();
    let before = snapshot(&f.state.pool).await;
    assert!(
        !moderation::purge_expired_suspension(&f.state, f.alice.id)
            .await
            .unwrap()
    );
    assert_eq!(snapshot(&f.state.pool).await, before);
    sqlx::query(
        "UPDATE account_deletion_requests SET due_at=now()-interval '1 day' WHERE account_id=$1",
    )
    .bind(f.alice.id)
    .execute(&f.state.pool)
    .await
    .unwrap();
    assert!(account::unsuspend(&f.state.pool, f.alice.id).await.unwrap());
    let restored = snapshot(&f.state.pool).await;
    assert!(
        !moderation::purge_expired_suspension(&f.state, f.alice.id)
            .await
            .unwrap()
    );
    assert_eq!(snapshot(&f.state.pool).await, restored);
    assert!(
        plamenu::music_assets::get(&f.state, f.owned, None)
            .await
            .is_ok()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn restoration_winning_the_expiry_lock_keeps_music_and_produces_no_delete(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    account::suspend(&pool, f.alice.id, "local").await.unwrap();
    sqlx::query(
        "UPDATE account_deletion_requests SET due_at=now()-interval '1 day' WHERE account_id=$1",
    )
    .bind(f.alice.id)
    .execute(&pool)
    .await
    .unwrap();
    let worker_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let worker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&worker_pool)
        .await
        .unwrap();
    let worker_state = test_state_with(worker_pool, StubFederation::with_actors([]));
    let mut restoring = pool.begin().await.unwrap();
    let restoring_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *restoring)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(f.alice.id)
        .execute(&mut *restoring)
        .await
        .unwrap();
    let owner = f.alice.id;
    let expiring =
        tokio::spawn(
            async move { moderation::purge_expired_suspension(&worker_state, owner).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $2=ANY(pg_blocking_pids($1))")
                .bind(worker_pid)
                .bind(restoring_pid)
                .fetch_one(&pool)
                .await
                .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expiry must reach the held restoration lock");
    assert!(
        account::unsuspend_conn(&mut restoring, owner)
            .await
            .unwrap()
    );
    restoring.commit().await.unwrap();
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(10), expiring)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    assert!(!account::is_deleted(&pool, owner).await.unwrap());
    assert!(
        plamenu::music_assets::get(&f.state, f.owned, None)
            .await
            .is_ok()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn hard_delete_erases_metadata_without_managed_files(pool: PgPool) {
    let owner = create_local_account(&pool, "metadata_owner", "Metadata owner").await;
    let other = create_local_account(&pool, "metadata_survivor", "Metadata survivor").await;
    let artist = id::next();
    for (account, entity, name) in [
        (owner.id, artist, "Unpublished private biography"),
        (other.id, id::next(), "Surviving artist"),
    ] {
        sqlx::query("INSERT INTO music_artists(id,owner_id,name,description) VALUES($1,$2,$3,'Private notes')")
            .bind(entity).bind(account).bind(name).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,description) VALUES($1,$2,$3,'Empty album','Unpublished notes')")
        .bind(id::next()).bind(owner.id).bind(artist).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO music_radios(id,owner_id,name,config) VALUES($1,$2,'Private station','Private station settings')")
        .bind(id::next()).bind(owner.id).execute(&pool).await.unwrap();
    assert!(account::delete_by_id(&pool, owner.id).await.unwrap());
    for query in [
        "SELECT count(*) FROM music_artists WHERE owner_id=$1",
        "SELECT count(*) FROM music_albums WHERE owner_id=$1",
        "SELECT count(*) FROM music_radios WHERE owner_id=$1",
    ] {
        let remaining: i64 = sqlx::query_scalar(query)
            .bind(owner.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }
    let surviving: i64 = sqlx::query_scalar("SELECT count(*) FROM music_artists WHERE owner_id=$1")
        .bind(other.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(surviving, 1);
    assert!(
        account::find_by_id(&pool, owner.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn uri_hard_delete_captures_profile_keys_before_metadata_cascades(pool: PgPool) {
    let owner =
        common::create_immutable_local_account(&pool, "uri_metadata_owner", "URI metadata owner")
            .await;
    let other = create_local_account(&pool, "uri_metadata_survivor", "URI metadata survivor").await;
    for (account, name) in [
        (owner.id, "Private biography"),
        (other.id, "Surviving artist"),
    ] {
        sqlx::query("INSERT INTO music_artists(id,owner_id,name,description) VALUES($1,$2,$3,'Private notes')")
            .bind(id::next()).bind(account).bind(name).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE accounts SET avatar_file_name='uri-erased-avatar.png',header_file_name='uri-erased-header.png' WHERE id=$1")
        .bind(owner.id).execute(&pool).await.unwrap();
    assert!(
        account::delete_by_uri(&pool, owner.uri.as_deref().unwrap())
            .await
            .unwrap()
    );
    assert!(
        account::find_by_id(&pool, owner.id)
            .await
            .unwrap()
            .is_none()
    );
    let erased: i64 = sqlx::query_scalar("SELECT count(*) FROM music_artists WHERE owner_id=$1")
        .bind(owner.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(erased, 0);
    let surviving: i64 = sqlx::query_scalar("SELECT count(*) FROM music_artists WHERE owner_id=$1")
        .bind(other.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(surviving, 1);
    let captured: i64 = sqlx::query_scalar("SELECT count(*) FROM media_cleanup_jobs WHERE file_name IN ('uri-erased-avatar.png','uri-erased-header.png')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(captured, 2);
    assert!(
        !account::delete_by_uri(&pool, owner.uri.as_deref().unwrap())
            .await
            .unwrap()
    );
}
