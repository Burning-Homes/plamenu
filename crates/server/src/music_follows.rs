//! Music resource subscriptions have their own authority and delivery edges.
//! Following a library or track never creates a social follow.
use crate::{AppState, entities, error::ApiError};
use plamenu_ap::activity::{PUBLIC, id_of};
use plamenu_db::{
    account::{self, Account},
    id, job, status,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Subscription {
    #[serde(serialize_with = "serialize_id")]
    pub follower_id: i64,
    #[serde(serialize_with = "serialize_id")]
    pub owner_id: i64,
    pub target_uri: String,
    pub target_kind: String,
    pub target_name: String,
    pub local_target: bool,
    pub follow_uri: String,
    pub pending: bool,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "Serde's serialize_with callback receives a reference"
)]
fn serialize_id<S: serde::Serializer>(id: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_string())
}

struct Resource {
    owner: Account,
    kind: String,
    name: String,
}

fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(message.into())
}

#[must_use]
pub fn library_uri(domain: &str, owner: &Account) -> String {
    format!("{}/library", entities::account_uri(domain, owner))
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| *c != '\0').take(200).collect()
}

fn valid_url(uri: &str) -> bool {
    uri.len() <= 2048
        && !uri.chars().any(char::is_control)
        && plamenu_federation::is_federation_url(uri)
        && url::Url::parse(uri).is_ok_and(|u| {
            u.username().is_empty() && u.password().is_none() && u.fragment().is_none()
        })
}

pub(crate) async fn public_resource_owner(
    state: &AppState,
    target: &str,
) -> Result<Option<i64>, ApiError> {
    Ok(local_resource(state, target)
        .await?
        .map(|resource| resource.owner.id))
}

async fn local_resource(state: &AppState, uri: &str) -> Result<Option<Resource>, ApiError> {
    if uri.len() > 2048 || !uri.starts_with(&format!("https://{}/", state.config.domain)) {
        return Ok(None);
    }
    if let Some((owner, kind, name)) = crate::music_entities::resource_owner(state, uri).await? {
        return Ok(Some(Resource {
            owner,
            kind: kind.ap_type().into(),
            name,
        }));
    }
    if let Some((owner, name)) = crate::music_library_federation::owner(state, uri).await? {
        return Ok(Some(Resource {
            owner,
            kind: "Library".into(),
            name,
        }));
    }
    if let Some(actor_uri) = uri.strip_suffix("/library") {
        if let Some(owner) =
            crate::local_identity::find_actor(&state.pool, &state.config.domain, actor_uri).await?
            && owner.is_local()
            && !owner.is_group()
            && crate::instance_policy::account_id_visible(&state.pool, owner.id).await?
        {
            let name = clean(if owner.display_name.is_empty() {
                &owner.username
            } else {
                &owner.display_name
            });
            return Ok(Some(Resource {
                owner,
                kind: "Library".into(),
                name,
            }));
        }
        return Ok(None);
    }
    let Some((actor_uri, tail)) = uri.rsplit_once("/statuses/") else {
        return Ok(None);
    };
    let (number, entity) = tail.split_once('/').unwrap_or((tail, ""));
    let Ok(number) = number.parse::<i64>() else {
        return Ok(None);
    };
    let Some(owner) =
        crate::local_identity::find_actor(&state.pool, &state.config.domain, actor_uri).await?
    else {
        return Ok(None);
    };
    let Some(stored) = status::find_by_id(&state.pool, number).await? else {
        return Ok(None);
    };
    if !owner.is_local()
        || owner.id != stored.account_id
        || status::is_deleted(&state.pool, stored.id).await?
        || stored.object_type.as_deref() != Some("Audio")
        || !matches!(stored.visibility.as_str(), "public" | "unlisted")
        || !crate::instance_policy::account_id_visible(&state.pool, owner.id).await?
    {
        return Ok(None);
    }
    let root = entities::status_uri_for_account(&state.config.domain, &stored, &owner);
    let expected = if entity.is_empty() {
        root
    } else {
        format!("{root}/{entity}")
    };
    if uri != expected {
        return Ok(None);
    }
    let kind = match entity {
        "" => "Audio",
        "track" => "Track",
        "artist" => "Artist",
        "album" => "Album",
        _ => return Ok(None),
    };
    if matches!(entity, "artist" | "album")
        && shared_alias(state, number, &owner, entity, uri).await?
    {
        return Ok(None);
    }
    let metadata = plamenu_db::music::for_statuses(&state.pool, &[number])
        .await?
        .remove(&number)
        .unwrap_or_default();
    let name = match entity {
        "artist" => metadata.artist,
        "album" => metadata.album,
        _ => stored.title.unwrap_or_default(),
    };
    Ok(Some(Resource {
        owner,
        kind: kind.into(),
        name: clean(&name),
    }))
}

async fn shared_alias(
    state: &AppState,
    number: i64,
    owner: &Account,
    entity: &str,
    uri: &str,
) -> Result<bool, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let snapshots = crate::music_entities::snapshots_conn(&mut conn, &[number]).await?;
    if let Some(snapshot) = snapshots.get(&number) {
        let mut object = serde_json::json!({"attributedTo":entities::account_uri(&state.config.domain,owner),"track":{"album":{"id":uri}}});
        snapshot.enrich(&state.config.domain, &mut object);
        let canonical = if entity == "artist" {
            object["track"]["artists"][0]["id"].as_str()
        } else {
            object["track"]["album"]["id"].as_str()
        };
        if canonical.is_some_and(|canonical| canonical != uri) {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn permitted(
    state: &AppState,
    owner: &Account,
    follower: &Account,
) -> Result<bool, ApiError> {
    if owner.suspended()
        || follower.suspended()
        || account::effectively_silenced(&state.pool, follower.id).await?
        || plamenu_db::block::exists(&state.pool, owner.id, follower.id).await?
        || plamenu_db::block::exists(&state.pool, follower.id, owner.id).await?
        || plamenu_db::account_domain_block::blocks_account_domain(
            &state.pool,
            owner.id,
            follower.id,
        )
        .await?
        || plamenu_db::account_domain_block::blocks_account_domain(
            &state.pool,
            follower.id,
            owner.id,
        )
        .await?
    {
        return Ok(false);
    }
    Ok(crate::instance_policy::can_federate_url(
        &state.pool,
        &state.config.domain,
        &entities::account_uri(&state.config.domain, owner),
    )
    .await?
        && crate::instance_policy::can_federate_url(
            &state.pool,
            &state.config.domain,
            &entities::account_uri(&state.config.domain, follower),
        )
        .await?)
}

fn follow_document(state: &AppState, actor: &Account, target: &str, follow_uri: &str) -> Value {
    json!({"@context":"https://www.w3.org/ns/activitystreams","id":follow_uri,"type":"Follow","actor":entities::account_uri(&state.config.domain,actor),"object":target})
}

async fn insert(
    conn: &mut plamenu_db::PgConnection,
    follower: i64,
    uri: &str,
    r: &Resource,
    follow_uri: &str,
    pending: bool,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO music_follows(follower_id,owner_id,target_uri,target_kind,target_name,local_target,follow_uri,pending) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(follower_id,target_uri) DO UPDATE SET follow_uri=EXCLUDED.follow_uri,pending=EXCLUDED.pending,target_name=EXCLUDED.target_name")
        .bind(follower).bind(r.owner.id).bind(uri).bind(&r.kind).bind(&r.name).bind(r.owner.is_local()).bind(follow_uri).bind(pending)
        .execute(conn).await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

pub(crate) async fn find_target(
    state: &AppState,
    follower: i64,
    target: &str,
) -> Result<Option<Subscription>, ApiError> {
    Ok(
        sqlx::query_as("SELECT * FROM music_follows WHERE follower_id=$1 AND target_uri=$2")
            .bind(follower)
            .bind(target)
            .fetch_optional(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?,
    )
}

pub async fn list(state: &AppState, follower: i64) -> Result<Vec<Subscription>, ApiError> {
    Ok(sqlx::query_as("SELECT * FROM music_follows WHERE follower_id=$1 ORDER BY created_at DESC,target_uri LIMIT 1000")
        .bind(follower).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?)
}

async fn resolve_resource(state: &AppState, target: &str) -> Result<Resource, ApiError> {
    let r = if target.starts_with(&format!("https://{}/", state.config.domain)) {
        local_resource(state, target)
            .await?
            .ok_or(ApiError::NotFound)?
    } else {
        if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, target)
            .await?
        {
            return Err(ApiError::NotFound);
        }
        let fetched = state
            .federation
            .fetch_activitypub_same_origin(target)
            .await
            .map_err(|_| invalid("Music resource could not be fetched safely"))?;
        if fetched.body_bytes > 2 * 1024 * 1024 {
            return Err(invalid("Music document is too large"));
        }
        let doc = fetched
            .document
            .ok_or_else(|| invalid("Music document is missing"))?;
        if doc["id"].as_str() != Some(target) {
            return Err(invalid("Music resource identity differs from its URL"));
        }
        let kind = doc["type"]
            .as_str()
            .filter(|k| matches!(*k, "Library" | "Audio" | "Track" | "Artist" | "Album"))
            .ok_or_else(|| invalid("This is not a music resource"))?;
        let owner_uri = doc
            .get("actor")
            .and_then(id_of)
            .or_else(|| plamenu_ap::activity::attributed_to_ids(&doc).next())
            .ok_or_else(|| invalid("Music resource has no owner"))?;
        let origin = url::Url::parse(target).map_err(|_| invalid("Invalid music URL"))?;
        let controller =
            url::Url::parse(owner_uri).map_err(|_| invalid("Invalid music owner URL"))?;
        if origin.origin() != controller.origin()
            || !valid_url(owner_uri)
            || !crate::instance_policy::can_federate_url(
                &state.pool,
                &state.config.domain,
                owner_uri,
            )
            .await?
        {
            return Err(invalid("Music owner must be on the publishing origin"));
        }
        let actor = state
            .federation
            .fetch_actor(owner_uri)
            .await
            .map_err(|_| invalid("Music owner is unavailable"))?;
        if actor.id != owner_uri
            || !matches!(
                actor.kind.as_str(),
                "Person" | "Service" | "Application" | "Group"
            )
        {
            return Err(invalid("Music owner identity is invalid"));
        }
        let owner = crate::remote::refresh_remote_actor(state, &actor).await?;
        Resource {
            owner,
            kind: kind.into(),
            name: clean(doc["name"].as_str().unwrap_or(target)),
        }
    };
    Ok(r)
}

/// Resolve a music document through the guarded AP client and verify its owner.
/// Remote music identities are retained, never converted into local actors.
pub async fn follow(state: &AppState, follower: &Account, target: &str) -> Result<(), ApiError> {
    let target = target.trim();
    if !valid_url(target) {
        return Err(invalid("Enter a valid music resource URL"));
    }
    let r = resolve_resource(state, target).await?;
    if !permitted(state, &r.owner, follower).await? {
        return Err(ApiError::NotFound);
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    // Serialize the account's subscription limit and concurrent repeat follows.
    sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR NO KEY UPDATE")
        .bind(follower.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    crate::music_queues::active_owner(&mut tx, follower.id).await?;
    if r.owner.is_local()
        && (!controller_active_conn(&mut tx, r.owner.id).await?
            || !crate::music_library_federation::followable_conn(
                &mut tx,
                &state.config.domain,
                target,
            )
            .await?
            || !crate::music_entities::followable_conn(&mut tx, &state.config.domain, target)
                .await?)
    {
        return Err(ApiError::NotFound);
    }
    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM music_follows WHERE follower_id=$1 AND target_uri=$2)",
    )
    .bind(follower.id)
    .bind(target)
    .fetch_one(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    if existing {
        return Ok(());
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_follows WHERE follower_id=$1")
        .bind(follower.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    if count >= 1000 {
        return Err(invalid(
            "You have reached the limit of 1000 music subscriptions",
        ));
    }
    let follow_uri = format!(
        "{}/music-follows/{}",
        entities::account_uri(&state.config.domain, follower),
        id::next()
    );
    insert(
        &mut tx,
        follower.id,
        target,
        &r,
        &follow_uri,
        !r.owner.is_local(),
    )
    .await?;
    if !r.owner.is_local() {
        job::enqueue(
            &mut *tx,
            follower.id,
            &r.owner.inbox_url,
            &follow_document(state, follower, target, &follow_uri),
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

pub async fn unfollow(state: &AppState, follower: &Account, target: &str) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let row: Option<Subscription> = sqlx::query_as(
        "DELETE FROM music_follows WHERE follower_id=$1 AND target_uri=$2 RETURNING *",
    )
    .bind(follower.id)
    .bind(target)
    .fetch_optional(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    if let Some(row) = row
        && !row.local_target
    {
        let owner = account::find_by_id(&mut *tx, row.owner_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let undo = json!({"@context":"https://www.w3.org/ns/activitystreams","id":format!("{}/undo/{}",row.follow_uri,id::next()),"type":"Undo","actor":entities::account_uri(&state.config.domain,follower),"object":follow_document(state,follower,&row.target_uri,&row.follow_uri)});
        job::enqueue(&mut *tx, follower.id, &owner.inbox_url, &undo).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Called only after ordinary inbox signature/actor verification.
pub async fn incoming(state: &AppState, sender: &Account, raw: &Value) -> Result<bool, ApiError> {
    let kind = raw["type"].as_str().unwrap_or_default();
    if matches!(kind, "Follow" | "Accept" | "Reject" | "Undo")
        && raw.get("actor").and_then(id_of)
            != Some(entities::account_uri(&state.config.domain, sender).as_str())
    {
        return Err(invalid(
            "Music activity actor differs from its verified sender",
        ));
    }
    match kind {
        "Follow" => incoming_follow(state, sender, raw).await,
        "Accept" | "Reject" => incoming_decision(state, sender, raw).await,
        "Undo" if raw["object"]["type"] == "Follow" => incoming_undo(state, sender, raw).await,
        _ => Ok(false),
    }
}

async fn incoming_follow(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(target) = raw.get("object").and_then(id_of) else {
        return Ok(false);
    };
    let Some(r) = local_resource(state, target).await? else {
        return Ok(false);
    };
    let follow_uri = raw["id"]
        .as_str()
        .filter(|s| s.len() <= 2048 && plamenu_federation::is_federation_url(s))
        .ok_or_else(|| invalid("Music Follow needs a valid id"))?;
    let allowed = permitted(state, &r.owner, sender).await?;
    let response = json!({"@context":"https://www.w3.org/ns/activitystreams","id":format!("{}/music-follow-responses/{}",entities::account_uri(&state.config.domain,&r.owner),id::next()),"type":if allowed{"Accept"}else{"Reject"},"actor":entities::account_uri(&state.config.domain,&r.owner),"to":[entities::account_uri(&state.config.domain,sender)],"object":raw});
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let allowed = allowed
        && controller_active_conn(&mut tx, r.owner.id).await?
        && crate::music_library_federation::followable_conn(&mut tx, &state.config.domain, target)
            .await?
        && crate::music_entities::followable_conn(&mut tx, &state.config.domain, target).await?;
    let mut response = response;
    response["type"] = json!(if allowed { "Accept" } else { "Reject" });
    if allowed {
        insert(&mut tx, sender.id, target, &r, follow_uri, false).await?;
    } else {
        sqlx::query(
            "DELETE FROM music_follows WHERE follower_id=$1 AND target_uri=$2 AND local_target",
        )
        .bind(sender.id)
        .bind(target)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    }
    job::enqueue_relationship_response(&mut *tx, r.owner.id, &sender.inbox_url, &response).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(true)
}

/// Acquire the controller FK lock before any resource row. Retirement holds
/// the stronger account lock until its final audience and purge commit, so a
/// stale resolution cannot insert a subscription after that purge.
async fn controller_active_conn(
    conn: &mut plamenu_db::PgConnection,
    owner: i64,
) -> Result<bool, ApiError> {
    let found:Option<i64>=sqlx::query_scalar("SELECT a.id FROM accounts a WHERE a.id=$1 AND a.suspended_at IS NULL AND a.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) FOR KEY SHARE OF a")
        .bind(owner).fetch_optional(conn).await.map_err(plamenu_db::DbError::from)?;
    Ok(found.is_some())
}

async fn incoming_decision(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let kind = raw["type"].as_str().unwrap_or_default();

    let Some(follow_uri) = raw.get("object").and_then(id_of) else {
        return Ok(false);
    };
    if !follow_uri.starts_with(&format!("https://{}/", state.config.domain))
        || !follow_uri.contains("/music-follows/")
    {
        return Ok(false);
    }
    let row: Option<Subscription> =
        sqlx::query_as("SELECT * FROM music_follows WHERE follow_uri=$1 AND NOT local_target")
            .bind(follow_uri)
            .fetch_optional(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?;
    let Some(row) = row else { return Ok(false) };
    if row.owner_id != sender.id {
        return Ok(true);
    }
    let local = account::find_by_id(&state.pool, row.follower_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if raw["object"].is_object()
        && (raw["object"]["type"] != "Follow"
            || raw["object"].get("actor").and_then(id_of)
                != Some(entities::account_uri(&state.config.domain, &local).as_str())
            || raw["object"].get("object").and_then(id_of) != Some(row.target_uri.as_str()))
    {
        return Ok(true);
    }
    if kind == "Accept" && permitted(state, sender, &local).await? {
        sqlx::query("UPDATE music_follows SET pending=false WHERE follow_uri=$1 AND owner_id=$2")
            .bind(follow_uri)
            .bind(sender.id)
            .execute(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?;
    } else {
        sqlx::query("DELETE FROM music_follows WHERE follow_uri=$1 AND owner_id=$2")
            .bind(follow_uri)
            .bind(sender.id)
            .execute(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?;
    }
    Ok(true)
}

async fn incoming_undo(state: &AppState, sender: &Account, raw: &Value) -> Result<bool, ApiError> {
    let Some(target) = raw["object"].get("object").and_then(id_of) else {
        return Ok(false);
    };
    if !target.starts_with(&format!("https://{}/", state.config.domain))
        || (!target.ends_with("/library")
            && !target.contains("/statuses/")
            && crate::music_library_federation::local_id(&state.config.domain, target).is_none()
            && crate::music_entities::local_id(&state.config.domain, target).is_none())
    {
        return Ok(false);
    }
    let Some(follow_uri) = raw["object"].get("id").and_then(Value::as_str) else {
        return Ok(false);
    };
    let row: Option<Subscription> = sqlx::query_as(
        "SELECT * FROM music_follows WHERE follower_id=$1 AND target_uri=$2 AND local_target",
    )
    .bind(sender.id)
    .bind(target)
    .fetch_optional(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?;
    let Some(_) = row else { return Ok(false) };
    if raw["object"].get("actor").and_then(id_of)
        == Some(entities::account_uri(&state.config.domain, sender).as_str())
    {
        sqlx::query("DELETE FROM music_follows WHERE follower_id=$1 AND target_uri=$2 AND follow_uri=$3 AND local_target").bind(sender.id).bind(target).bind(follow_uri).execute(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    }
    Ok(true)
}

/// Batch public Audio subscribers, applying current blocks and moderation.
/// Kept out of non-Audio publication paths.
pub(crate) async fn inboxes(
    conn: &mut plamenu_db::PgConnection,
    domain: &str,
    stored: &status::Status,
    owner: &Account,
) -> Result<Vec<String>, ApiError> {
    if stored.object_type.as_deref() != Some("Audio")
        || !matches!(stored.visibility.as_str(), "public" | "unlisted")
    {
        return Ok(Vec::new());
    }
    let root = entities::status_uri_for_account(domain, stored, owner);
    let mut targets = vec![
        root.clone(),
        format!("{root}/track"),
        format!("{root}/artist"),
        format!("{root}/album"),
    ];
    if stored.visibility == "public" {
        targets.push(library_uri(domain, owner));
    }
    Ok(resource_inboxes(conn, &targets, owner.id)
        .await?
        .into_iter()
        .map(|r| r.inbox)
        .collect())
}

#[derive(sqlx::FromRow)]
struct Recipient {
    target_uri: String,
    inbox: String,
}
async fn resource_inboxes(
    conn: &mut plamenu_db::PgConnection,
    targets: &[String],
    owner: i64,
) -> Result<Vec<Recipient>, ApiError> {
    recipients(conn, Some(targets), owner, false).await
}

async fn recipients(
    conn: &mut plamenu_db::PgConnection,
    targets: Option<&[String]>,
    owner: i64,
    actor_lifecycle: bool,
) -> Result<Vec<Recipient>, ApiError> {
    Ok(sqlx::query_as("SELECT DISTINCT f.target_uri, CASE WHEN a.shared_inbox_url<>'' THEN a.shared_inbox_url ELSE a.inbox_url END AS inbox FROM music_follows f JOIN accounts a ON a.id=f.follower_id JOIN accounts o ON o.id=f.owner_id WHERE ($1::text[] IS NULL OR f.target_uri=ANY($1)) AND f.owner_id=$2 AND f.local_target AND NOT f.pending AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.deleted_at IS NULL AND a.silenced_at IS NULL AND ($3 OR o.suspended_at IS NULL) AND o.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=f.owner_id AND b.target_account_id=f.follower_id) OR (b.account_id=f.follower_id AND b.target_account_id=f.owner_id)) AND NOT EXISTS(SELECT 1 FROM account_domain_blocks b WHERE (b.account_id=f.owner_id AND b.domain=a.domain) OR (b.account_id=f.follower_id AND b.domain=o.domain)) AND instance_domain_allowed(a.domain) AND NOT EXISTS(SELECT 1 FROM domain_blocks b WHERE b.domain=a.domain AND b.severity='silence')")
        .bind(targets).bind(owner).bind(actor_lifecycle).fetch_all(conn).await.map_err(plamenu_db::DbError::from)?)
}

/// Resource subscribers still need the controller's profile/key updates and
/// final actor retraction. A temporary owner suspension permits its blanked
/// actor Update and subsequent Delete, while recipient moderation still applies.
/// This audience is never used for unrelated social posts.
pub(crate) async fn actor_inboxes(
    conn: &mut plamenu_db::PgConnection,
    owner: i64,
) -> Result<Vec<String>, ApiError> {
    Ok(recipients(conn, None, owner, true)
        .await?
        .into_iter()
        .map(|r| r.inbox)
        .collect())
}

/// Retract cached music before the actor's own Delete and before erasing the
/// subscription edges. Some music peers retain Audio caches after Delete(Actor).
/// Only this controller's sources are retracted; foreign collection entries
/// keep their original controller and are never included.
pub(crate) async fn retire_owner_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    owner: &Account,
) -> Result<(), ApiError> {
    let mut subscriptions: HashMap<String, HashSet<String>> = HashMap::new();
    for recipient in recipients(conn, None, owner.id, true).await? {
        if !recipient.inbox.is_empty() {
            subscriptions
                .entry(recipient.target_uri)
                .or_default()
                .insert(recipient.inbox);
        }
    }
    let social = plamenu_db::follow::follower_inboxes(&mut *conn, owner.id).await?;
    if subscriptions.is_empty() && social.is_empty() {
        return Ok(());
    }
    let mut before = None::<i64>;
    loop {
        let ids:Vec<i64>=sqlx::query_scalar("SELECT id FROM statuses WHERE account_id=$1 AND object_type='Audio' AND reblog_of_id IS NULL AND visibility IN ('public','unlisted') AND deleted_at IS NULL AND ($2::bigint IS NULL OR id<$2) ORDER BY id DESC LIMIT 50\n-- STUBFILTER: account retirement retracts only live origin-authored Audio")
            .bind(owner.id).bind(before).fetch_all(&mut *conn).await.map_err(plamenu_db::DbError::from)?;
        if ids.is_empty() {
            break;
        }
        let related:Vec<(i64,i64,Option<i64>,Option<i64>)>=sqlx::query_as("SELECT p.status_id,t.artist_id,t.album_id,a.artist_id FROM music_publications p JOIN music_assets t ON t.id=p.asset_id LEFT JOIN music_albums a ON a.id=t.album_id WHERE p.status_id=ANY($1) AND t.owner_id=$2")
            .bind(&ids).bind(owner.id).fetch_all(&mut *conn).await.map_err(plamenu_db::DbError::from)?;
        let related = related
            .into_iter()
            .map(|(status, artist, album, album_artist)| (status, (artist, album, album_artist)))
            .collect::<HashMap<_, _>>();
        let libraries:Vec<(i64,i64)>=sqlx::query_as("SELECT DISTINCT COALESCE(e.status_id,p.status_id),c.id FROM music_collections c JOIN music_collection_entries e ON e.collection_id=c.id LEFT JOIN music_publications p ON p.asset_id=e.asset_id WHERE c.owner_id=$1 AND c.kind='library' AND c.visibility='public' AND COALESCE(e.status_id,p.status_id)=ANY($2)")
            .bind(owner.id).bind(&ids).fetch_all(&mut *conn).await.map_err(plamenu_db::DbError::from)?;
        for stored in status::find_by_ids(&mut *conn, &ids).await? {
            let root = entities::status_uri_for_account(&state.config.domain, &stored, owner);
            let mut documents = vec![root.clone(), format!("{root}/track")];
            if let Some((artist, album, album_artist)) = related.get(&stored.id) {
                documents.push(crate::music_entities::uri(
                    &state.config.domain,
                    crate::music_entities::Kind::Artist,
                    *artist,
                ));
                if let Some(album) = album {
                    documents.push(crate::music_entities::uri(
                        &state.config.domain,
                        crate::music_entities::Kind::Album,
                        *album,
                    ));
                }
                if let Some(artist) = album_artist {
                    documents.push(crate::music_entities::uri(
                        &state.config.domain,
                        crate::music_entities::Kind::Artist,
                        *artist,
                    ));
                }
            } else {
                documents.extend([format!("{root}/artist"), format!("{root}/album")]);
            }
            let mut targets = documents.clone();
            if stored.visibility == "public" {
                targets.push(library_uri(&state.config.domain, owner));
            }
            targets.extend(
                libraries
                    .iter()
                    .filter(|(status, _)| *status == stored.id)
                    .map(|(_, library)| {
                        crate::music_library_federation::uri(&state.config.domain, *library)
                    }),
            );
            let mut inboxes = social.iter().cloned().collect::<HashSet<_>>();
            for target in targets {
                if let Some(subscribed) = subscriptions.get(&target) {
                    inboxes.extend(subscribed.iter().cloned());
                }
            }
            let inboxes = inboxes.into_iter().collect::<Vec<_>>();
            documents.sort_unstable();
            documents.dedup();
            for target in documents {
                retire_document_conn(state, conn, owner, &target, &inboxes).await?;
            }
        }
        before = ids.last().copied();
    }
    for (target, inboxes) in subscriptions {
        retire_document_conn(
            state,
            conn,
            owner,
            &target,
            &inboxes.into_iter().collect::<Vec<_>>(),
        )
        .await?;
    }
    Ok(())
}

async fn retire_document_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    owner: &Account,
    target: &str,
    inboxes: &[String],
) -> Result<(), ApiError> {
    if inboxes.is_empty() {
        return Ok(());
    }
    let activity = json!({"@context":["https://www.w3.org/ns/activitystreams","https://funkwhale.audio/ns"],"id":format!("{target}/activities/{}",id::next()),"type":"Delete","actor":entities::account_uri(&state.config.domain,owner),"to":[PUBLIC],"object":{"id":target,"type":"Tombstone"}});
    job::enqueue_many(conn, owner.id, inboxes, &activity, false).await?;
    Ok(())
}

pub(crate) async fn target_inboxes(
    conn: &mut plamenu_db::PgConnection,
    target: &str,
    owner: i64,
) -> Result<Vec<String>, ApiError> {
    Ok(resource_inboxes(conn, &[target.to_owned()], owner)
        .await?
        .into_iter()
        .map(|r| r.inbox)
        .collect())
}

/// A resource's own Update/Delete reaches its subscribers under its own id.
/// Artist, Album and Track identities are metadata resources, never actors.
pub(crate) async fn deliver_entities(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    owner: &Account,
    object: &Value,
    kind: &str,
) -> Result<(), ApiError> {
    let docs = [
        object["track"].clone(),
        object["track"]["artists"][0].clone(),
        object["track"]["album"].clone(),
    ];
    let targets = docs
        .iter()
        .filter_map(|doc| doc["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return Ok(());
    }
    let recipients = resource_inboxes(conn, &targets, owner.id).await?;
    for mut doc in docs {
        let Some(uri) = doc["id"].as_str().map(str::to_owned) else {
            continue;
        };
        // Shared entities outlive a single Audio. Their own metadata/source
        // lifecycle emits notifications independently of the parent post.
        if crate::music_entities::local_id(&state.config.domain, &uri).is_some() {
            continue;
        }
        let inboxes = recipients
            .iter()
            .filter(|r| r.target_uri == uri)
            .map(|r| r.inbox.clone())
            .collect::<Vec<_>>();
        if inboxes.is_empty() {
            continue;
        }
        if kind == "Delete" {
            doc = json!({"id":uri,"type":"Tombstone"});
        } else {
            doc["@context"] = object["@context"].clone();
            doc["to"] = json!([PUBLIC]);
            doc["updated"] = object["updated"].clone();
        }
        let activity = json!({"@context":object["@context"],"id":format!("{uri}/activities/{}",id::next()),"type":kind,"actor":entities::account_uri(&state.config.domain,owner),"to":[PUBLIC],"object":doc});
        job::enqueue_many(&mut *conn, owner.id, &inboxes, &activity, false).await?;
    }
    Ok(())
}

pub async fn library(
    state: &AppState,
    owner: &Account,
    page: Option<i64>,
) -> Result<Value, ApiError> {
    let uri = library_uri(&state.config.domain, owner);
    if !owner.is_local()
        || owner.is_group()
        || !crate::instance_policy::account_id_visible(&state.pool, owner.id).await?
    {
        return Err(ApiError::NotFound);
    }
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id=$1 AND object_type='Audio' AND visibility='public' AND reblog_of_id IS NULL AND deleted_at IS NULL\n-- STUBFILTER: library omits deleted publications")
        .bind(owner.id).fetch_one(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let last = ((count + 49) / 50).max(1);
    let mut doc = json!({"@context":["https://www.w3.org/ns/activitystreams","https://funkwhale.audio/ns"],"id":uri,"type":"Library","name":if owner.display_name.is_empty(){&owner.username}else{&owner.display_name},"actor":entities::account_uri(&state.config.domain,owner),"attributedTo":entities::account_uri(&state.config.domain,owner),"followers":format!("{uri}/followers"),"audience":PUBLIC,"totalItems":count,"first":format!("{uri}?page=1"),"current":format!("{uri}?page=1"),"last":format!("{uri}?page={last}"),"to":[PUBLIC]});
    if let Some(page) = page {
        if !(1..=last).contains(&page) {
            return Err(ApiError::NotFound);
        }
        let ids:Vec<i64>=sqlx::query_scalar("SELECT id FROM statuses WHERE account_id=$1 AND object_type='Audio' AND visibility='public' AND reblog_of_id IS NULL AND deleted_at IS NULL ORDER BY id DESC LIMIT 50 OFFSET $2\n-- STUBFILTER: library omits deleted publications")
            .bind(owner.id).bind((page-1)*50).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
        let mut rows = status::find_by_ids(&state.pool, &ids).await?;
        rows.sort_unstable_by_key(|s| std::cmp::Reverse(s.id));
        let refs = rows.iter().collect::<Vec<_>>();
        let batch = crate::note::NoteBatch::load(state, &refs).await?;
        let items = rows
            .iter()
            .map(|s| crate::note::note_in_batch(state, s, owner, &batch))
            .collect::<Result<Vec<_>, _>>()?;
        doc["id"] = json!(format!("{uri}?page={page}"));
        doc["type"] = json!("CollectionPage");
        doc["partOf"] = json!(uri);
        doc["items"] = json!(items);
        if page < last {
            doc["next"] = json!(format!("{uri}?page={}", page + 1));
        }
        if page > 1 {
            doc["prev"] = json!(format!("{uri}?page={}", page - 1));
        }
    }
    Ok(doc)
}
