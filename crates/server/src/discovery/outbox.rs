//! A bounded collection sample, one canonical post per author per round.
use super::{profiles, transport};
use crate::AppState;
use plamenu_ap::activity::id_of;
use plamenu_db::discovery_store::{self as db, Entry, Lease};
use plamenu_federation::{DiscoveryFormat, FederationError};
use serde_json::Value;

pub(super) fn same_origin(uri: &str, actor: &str) -> bool {
    let (Ok(uri), Ok(actor)) = (url::Url::parse(uri), url::Url::parse(actor)) else {
        return false;
    };
    uri.as_str().len() <= 2048
        && matches!(uri.scheme(), "http" | "https")
        && uri.origin() == actor.origin()
        && uri.username().is_empty()
        && uri.password().is_none()
        && uri.fragment().is_none()
}

pub(super) fn advertised<'a>(document: &'a Value, actor: &str) -> Option<&'a str> {
    document
        .get("outbox")
        .and_then(id_of)
        .filter(|uri| same_origin(uri, actor) && *uri != actor)
}

pub(super) fn author_allows(actor: &Entry, settings: &db::Settings) -> bool {
    actor.kind == "profile"
        && actor.visibility == "public"
        && actor.verified_at <= time::OffsetDateTime::now_utc()
        && actor.valid_until > time::OffsetDateTime::now_utc()
        && (actor.state == "active"
            || (actor.state == "denied"
                && actor.invalidation_reason.as_deref() == Some("ineligible")))
        && (actor.indexable == "allow"
            || (actor.indexable == "missing" && settings.allow_missing_indexable))
}

fn invalid() -> FederationError {
    FederationError::InvalidActor("invalid discovery outbox".into())
}

fn collection(document: &Value, uri: &str, outbox: &str) -> Result<(), FederationError> {
    if !matches!(
        document.get("type").and_then(Value::as_str),
        Some("OrderedCollection" | "Collection" | "OrderedCollectionPage" | "CollectionPage")
    ) || document.get("id").is_some_and(|v| id_of(v) != Some(uri))
        || document
            .get("partOf")
            .is_some_and(|v| id_of(v) != Some(outbox))
    {
        return Err(invalid());
    }
    Ok(())
}

fn items(document: &Value) -> Option<&Vec<Value>> {
    document
        .get("orderedItems")
        .or_else(|| document.get("items"))
        .and_then(Value::as_array)
}

/// Boosts are not authorship evidence; embedded text is never trusted or stored.
pub fn hints(document: &Value, actor: &str) -> Vec<String> {
    let mut hints = Vec::new();
    for item in items(document).into_iter().flatten().take(32) {
        let object = match item.get("type").and_then(Value::as_str) {
            Some("Create") if item.get("actor").and_then(id_of) == Some(actor) => {
                item.get("object")
            }
            Some("Note" | "Article" | "Page" | "Question")
                if super::posts::author_uri(item) == Some(actor) =>
            {
                Some(item)
            }
            None if item.is_string() => Some(item),
            _ => None,
        };
        // Embedded replies are poor standalone recommendations. This hint only
        // avoids an unnecessary fetch; canonical verification still decides
        // whether the selected object may be indexed.
        if object.is_some_and(|object| {
            object
                .get("inReplyTo")
                .is_some_and(|reply| !reply.is_null())
        }) {
            continue;
        }
        if let Some(uri) = object
            .and_then(id_of)
            .filter(|uri| same_origin(uri, actor) && *uri != actor)
            && !hints.iter().any(|known| known == uri)
        {
            hints.push(uri.to_owned());
        }
    }
    hints
}

pub async fn fetch(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let db_error = |e: plamenu_db::DbError| FederationError::Admission(e.to_string());
    let actor = db::entry(&state.pool, &lease.target_uri)
        .await
        .map_err(db_error)?;
    let settings = db::settings(&state.pool).await.map_err(db_error)?;
    let Some(actor) =
        actor.filter(|a| Some(a.revision) == lease.entry_revision && author_allows(a, &settings))
    else {
        db::finish_job(&state.pool, lease).await.map_err(db_error)?;
        return Ok(());
    };
    if settings
        .disabled_mechanisms
        .iter()
        .any(|m| m == "public_post_indexing" || m == "metadata_cache")
    {
        return Err(FederationError::DiscoveryStale);
    }
    if profiles::access(state, &actor.canonical_uri)
        .await
        .map_err(db_error)?
        != super::policy::Access::Allowed
    {
        db::invalidate_entry(
            &state.pool,
            &actor.canonical_uri,
            db::Invalidation::Denied,
            "local_moderation",
        )
        .await
        .map_err(db_error)?;
        return Err(FederationError::DiscoveryStale);
    }
    let Some(uri) = actor
        .outbox_uri
        .as_deref()
        .filter(|uri| same_origin(uri, &actor.canonical_uri))
    else {
        db::finish_job(&state.pool, lease).await.map_err(db_error)?;
        return Ok(());
    };
    let continuation = db::outbox_continuation(&state.pool, &actor.canonical_uri)
        .await
        .map_err(db_error)?;
    let page_uri = continuation
        .as_deref()
        .filter(|next| same_origin(next, uri))
        .unwrap_or(uri);
    let root = transport::fetch(state, lease, page_uri, DiscoveryFormat::ActivityPub).await?;
    collection(&root.document, &root.final_url, uri)?;
    let document = if items(&root.document).is_some() {
        root.document
    } else if let Some(first) = root.document.get("first") {
        let first_uri = id_of(first)
            .filter(|first| same_origin(first, uri) && *first != uri)
            .ok_or_else(invalid)?;
        // Even an embedded first-page object supplies only its canonical URI.
        let page = transport::fetch(state, lease, first_uri, DiscoveryFormat::ActivityPub).await?;
        collection(&page.document, &page.final_url, uri)?;
        if items(&page.document).is_none() {
            return Err(invalid());
        }
        page.document
    } else if root.document.get("totalItems").and_then(Value::as_u64) == Some(0) {
        root.document
    } else {
        return Err(invalid());
    };
    let versions: Vec<_> = items(&document)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let object = if item.get("type").and_then(Value::as_str) == Some("Create") {
                item.get("object")?
            } else {
                item
            };
            Some((
                id_of(object)?.to_owned(),
                object.get("updated")?.as_str()?.to_owned(),
            ))
        })
        .take(32)
        .filter(|(uri, _)| same_origin(uri, &actor.canonical_uri))
        .collect();
    db::observe_versions(&state.pool, lease, &versions)
        .await
        .map_err(db_error)?;
    let next = document
        .get("next")
        .and_then(id_of)
        .filter(|next| same_origin(next, uri) && *next != page_uri);
    db::save_outbox_continuation(&state.pool, lease, next)
        .await
        .map_err(db_error)?;
    let mut candidates = hints(&document, &actor.canonical_uri);
    if !candidates.is_empty() {
        let offset = usize::try_from(actor.outbox_round).unwrap_or_default() % candidates.len();
        candidates.rotate_left(offset);
    }
    db::finish_outbox(&state.pool, lease, &candidates)
        .await
        .map_err(db_error)?;
    Ok(())
}
