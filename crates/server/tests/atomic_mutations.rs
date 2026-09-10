//! Failure injection at the outbox boundary, covering mutations and retries.
mod common;

use common::{RemoteUser, create_local_account, test_state_with};
use plamenu::{AppState, actions};
use plamenu_db::{PgPool, account::Account, follow, group, status::Status};
use serde_json::Value;

async fn fixture(pool: &PgPool) -> (AppState, Account, Status, Account) {
    let alice = create_local_account(pool, "alice", "Alice").await;
    let remote = RemoteUser::new("remote.example", "bob");
    let bob = plamenu::remote::store_remote_actor(pool, &remote.actor)
        .await
        .unwrap();
    follow::create(pool, bob.id, alice.id, None).await.unwrap();
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let (community, _) = group::create(
        pool,
        group::NewLocalGroup {
            account: plamenu_db::account::NewLocalAccount {
                username: "hiking",
                display_name: "Hiking",
                note: "",
                public_key_pem: "pub",
            },
            membership_policy: group::MembershipPolicy::Open,
            posting_policy: group::PostingPolicy::Anyone,
            created_by: alice.id,
        },
    )
    .await
    .unwrap();
    follow::create(pool, bob.id, community.id, None)
        .await
        .unwrap();
    let (post, _) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "before #old",
            visibility: "public",
            group_id: Some(community.id),
            title: Some("Topic"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM delivery_jobs")
        .execute(pool)
        .await
        .unwrap();
    (state, alice, post, community)
}

async fn fail_deliveries(pool: &PgPool) {
    sqlx::query("CREATE OR REPLACE FUNCTION reject_delivery() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END; $$ LANGUAGE plpgsql").execute(pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_delivery()").execute(pool).await.unwrap();
}

async fn recover(pool: &PgPool) {
    sqlx::query("DROP TRIGGER reject_delivery ON delivery_jobs")
        .execute(pool)
        .await
        .unwrap();
}

async fn snapshot(pool: &PgPool) -> Value {
    let mut rows = serde_json::Map::new();
    for table in [
        "accounts",
        "follows",
        "groups",
        "group_affiliations",
        "group_locked_posts",
        "collections",
        "collection_items",
        "reports",
        "statuses",
        "status_edits",
        "status_mentions",
        "status_tags",
        "status_events",
        "media_attachments",
        "media_cleanup_jobs",
        "favourites",
        "status_dislikes",
        "status_reactions",
        "status_pins",
        "featured_tags",
        "status_tombstones",
        "delivery_jobs",
        "tagged_objects",
        "preview_cards_statuses",
    ] {
        let query = format!(
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM {table} t"
        );
        let value: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .fetch_one(pool)
            .await
            .unwrap();
        rows.insert(table.to_owned(), value);
    }
    Value::Object(rows)
}

async fn apply(
    state: &AppState,
    actor: &Account,
    post: &Status,
    verb: &str,
) -> Result<(), plamenu::error::ApiError> {
    match verb {
        "edit" => {
            actions::edit_status(
                state,
                actor,
                post.id,
                actions::EditParams {
                    text: Some("after #new"),
                    ..Default::default()
                },
            )
            .await?;
        }
        "boost" => {
            actions::reblog_status(state, actor, post.id).await?;
        }
        "unboost" => {
            actions::unreblog_status(state, actor, post.id).await?;
        }
        "favourite" => {
            actions::favourite_status(state, actor, post.id).await?;
        }
        "unfavourite" => {
            actions::unfavourite_status(state, actor, post.id).await?;
        }
        "downvote" => {
            actions::downvote_status(state, actor, post.id).await?;
        }
        "undownvote" => {
            actions::undownvote_status(state, actor, post.id).await?;
        }
        "reaction" => {
            actions::react_with_emoji(state, actor, post.id, "🔥").await?;
        }
        "unreaction" => {
            actions::unreact_with_emoji(state, actor, post.id, "🔥").await?;
        }
        "delete" => {
            actions::delete_status(state, actor, post.id, actions::DeleteMode::Wipe).await?;
        }
        "stub" => {
            actions::delete_status(state, actor, post.id, actions::DeleteMode::Stub).await?;
        }
        "pin" => {
            actions::pin_status(state, actor, post.id).await?;
        }
        "unpin" => {
            actions::unpin_status(state, actor, post.id).await?;
        }
        "policy" => {
            actions::set_interaction_policy(state, actor, post.id, 0).await?;
        }
        "feature" | "unfeature" | "unfeature_by_id" => {
            let tag = plamenu_db::tag::ensure(&state.pool, "old").await?;
            match verb {
                "feature" => {
                    actions::feature_tag(state, actor, tag, "old").await?;
                }
                "unfeature" => actions::unfeature_tag(state, actor, tag, "old").await?,
                _ => {
                    let row: i64 = sqlx::query_scalar(
                        "SELECT id FROM featured_tags WHERE account_id = $1 AND tag_id = $2",
                    )
                    .bind(actor.id)
                    .bind(tag)
                    .fetch_one(&state.pool)
                    .await
                    .map_err(plamenu_db::DbError::from)?;
                    assert!(actions::unfeature_tag_by_id(state, actor, row).await?);
                }
            }
        }
        _ => panic!("unknown mutation {verb}"),
    }
    Ok(())
}

async fn rollback_and_retry(pool: PgPool, verb: &str) {
    let (state, alice, post, _) = fixture(&pool).await;
    let initial = match verb {
        "unboost" => Some("boost"),
        "unfavourite" | "downvote" => Some("favourite"),
        "undownvote" | "favourite" => Some("downvote"),
        "unreaction" => Some("reaction"),
        "unpin" => Some("pin"),
        "unfeature" | "unfeature_by_id" => Some("feature"),
        _ => None,
    };
    if let Some(initial) = initial {
        apply(&state, &alice, &post, initial).await.unwrap();
    }
    if verb == "stub" {
        actions::post_status(
            &state,
            actions::PostParams {
                username: "alice",
                text: "reply",
                visibility: "public",
                in_reply_to_id: Some(post.id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    // Retractions must enqueue an Undo, not merely cancel an unattempted job.
    sqlx::query("UPDATE delivery_jobs SET attempts = 1")
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    fail_deliveries(&pool).await;
    let failure = apply(&state, &alice, &post, verb)
        .await
        .expect_err("injected enqueue must fail");
    assert!(
        format!("{failure:?}").contains("injected outbox failure"),
        "{failure}"
    );
    assert_eq!(
        snapshot(&pool).await,
        before,
        "{verb} must roll back domain rows, sidecars and cancellation of existing jobs"
    );
    recover(&pool).await;
    apply(&state, &alice, &post, verb).await.unwrap();
    assert_ne!(snapshot(&pool).await, before);
    let fresh: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE attempts = 0")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(fresh > 0, "{verb} retry must persist a new delivery");
}

macro_rules! mutation_test {
    ($name:ident, $verb:literal) => {
        #[sqlx::test(migrations = "../db/migrations")]
        async fn $name(pool: PgPool) {
            rollback_and_retry(pool, $verb).await;
        }
    };
}
mutation_test!(edit_rollback_and_retry, "edit");
mutation_test!(boost_rollback_and_retry, "boost");
mutation_test!(unboost_rollback_and_retry, "unboost");
mutation_test!(favourite_replaces_downvote_atomically, "favourite");
mutation_test!(unfavourite_rollback_and_retry, "unfavourite");
mutation_test!(downvote_replaces_favourite_atomically, "downvote");
mutation_test!(undownvote_rollback_and_retry, "undownvote");
mutation_test!(reaction_rollback_and_retry, "reaction");
mutation_test!(unreaction_rollback_and_retry, "unreaction");
mutation_test!(delete_rollback_and_retry, "delete");
mutation_test!(stub_rollback_and_retry, "stub");
mutation_test!(pin_rollback_and_retry, "pin");
mutation_test!(unpin_rollback_and_retry, "unpin");
mutation_test!(policy_rollback_and_retry, "policy");
mutation_test!(feature_rollback_and_retry, "feature");
mutation_test!(unfeature_rollback_and_retry, "unfeature");
mutation_test!(unfeature_by_id_rollback_and_retry, "unfeature_by_id");

#[sqlx::test(migrations = "../db/migrations")]
async fn failure_in_group_fanout_rolls_back_author_jobs_and_group_boost(pool: PgPool) {
    let (state, alice, post, community) = fixture(&pool).await;
    // Author deliveries succeed; only the later group wrapper insert fails.
    let trigger = format!(
        "CREATE FUNCTION reject_group_delivery() RETURNS trigger AS $$ BEGIN IF NEW.account_id = {} THEN RAISE EXCEPTION 'group outbox failure'; END IF; RETURN NEW; END; $$ LANGUAGE plpgsql",
        community.id
    );
    sqlx::query(sqlx::AssertSqlSafe(trigger))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_group_delivery()").execute(&pool).await.unwrap();
    let before = snapshot(&pool).await;
    let error = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "new group post",
            visibility: "public",
            group_id: Some(community.id),
            title: Some("Another topic"),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{error:?}").contains("group outbox failure"));
    assert_eq!(snapshot(&pool).await, before);
    for verb in ["edit", "delete"] {
        let error = apply(&state, &alice, &post, verb).await.unwrap_err();
        assert!(format!("{error:?}").contains("group outbox failure"));
        assert_eq!(
            snapshot(&pool).await,
            before,
            "{verb}: all author jobs roll back when group delivery fails"
        );
    }
    recover(&pool).await;
    apply(&state, &alice, &post, "edit").await.unwrap();
    let group_jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE account_id = $1")
            .bind(community.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(group_jobs > 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn event_and_media_edit_roll_back_and_retry_serializes_new_sidecars(pool: PgPool) {
    let (state, alice, _, _) = fixture(&pool).await;
    let upload = plamenu_db::media::create_local(
        &pool,
        plamenu_db::media::NewLocalMedia {
            description: Some("old description"),
            width: Some(10),
            height: Some(10),
            ..plamenu_db::media::NewLocalMedia::new(
                alice.id,
                plamenu_db::id::next(),
                "picture.jpg",
                "image/jpeg",
            )
        },
    )
    .await
    .unwrap();
    let (event, _) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "an event",
            title: Some("Meetup"),
            visibility: "public",
            media_ids: &[upload.id],
            event: Some(actions::EventParams {
                start_time: "2030-01-01T10:00:00Z".into(),
                join_mode: "free".into(),
                status: "CONFIRMED".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    let patch = || actions::EditParams {
        text: Some("moved #event"),
        event: Some(actions::EventPatch {
            start_time: Some("2030-01-02T10:00:00Z".into()),
            ..Default::default()
        }),
        media_attributes: vec![actions::MediaEditAttributes {
            id: upload.id,
            description: Some("new description".into()),
            focus: None,
        }],
        ..Default::default()
    };
    let before = snapshot(&pool).await;
    fail_deliveries(&pool).await;
    assert!(
        actions::edit_status(&state, &alice, event.id, patch())
            .await
            .is_err()
    );
    assert_eq!(snapshot(&pool).await, before);
    recover(&pool).await;
    actions::edit_status(&state, &alice, event.id, patch())
        .await
        .unwrap();
    let activity: Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type' = 'Update' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(activity["object"]["startTime"], "2030-01-02T10:00:00Z");
    assert_eq!(
        activity["object"]["attachment"][0]["name"],
        "new description"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn quote_revocation_rolls_back_and_retry_updates_the_quoting_note(pool: PgPool) {
    let (state, alice, post, _) = fixture(&pool).await;
    let (quoting, _) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "quoting",
            visibility: "public",
            quoted_status_id: Some(post.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    fail_deliveries(&pool).await;
    assert!(
        actions::revoke_quote(&state, &alice, post.id, quoting.id)
            .await
            .is_err()
    );
    let quote = plamenu_db::quote::for_statuses(&pool, &[quoting.id])
        .await
        .unwrap()
        .remove(&quoting.id)
        .unwrap();
    assert_eq!(quote.state, "accepted");
    recover(&pool).await;
    actions::revoke_quote(&state, &alice, post.id, quoting.id)
        .await
        .unwrap();
    let quote = plamenu_db::quote::for_statuses(&pool, &[quoting.id])
        .await
        .unwrap()
        .remove(&quoting.id)
        .unwrap();
    assert_eq!(quote.state, "revoked");
    assert!(plamenu_db::job::pending_count(&pool).await.unwrap() > 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn profile_and_account_deletion_roll_back_with_their_delivery(pool: PgPool) {
    let (state, alice, _, _) = fixture(&pool).await;
    let before: Value = sqlx::query_scalar("SELECT to_jsonb(a) FROM accounts a WHERE id = $1")
        .bind(alice.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    fail_deliveries(&pool).await;
    let changes = || plamenu::profile::ProfileChanges {
        display_name: Some("Changed".into()),
        attribution_domains: Some(vec!["author.example".into()]),
        ..Default::default()
    };
    assert!(
        plamenu::profile::update_profile(&state, &alice, changes())
            .await
            .is_err()
    );
    let after: Value = sqlx::query_scalar("SELECT to_jsonb(a) FROM accounts a WHERE id = $1")
        .bind(alice.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, before);
    let domain_before = snapshot(&pool).await;
    assert!(
        plamenu::moderation::self_delete_account(&state, &alice)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&pool).await, domain_before);
    let after: Value = sqlx::query_scalar("SELECT to_jsonb(a) FROM accounts a WHERE id = $1")
        .bind(alice.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, before);
    recover(&pool).await;
    let updated = plamenu::profile::update_profile(&state, &alice, changes())
        .await
        .unwrap();
    assert_eq!(updated.display_name, "Changed");
    plamenu::moderation::self_delete_account(&state, &updated)
        .await
        .unwrap();
    assert!(
        plamenu_db::account::is_deleted(&pool, alice.id)
            .await
            .unwrap()
    );
    assert!(plamenu_db::job::pending_count(&pool).await.unwrap() > 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn collection_lifecycle_rolls_back_with_its_delivery(pool: PgPool) {
    use plamenu::collections::{self, CollectionParams};
    let (state, alice, _, _) = fixture(&pool).await;
    let params = |name: &str| CollectionParams {
        name: name.into(),
        description: "description".into(),
        language: None,
        sensitive: false,
        discoverable: true,
        tag_name: None,
    };
    fail_deliveries(&pool).await;
    assert!(
        collections::create_collection(&state, &alice, params("People"), &[])
            .await
            .is_err()
    );
    assert_eq!(
        plamenu_db::collection::count_owned(&pool, alice.id)
            .await
            .unwrap(),
        0
    );
    recover(&pool).await;
    let collection = collections::create_collection(&state, &alice, params("People"), &[])
        .await
        .unwrap();
    sqlx::query("DELETE FROM delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    fail_deliveries(&pool).await;
    assert!(
        collections::update_collection(&state, &collection, &alice, params("Changed"))
            .await
            .is_err()
    );
    assert_eq!(
        plamenu_db::collection::find(&pool, collection.id)
            .await
            .unwrap()
            .unwrap()
            .name,
        "People"
    );
    assert!(
        collections::delete_collection(&state, &collection, &alice)
            .await
            .is_err()
    );
    assert!(
        plamenu_db::collection::find(&pool, collection.id)
            .await
            .unwrap()
            .is_some()
    );
    recover(&pool).await;
    let updated = collections::update_collection(&state, &collection, &alice, params("Changed"))
        .await
        .unwrap();
    collections::delete_collection(&state, &updated, &alice)
        .await
        .unwrap();
    assert!(
        plamenu_db::collection::find(&pool, collection.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn group_moderation_rolls_back_with_its_delivery(pool: PgPool) {
    let (state, alice, post, community) = fixture(&pool).await;
    let target = create_local_account(&pool, "charlie", "Charlie").await;
    follow::create(&pool, target.id, community.id, None)
        .await
        .unwrap();
    for operation in [
        "ban", "unban", "lock", "unlock", "pin", "unpin", "grant", "revoke", "remove",
    ] {
        let perform = || async {
            match operation {
                "ban" => {
                    plamenu::groups::ban_member(&state, &community, &alice, &target, None, None)
                        .await
                }
                "unban" => plamenu::groups::unban_member(&state, &community, &alice, &target).await,
                "lock" | "unlock" => {
                    plamenu::groups::set_thread_lock(
                        &state,
                        &community,
                        &alice,
                        &post,
                        operation == "lock",
                    )
                    .await
                }
                "pin" | "unpin" => {
                    plamenu::groups::set_group_pin(
                        &state,
                        &community,
                        &alice,
                        &post,
                        operation == "pin",
                    )
                    .await
                }
                "grant" | "revoke" => {
                    plamenu::groups::set_moderator(
                        &state,
                        &community,
                        &alice,
                        &target,
                        operation == "grant",
                    )
                    .await
                }
                _ => {
                    plamenu::groups::remove_from_group(
                        &state,
                        &community,
                        &alice,
                        &post,
                        "moderation",
                    )
                    .await
                }
            }
        };
        let before = snapshot(&pool).await;
        fail_deliveries(&pool).await;
        let error = perform().await.expect_err(operation);
        assert!(
            format!("{error:?}").contains("injected outbox failure"),
            "{operation}: {error:?}"
        );
        assert_eq!(snapshot(&pool).await, before, "{operation}");
        recover(&pool).await;
        perform().await.unwrap();
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn note_render_and_edit_use_a_single_pool_connection(pool: PgPool) {
    let (_, alice, post, _) = fixture(&pool).await;
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let state = test_state_with(single, std::sync::Arc::default());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        plamenu::note::note_for_status(&state, &post, &alice)
            .await
            .unwrap();
        actions::edit_status(
            &state,
            &alice,
            post.id,
            actions::EditParams {
                text: Some("edited"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    })
    .await
    .expect("render/edit must not hold a pool connection while asking for another");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn relay_handshakes_roll_back_with_their_delivery(pool: PgPool) {
    let (state, _, _, _) = fixture(&pool).await;
    let relay = plamenu_db::relay::create(&pool, "https://relay.example/inbox", None)
        .await
        .unwrap()
        .unwrap();
    for operation in ["enable", "disable", "enable", "remove"] {
        let perform = || async {
            match operation {
                "enable" => plamenu::relays::enable(&state, relay.id).await,
                "disable" => plamenu::relays::disable(&state, relay.id).await,
                _ => plamenu::relays::remove(&state, relay.id).await,
            }
        };
        let before = plamenu_db::relay::find(&pool, relay.id)
            .await
            .unwrap()
            .unwrap();
        fail_deliveries(&pool).await;
        let error = perform().await.unwrap_err();
        assert!(format!("{error:?}").contains("injected outbox failure"));
        let after = plamenu_db::relay::find(&pool, relay.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, before.state);
        assert_eq!(after.follow_activity_id, before.follow_activity_id);
        recover(&pool).await;
        assert!(perform().await.unwrap());
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_report_rolls_back_with_its_flag(pool: PgPool) {
    let (state, alice, _, _) = fixture(&pool).await;
    let target = plamenu_db::account::find_by_uri(&pool, "https://remote.example/users/bob")
        .await
        .unwrap()
        .unwrap();
    let perform = || {
        actions::create_report(
            &state,
            &alice,
            &target,
            actions::ReportParams {
                comment: "Please investigate",
                forward: true,
                ..Default::default()
            },
        )
    };
    let before = snapshot(&pool).await;
    fail_deliveries(&pool).await;
    let error = perform().await.unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    assert_eq!(snapshot(&pool).await, before);
    recover(&pool).await;
    perform().await.unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn account_key_rotation_rolls_back_publication_and_expiry(pool: PgPool) {
    let (state, _, _, _) = fixture(&pool).await;
    let owner = common::create_immutable_local_account(&pool, "dana", "Dana").await;
    let bob = plamenu_db::account::find_by_uri(&pool, "https://remote.example/users/bob")
        .await
        .unwrap()
        .unwrap();
    follow::create(&pool, bob.id, owner.id, None).await.unwrap();
    let keys = || async {
        sqlx::query_scalar::<_, Value>(
            "SELECT jsonb_agg(to_jsonb(k) ORDER BY id) FROM actor_keys k",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let before = keys().await;
    let perform = || {
        plamenu::key_store::rotate_account_key(
            &state,
            &owner,
            "ed25519",
            time::Duration::hours(24),
            time::Duration::minutes(5),
        )
    };
    fail_deliveries(&pool).await;
    let error = perform().await.unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    assert_eq!(keys().await, before);
    recover(&pool).await;
    perform().await.unwrap();
    assert_ne!(keys().await, before);
    plamenu::instance_actor::document(&state).await.unwrap();
    let before = keys().await;
    fail_deliveries(&pool).await;
    let error = plamenu::key_store::rotate_instance_key(
        &state,
        "ed25519",
        time::Duration::hours(24),
        time::Duration::minutes(5),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    assert_eq!(keys().await, before);
    recover(&pool).await;
    plamenu::key_store::rotate_instance_key(
        &state,
        "ed25519",
        time::Duration::hours(24),
        time::Duration::minutes(5),
    )
    .await
    .unwrap();
    assert_ne!(keys().await, before);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn group_profile_transfer_and_deletion_roll_back_with_delivery(pool: PgPool) {
    let (state, _, _, community) = fixture(&pool).await;
    let successor = create_local_account(&pool, "successor", "Successor").await;
    follow::create(&pool, successor.id, community.id, None)
        .await
        .unwrap();
    for operation in ["profile", "transfer", "delete"] {
        let perform = || async {
            match operation {
                "profile" => {
                    plamenu::groups::update_settings(
                        &state,
                        &community,
                        plamenu::groups::GroupSettings {
                            display_name: "Updated group",
                            note_html: "<p>updated</p>",
                            note_source: "updated",
                            policy: group::MembershipPolicy::Approval,
                            sensitive: true,
                            posting_policy: group::PostingPolicy::Anyone,
                            discoverable: true,
                            profile: plamenu::groups::GroupProfileEdit::default(),
                        },
                    )
                    .await
                }
                "transfer" => plamenu::groups::transfer_owner(&state, &community, &successor).await,
                _ => plamenu::groups::delete_group(&state, &community).await,
            }
        };
        let before = snapshot(&pool).await;
        fail_deliveries(&pool).await;
        let error = perform().await.unwrap_err();
        assert!(
            format!("{error:?}").contains("injected outbox failure"),
            "{operation}: {error:?}"
        );
        assert_eq!(snapshot(&pool).await, before, "{operation}");
        recover(&pool).await;
        perform().await.unwrap();
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn collection_membership_rolls_back_and_removal_reaches_removed_member(pool: PgPool) {
    use plamenu::collections::{self, CollectionParams};
    let (state, alice, _, _) = fixture(&pool).await;
    let bob = plamenu_db::account::find_by_uri(&pool, "https://remote.example/users/bob")
        .await
        .unwrap()
        .unwrap();
    // The removed member is not also in the owner's follower audience.
    follow::delete(&pool, bob.id, alice.id).await.unwrap();
    let collection = collections::create_collection(
        &state,
        &alice,
        CollectionParams {
            name: "People".into(),
            description: String::new(),
            language: None,
            sensitive: false,
            discoverable: true,
            tag_name: None,
        },
        &[],
    )
    .await
    .unwrap();
    let before = snapshot(&pool).await;
    fail_deliveries(&pool).await;
    let error = collections::add_account(&state, &collection, &alice, &bob)
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    assert_eq!(snapshot(&pool).await, before);
    recover(&pool).await;
    let item = collections::add_account(&state, &collection, &alice, &bob)
        .await
        .unwrap();
    sqlx::query("DELETE FROM delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    fail_deliveries(&pool).await;
    let error = collections::delete_item(&state, &collection, &alice, &item)
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    assert_eq!(snapshot(&pool).await, before);
    recover(&pool).await;
    collections::delete_item(&state, &collection, &alice, &item)
        .await
        .unwrap();
    let inboxes: Vec<String> = sqlx::query_scalar(
        "SELECT inbox_url FROM delivery_jobs WHERE activity->>'type' = 'Remove'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(inboxes.contains(&bob.preferred_inbox().to_owned()));
}
