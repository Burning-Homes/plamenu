//! Reverse resolution for both generations of local `ActivityPub` actor IDs.

use plamenu_db::account::{self, Account};
use plamenu_db::{DbError, PgPool};
use std::collections::{HashMap, HashSet};

async fn if_public<'e, E: plamenu_db::PgExecutor<'e>>(
    pool: E,
    actor: Option<Account>,
) -> Result<Option<Account>, DbError> {
    let Some(actor) = actor else {
        return Ok(None);
    };
    if !account::is_publicly_available(pool, actor.id).await? {
        return Ok(None);
    }
    Ok(Some(actor))
}

/// Resolves an exact, published local actor URI without ever treating its
/// mutable handle as identity. Pending login-backed actors deliberately resolve
/// as absent. The username parser is retained only for legacy actor IDs and
/// pre-backfill fixtures; numeric actors are loaded by immutable account id and
/// checked against their persisted URI.
pub async fn find_actor(
    pool: &PgPool,
    domain: &str,
    uri: &str,
) -> Result<Option<Account>, DbError> {
    let mut conn = pool.acquire().await?;
    find_actor_conn(&mut conn, domain, uri).await
}

pub async fn find_actor_conn(
    conn: &mut plamenu_db::PgConnection,
    domain: &str,
    uri: &str,
) -> Result<Option<Account>, DbError> {
    // Portable actors use a gateway path rather than `/users` or
    // `/ap/accounts`, but the exact persisted URI is still their immutable
    // identity and the account participates in this instance's namespace.
    if let Some(stored) = account::find_by_uri(&mut *conn, uri)
        .await?
        .filter(|actor| actor.has_local_account_on(domain))
    {
        return Box::pin(if_public(&mut *conn, Some(stored))).await;
    }
    if let Some(account_id) = plamenu_ap::urls::parse_local_numeric_actor_url(domain, uri) {
        let actor = account::find_by_id(&mut *conn, account_id)
            .await?
            .filter(Account::is_local)
            .filter(|account| account.uri.as_deref() == Some(uri));
        return Box::pin(if_public(&mut *conn, actor)).await;
    }
    let Some(username) = plamenu_ap::urls::parse_local_user_url(domain, uri) else {
        return Ok(None);
    };
    // Compatibility for pre-backfill test/upgrade rows only. Once `uri` has
    // been persisted it is the identity; a later handle rename must continue
    // resolving the old `/users/:name` URI by that exact stored value above.
    let actor = account::find_local_by_username(&mut *conn, username)
        .await?
        .filter(|account| account.uri.is_none());
    Box::pin(if_public(&mut *conn, actor)).await
}

/// Set-based form of [`find_actor`], keyed by the exact input URI. This keeps
/// remote-controlled addressing arrays to a bounded four database queries
/// (the fourth applies the publication boundary as one set) while supporting
/// both immutable numeric actors and legacy actor IDs.
pub async fn find_actors(
    pool: &PgPool,
    domain: &str,
    uris: &[&str],
) -> Result<HashMap<String, Account>, DbError> {
    let mut resolved = HashMap::new();

    for actor in account::find_by_uris(pool, uris).await? {
        if actor.has_local_account_on(domain)
            && let Some(uri) = actor.uri.clone()
            && uris.contains(&uri.as_str())
        {
            resolved.insert(uri, actor);
        }
    }

    let numeric: Vec<(String, i64)> = uris
        .iter()
        .filter(|uri| !resolved.contains_key(**uri))
        .filter_map(|uri| {
            plamenu_ap::urls::parse_local_numeric_actor_url(domain, uri)
                .map(|id| ((*uri).to_owned(), id))
        })
        .collect();
    if !numeric.is_empty() {
        let ids: Vec<i64> = numeric.iter().map(|(_, id)| *id).collect();
        let by_id: HashMap<i64, Account> = account::find_by_ids(pool, &ids)
            .await?
            .into_iter()
            .filter(Account::is_local)
            .map(|actor| (actor.id, actor))
            .collect();
        for (uri, id) in numeric {
            if let Some(actor) = by_id.get(&id)
                && actor.uri.as_deref() == Some(uri.as_str())
            {
                resolved.insert(uri, actor.clone());
            }
        }
    }

    let legacy: Vec<(&str, &str)> = uris
        .iter()
        .filter(|uri| !resolved.contains_key(**uri))
        .filter_map(|uri| {
            plamenu_ap::urls::parse_local_user_url(domain, uri).map(|name| (*uri, name))
        })
        .collect();
    if !legacy.is_empty() {
        let names: Vec<&str> = legacy.iter().map(|(_, name)| *name).collect();
        let by_name: HashMap<String, Account> = account::find_local_by_usernames(pool, &names)
            .await?
            .into_iter()
            .filter(|actor| actor.uri.is_none())
            .map(|actor| (actor.username.to_lowercase(), actor))
            .collect();
        for (uri, name) in legacy {
            if let Some(actor) = by_name.get(&name.to_lowercase()) {
                resolved.insert(uri.to_owned(), actor.clone());
            }
        }
    }

    let ids: Vec<i64> = resolved.values().map(|actor| actor.id).collect();
    // Keep this fourth SQL future behind a pointer: `find_actors` sits inside
    // recursive federation ingestion paths whose debug futures are already
    // close to the test runner's deliberately small stack.
    let public: HashSet<i64> = Box::pin(account::publicly_available_ids(pool, &ids))
        .await?
        .into_iter()
        .collect();
    resolved.retain(|_, actor| public.contains(&actor.id));

    Ok(resolved)
}

#[must_use]
pub fn has_local_actor_shape(domain: &str, uri: &str) -> bool {
    plamenu_ap::urls::parse_local_user_url(domain, uri).is_some()
        || plamenu_ap::urls::parse_local_numeric_actor_url(domain, uri).is_some()
        || has_gateway_actor_shape(domain, uri)
}

fn has_gateway_actor_shape(domain: &str, uri: &str) -> bool {
    let prefix = format!("https://{domain}/.well-known/apgateway/");
    let Some((identity, actor_id)) = uri
        .strip_prefix(&prefix)
        .and_then(|path| path.split_once("/actors/"))
    else {
        return false;
    };
    identity.starts_with("did:key:")
        && !identity.contains('/')
        && !actor_id.is_empty()
        && !actor_id.contains(['/', '?', '#'])
}

#[cfg(test)]
mod tests {
    use super::has_local_actor_shape;

    #[test]
    fn gateway_actor_shape_does_not_match_owned_objects_or_collections() {
        let actor = "https://example.test/.well-known/apgateway/did:key:z6MkExample/actors/abc";
        assert!(has_local_actor_shape("example.test", actor));
        assert!(!has_local_actor_shape(
            "example.test",
            &format!("{actor}/objects/note")
        ));
        assert!(!has_local_actor_shape(
            "example.test",
            &format!("{actor}/inbox")
        ));
        assert!(!has_local_actor_shape("other.test", actor));
    }
}
