//! Named public libraries expose references without changing source ownership.
use crate::{AppState, entities, error::ApiError, music_collections};
use plamenu_ap::activity::PUBLIC;
use plamenu_db::{
    DbError,
    account::{self, Account},
    id, job, status,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[must_use]
pub fn uri(domain: &str, collection: i64) -> String {
    format!("https://{domain}/ap/music/libraries/{collection}")
}

#[must_use]
pub(crate) fn local_id(domain: &str, target: &str) -> Option<i64> {
    let id = target
        .strip_prefix(&format!("https://{domain}/ap/music/libraries/"))?
        .parse::<i64>()
        .ok()?;
    (id > 0 && target == uri(domain, id)).then_some(id)
}

pub(crate) async fn owner(
    state: &AppState,
    target: &str,
) -> Result<Option<(Account, String)>, ApiError> {
    let Some(id) = local_id(&state.config.domain, target) else {
        return Ok(None);
    };
    let row = match music_collections::get(state, id, None).await {
        Ok(row) if row.kind == "library" && row.visibility == "public" => row,
        Ok(_) | Err(ApiError::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(owner) = account::find_by_id(&state.pool, row.owner_id).await? else {
        return Ok(None);
    };
    if !owner.is_local()
        || owner.is_group()
        || !crate::instance_policy::public_account_visible(
            &state.pool,
            &state.config.domain,
            &owner,
        )
        .await?
    {
        return Ok(None);
    }
    Ok(Some((owner, row.name)))
}

/// Follow and withdrawal serialize on the same collection row. A lookup made
/// before acquiring this lock cannot authorize a newly private collection.
pub(crate) async fn followable_conn(
    conn: &mut sqlx::PgConnection,
    domain: &str,
    target: &str,
) -> Result<bool, ApiError> {
    let Some(id) = local_id(domain, target) else {
        return Ok(true);
    };
    let allowed: Option<i64> = sqlx::query_scalar("SELECT c.id FROM music_collections c JOIN accounts a ON a.id=c.owner_id WHERE c.id=$1 AND c.kind='library' AND c.visibility='public' AND a.suspended_at IS NULL AND a.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) FOR SHARE OF c")
        .bind(id).fetch_optional(conn).await.map_err(DbError::from)?;
    Ok(allowed.is_some())
}

fn index(
    domain: &str,
    row: &music_collections::Collection,
    owner: &Account,
    count: usize,
) -> Value {
    let uri = uri(domain, row.id);
    let last = count.div_ceil(50).max(1);
    json!({"@context":["https://www.w3.org/ns/activitystreams","https://funkwhale.audio/ns"],"id":uri,"type":"Library","name":row.name,"summary":row.description,"actor":entities::account_uri(domain,owner),"attributedTo":entities::account_uri(domain,owner),"followers":format!("{uri}/followers"),"audience":PUBLIC,"to":[PUBLIC],"totalItems":count,"first":format!("{uri}?page=1"),"current":format!("{uri}?page=1"),"last":format!("{uri}?page={last}")})
}

enum PublicItem {
    Status(Box<status::Status>),
    Remote(String),
}
/// Retained originals require explicit publication; imported music stays a URI.
async fn public_items(state: &AppState, collection: i64) -> Result<Vec<PublicItem>, ApiError> {
    let entries = music_collections::entries(state, collection).await?;
    let visible = music_collections::playables(state, collection, None)
        .await?
        .into_iter()
        .filter_map(|item| item.entry_id.parse::<i64>().ok())
        .collect::<HashSet<_>>();
    let assets = entries
        .iter()
        .filter_map(|entry| entry.asset_id)
        .collect::<Vec<_>>();
    let publications: Vec<(i64, i64)> =
        sqlx::query_as("SELECT asset_id,status_id FROM music_publications WHERE asset_id=ANY($1)")
            .bind(&assets)
            .fetch_all(&state.pool)
            .await
            .map_err(DbError::from)?;
    let publications = publications.into_iter().collect::<HashMap<_, _>>();
    let status_id = |entry: &music_collections::Entry| {
        entry
            .status_id
            .or_else(|| entry.asset_id.and_then(|id| publications.get(&id).copied()))
    };
    let ids = entries
        .iter()
        .filter(|entry| visible.contains(&entry.id))
        .filter_map(status_id)
        .collect::<Vec<_>>();
    let mut statuses = HashMap::new();
    for page in ids.chunks(100) {
        let rows = status::find_by_ids(&state.pool, page).await?;
        let deleted = status::deleted_ids(&state.pool, page).await?;
        for row in entities::filter_viewable(&state.pool, &rows, None).await? {
            if row.object_type.as_deref() == Some("Audio")
                && row.reblog_of_id.is_none()
                && !deleted.contains(&row.id)
            {
                statuses.insert(row.id, row);
            }
        }
    }
    let mut remotes = HashMap::new();
    let ids = entries
        .iter()
        .filter(|entry| visible.contains(&entry.id))
        .filter_map(|entry| entry.remote_id)
        .collect::<Vec<_>>();
    for page in ids.chunks(100) {
        let rows = crate::music_remote::for_ids_pool(&state.pool, page, None).await?;
        let uris = rows.iter().map(|row| row.uri.clone()).collect::<Vec<_>>();
        let media = crate::music_remote::media_for_uris_pool(&state.pool, &uris, None).await?;
        for row in rows {
            let selected = if row.audio_url.is_some() {
                Some(&row)
            } else {
                media.get(&row.uri)
            };
            if let Some(selected) = selected {
                remotes.insert(row.id, selected.uri.clone());
            }
        }
    }
    Ok(entries
        .iter()
        .filter(|entry| visible.contains(&entry.id))
        .filter_map(|entry| {
            if let Some(remote) = entry.remote_id {
                remotes.get(&remote).cloned().map(PublicItem::Remote)
            } else {
                status_id(entry)
                    .and_then(|id| statuses.get(&id).cloned())
                    .map(|row| PublicItem::Status(Box::new(row)))
            }
        })
        .collect())
}

async fn item_documents(state: &AppState, items: &[PublicItem]) -> Result<Vec<Value>, ApiError> {
    let rows = items
        .iter()
        .filter_map(|item| match item {
            PublicItem::Status(row) => Some(row.as_ref()),
            PublicItem::Remote(_) => None,
        })
        .collect::<Vec<_>>();
    let authors = account::find_by_ids(
        &state.pool,
        &rows.iter().map(|row| row.account_id).collect::<Vec<_>>(),
    )
    .await?
    .into_iter()
    .map(|author| (author.id, author))
    .collect::<HashMap<_, _>>();
    let batch = crate::note::NoteBatch::load(state, &rows).await?;
    items
        .iter()
        .map(|item| match item {
            PublicItem::Remote(uri) => Ok(json!(uri)),
            PublicItem::Status(row) => {
                let author = authors.get(&row.account_id).ok_or(ApiError::NotFound)?;
                if author.is_local() {
                    crate::note::note_in_batch(state, row, author, &batch)
                } else {
                    Ok(json!(row.uri))
                }
            }
        })
        .collect()
}

pub async fn document(
    state: &AppState,
    collection: i64,
    page: Option<i64>,
) -> Result<Value, ApiError> {
    let target = uri(&state.config.domain, collection);
    let (publisher, _) = owner(state, &target).await?.ok_or(ApiError::NotFound)?;
    let row = music_collections::get(state, collection, None).await?;
    let items = public_items(state, collection).await?;
    let mut doc = index(&state.config.domain, &row, &publisher, items.len());
    let mut connection = state.pool.acquire().await.map_err(DbError::from)?;
    let artwork = crate::music_artworks::raw_image_conn(
        &mut connection,
        crate::music_artworks::Target::Collections,
        collection,
    )
    .await?;
    drop(connection);
    doc["image"] = crate::music_artworks::image(&state.config.domain, &artwork);
    if let Some(page) = page {
        let last = items.len().div_ceil(50).max(1);
        let number = usize::try_from(page)
            .ok()
            .filter(|page| (1..=last).contains(page))
            .ok_or(ApiError::NotFound)?;
        let start = (number - 1) * 50;
        doc["id"] = json!(format!("{target}?page={page}"));
        doc["type"] = json!("CollectionPage");
        doc["partOf"] = json!(target);
        doc["items"] = json!(
            item_documents(
                state,
                &items[start.min(items.len())..(start + 50).min(items.len())]
            )
            .await?
        );
        if number < last {
            doc["next"] = json!(format!("{target}?page={}", page + 1));
        }
        if page > 1 {
            doc["prev"] = json!(format!("{target}?page={}", page - 1));
        }
    }
    owner(state, &target).await?.ok_or(ApiError::NotFound)?;
    Ok(doc)
}

/// Metadata/membership mutations and their index notification commit together.
/// Withdrawal never sends the newly private name, description, or item URLs.
pub(crate) async fn changed_conn(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    collection: &music_collections::Collection,
    withdraw: bool,
) -> Result<(), ApiError> {
    if collection.kind != "library" || collection.visibility != "public" {
        return Ok(());
    }
    let owner = account::find_by_id(&mut *conn, collection.owner_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let target = uri(&state.config.domain, collection.id);
    let inboxes = crate::music_follows::target_inboxes(conn, &target, owner.id).await?;
    let object = if withdraw {
        json!({"id":target,"type":"Tombstone"})
    } else {
        // Item counts are computed on dereference with current source ACLs.
        let mut doc = index(&state.config.domain, collection, &owner, 0);
        let artwork = crate::music_artworks::raw_image_conn(
            conn,
            crate::music_artworks::Target::Collections,
            collection.id,
        )
        .await?;
        doc["image"] = crate::music_artworks::image(&state.config.domain, &artwork);
        let fields = doc.as_object_mut().expect("library document");
        fields.remove("totalItems");
        fields.remove("last");
        doc
    };
    let activity = json!({"@context":["https://www.w3.org/ns/activitystreams","https://funkwhale.audio/ns"],"id":format!("{target}/activities/{}",id::next()),"type":if withdraw {"Delete"} else {"Update"},"actor":entities::account_uri(&state.config.domain,&owner),"to":[PUBLIC],"object":object});
    job::enqueue_many(&mut *conn, owner.id, &inboxes, &activity, false).await?;
    if withdraw {
        sqlx::query("DELETE FROM music_follows WHERE local_target AND target_uri=$1")
            .bind(&target)
            .execute(conn)
            .await
            .map_err(DbError::from)?;
    }
    Ok(())
}

/// A scoped subscriber receives an origin-authored Audio with the library it
/// followed. Cross-owner collection references never become forged Creates.
pub(crate) async fn deliver_audio_conn(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: &Account,
    stored: &status::Status,
    object: &Value,
    kind: &str,
    only_collection: Option<i64>,
) -> Result<(), ApiError> {
    if !owner.is_local()
        || stored.account_id != owner.id
        || stored.object_type.as_deref() != Some("Audio")
        || !matches!(stored.visibility.as_str(), "public" | "unlisted")
    {
        return Ok(());
    }
    let collections: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT c.id FROM music_collections c JOIN music_collection_entries e ON e.collection_id=c.id LEFT JOIN music_publications p ON p.asset_id=e.asset_id WHERE c.owner_id=$1 AND c.kind='library' AND c.visibility='public' AND (e.status_id=$2 OR p.status_id=$2) AND ($3::bigint IS NULL OR c.id=$3)")
        .bind(owner.id).bind(stored.id).bind(only_collection).fetch_all(&mut *conn).await.map_err(DbError::from)?;
    for collection in collections {
        let target = uri(&state.config.domain, collection);
        let inboxes = crate::music_follows::target_inboxes(conn, &target, owner.id).await?;
        if inboxes.is_empty() {
            continue;
        }
        let mut doc = if kind == "Delete" {
            json!({"id":object["id"],"type":"Tombstone"})
        } else {
            object.clone()
        };
        doc["library"] = json!(target);
        let activity = json!({"@context":["https://www.w3.org/ns/activitystreams","https://funkwhale.audio/ns"],"id":format!("{target}/activities/{}",id::next()),"type":kind,"actor":entities::account_uri(&state.config.domain,owner),"to":[PUBLIC],"object":doc});
        job::enqueue_many(&mut *conn, owner.id, &inboxes, &activity, false).await?;
    }
    Ok(())
}

pub(crate) async fn added_conn(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    collection: &music_collections::Collection,
    asset: Option<i64>,
    status: Option<i64>,
) -> Result<(), ApiError> {
    if collection.kind != "library" || collection.visibility != "public" {
        return Ok(());
    }
    let source = match status {
        Some(id) => Some(id),
        None => sqlx::query_scalar("SELECT status_id FROM music_publications WHERE asset_id=$1")
            .bind(asset)
            .fetch_optional(&mut *conn)
            .await
            .map_err(DbError::from)?,
    };
    let Some(id) = source else { return Ok(()) };
    let Some(stored) = status::find_by_id(&mut *conn, id).await? else {
        return Ok(());
    };
    if stored.account_id != collection.owner_id {
        return Ok(());
    }
    let owner = account::find_by_id(&mut *conn, collection.owner_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let batch = crate::note::NoteBatch::load_conn(state, conn, &[&stored]).await?;
    let object = crate::note::note_in_batch(state, &stored, &owner, &batch)?;
    deliver_audio_conn(
        state,
        conn,
        &owner,
        &stored,
        &object,
        "Create",
        Some(collection.id),
    )
    .await
}
