//! Lemmy user/community namespace collisions: a single `acct:name@host` served
//! as BOTH a Person (`/u/name`) and a Group (`/c/name`) — two `ActivityPub`
//! actors with distinct ids under one handle. Verifies both coexist by URI and
//! that typed resolution (`@` → person, `!` → group) is order-independent and
//! refresh-isolated.

mod common;

use std::collections::HashSet;

use common::{RemoteUser, StubFederation, test_state_with};
use plamenu::remote::{resolve_remote_account, resolve_remote_accounts};
use plamenu_db::PgPool;
use plamenu_db::account::{self, ActorClass};

/// A Lemmy-style actor: id `https://{domain}/{path}/{name}`, acct
/// `{name}@{domain}`, of the given `kind`. Two of these with the same `name`
/// but different `path`/`kind` reproduce the collision.
fn lemmy_actor(domain: &str, name: &str, path: &str, kind: &str) -> RemoteUser {
    let mut user = RemoteUser::new(domain, name);
    let id = format!("https://{domain}/{path}/{name}");
    kind.clone_into(&mut user.actor.kind);
    user.actor.inbox = format!("{id}/inbox");
    user.actor.public_key.id = format!("{id}#main-key");
    user.actor.public_key.owner.clone_from(&id);
    user.actor.id = id;
    // `RemoteUser::new` already set acct = "{name}@{domain}", shared by both.
    user
}

fn person() -> RemoteUser {
    lemmy_actor("lemmy.example", "collision", "u", "Person")
}

fn group() -> RemoteUser {
    lemmy_actor("lemmy.example", "collision", "c", "Group")
}

const PERSON_URI: &str = "https://lemmy.example/u/collision";
const GROUP_URI: &str = "https://lemmy.example/c/collision";

#[sqlx::test(migrations = "../db/migrations")]
async fn person_and_group_collision_resolves_by_type(pool: PgPool) {
    let stub = StubFederation::with_users(&[&person(), &group()]);
    let state = test_state_with(pool.clone(), stub);
    let acct = "collision@lemmy.example".parse().unwrap();

    // `Any` discovers and stores BOTH actors as distinct rows.
    let both = resolve_remote_accounts(&state, &acct, ActorClass::Any)
        .await
        .unwrap();
    assert_eq!(both.len(), 2, "both a Person and a Group are stored");
    let uris: HashSet<_> = both.iter().filter_map(|a| a.uri.clone()).collect();
    assert!(uris.contains(PERSON_URI) && uris.contains(GROUP_URI));

    // `@collision@host` (PersonLike) → the Person. This is the path composer
    // mentions take, so `@name@host` always targets the Person, never the Group.
    let p = resolve_remote_account(&state, &acct, ActorClass::PersonLike)
        .await
        .unwrap()
        .expect("person resolves");
    assert_eq!(p.uri.as_deref(), Some(PERSON_URI));
    assert!(!p.is_group());

    // `!collision@host` (Group) → the Group.
    let g = resolve_remote_account(&state, &acct, ActorClass::Group)
        .await
        .unwrap()
        .expect("group resolves");
    assert_eq!(g.uri.as_deref(), Some(GROUP_URI));
    assert!(g.is_group());

    // They are genuinely different rows.
    assert_ne!(p.id, g.id);
}

/// The `WebFinger` link order must not decide which actor a typed lookup gets:
/// Lemmy puts Person first, but the type — not position — is authoritative.
#[sqlx::test(migrations = "../db/migrations")]
async fn resolution_is_independent_of_webfinger_order(pool: PgPool) {
    // Group registered FIRST, so it leads the candidate list.
    let stub = StubFederation::with_users(&[&group(), &person()]);
    let state = test_state_with(pool.clone(), stub);
    let acct = "collision@lemmy.example".parse().unwrap();

    let p = resolve_remote_account(&state, &acct, ActorClass::PersonLike)
        .await
        .unwrap()
        .expect("person still resolves when it is not first");
    assert_eq!(p.uri.as_deref(), Some(PERSON_URI));

    let g = resolve_remote_account(&state, &acct, ActorClass::Group)
        .await
        .unwrap()
        .expect("group still resolves");
    assert_eq!(g.uri.as_deref(), Some(GROUP_URI));
}

/// Refreshing one actor of a collided handle must never replace or delete the
/// other: after both are cached, forcing a re-webfinger of the Person leaves the
/// Group row intact.
#[sqlx::test(migrations = "../db/migrations")]
async fn refresh_of_one_actor_leaves_the_other(pool: PgPool) {
    let stub = StubFederation::with_users(&[&person(), &group()]);
    let state = test_state_with(pool.clone(), stub);
    let acct = "collision@lemmy.example".parse().unwrap();

    // Prime both.
    resolve_remote_accounts(&state, &acct, ActorClass::Any)
        .await
        .unwrap();
    let group_before = account::find_by_uri(&pool, GROUP_URI)
        .await
        .unwrap()
        .expect("group stored");

    // Make the Person look stale so the next PersonLike lookup re-webfingers.
    sqlx::query!(
        "UPDATE accounts SET last_webfingered_at = NULL WHERE uri = $1",
        PERSON_URI,
    )
    .execute(&pool)
    .await
    .unwrap();

    let refreshed_person = resolve_remote_account(&state, &acct, ActorClass::PersonLike)
        .await
        .unwrap()
        .expect("person refreshes");
    assert_eq!(refreshed_person.uri.as_deref(), Some(PERSON_URI));

    // The Group row is untouched: same id, still a Group, still present.
    let group_after = account::find_by_uri(&pool, GROUP_URI)
        .await
        .unwrap()
        .expect("group still stored after person refresh");
    assert_eq!(group_after.id, group_before.id);
    assert!(group_after.is_group());
}
