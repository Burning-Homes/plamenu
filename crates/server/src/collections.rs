//! Account collections (Mastodon 4.6 / FEP-7aa9): the REST entity serializers,
//! the featureability policy, the local services that mutate a collection and
//! distribute the change over `ActivityPub`, and the inbound handlers that
//! process remote collections and the feature-request consent handshake.
//!
//! The consent flow mirrors the FEP-044f quote handshake already in
//! [`crate::note`]/`crate::actions`: a local collection featuring a *remote*
//! account sends a `FeatureRequest` and waits for an `Accept` carrying a
//! `FeatureAuthorization` stamp; a remote collection featuring a *local*
//! account triggers the inverse — we mint and serve the stamp ourselves.

use std::collections::HashMap;
use std::fmt;

use plamenu_ap::activity::id_of;
use plamenu_ap::featured::{
    self, FeatureResponseParams, FeaturedCollectionParams, FeaturedItemParams,
};
use plamenu_ap::urls::{self, LocalUserUrls};
use plamenu_db::account::{self, Account};
use plamenu_db::collection::{
    self, ChangeCollection, Collection, CollectionItem, NewCollection, NewCollectionItem,
};
use plamenu_db::{PgPool, block, id, job, notification, tag};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::AppState;
use crate::entities::{account_json, account_uri, feature_policy_for_account, rfc3339};
use crate::error::ApiError;
use crate::remote::{host_of, refresh_remote_actor};

// ----------------------------------------------------------------------------
// Validation
// ----------------------------------------------------------------------------

/// The mutable attributes a create/update request carries (Mastodon's
/// permitted params, with `tag_name` already resolved to a `tag_id`).
#[derive(Debug, Default)]
pub struct CollectionParams {
    pub name: String,
    pub description: String,
    pub language: Option<String>,
    pub sensitive: bool,
    pub discoverable: bool,
    pub tag_name: Option<String>,
}

/// A rejected collection mutation, kept as a typed variant rather than a
/// sentence so both surfaces can render it their own way: the REST API prints
/// [`Display`] (the `Validation failed: …` wording Mastodon clients already
/// parse) while the web forms map the variant onto a translated catalog
/// message. Same shape as [`crate::oauth_app::Invalid`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Invalid {
    NameBlank,
    NameTooLong,
    DescriptionTooLong,
    CollectionLimit,
    ItemLimit,
    NotFeatureable,
    AlreadyAMember,
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameBlank => f.write_str("Name can't be blank"),
            Self::NameTooLong => write!(
                f,
                "Name is too long (maximum is {} characters)",
                collection::NAME_LENGTH_LIMIT
            ),
            Self::DescriptionTooLong => write!(
                f,
                "Description is too long (maximum is {} characters)",
                collection::DESCRIPTION_LENGTH_LIMIT
            ),
            Self::CollectionLimit => write!(
                f,
                "You have reached the limit of {} collections",
                collection::PER_ACCOUNT_LIMIT
            ),
            Self::ItemLimit => write!(
                f,
                "Collection items can have at most {} accounts",
                collection::MAX_ITEMS
            ),
            Self::NotFeatureable => f.write_str("Account can't be added to collections"),
            Self::AlreadyAMember => f.write_str("Account has already been taken"),
        }
    }
}

/// How a collection mutation can fail: a typed rejection the caller may want to
/// phrase itself, or an ordinary error raised while carrying the change out.
#[derive(Debug)]
pub enum Failure {
    Invalid(Vec<Invalid>),
    Api(ApiError),
}

impl From<Invalid> for Failure {
    fn from(invalid: Invalid) -> Self {
        Self::Invalid(vec![invalid])
    }
}

impl From<ApiError> for Failure {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl From<plamenu_db::DbError> for Failure {
    fn from(err: plamenu_db::DbError) -> Self {
        Self::Api(err.into())
    }
}

impl From<Failure> for ApiError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Invalid(errors) => Self::Unprocessable(format!(
                "Validation failed: {}",
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Failure::Api(err) => err,
        }
    }
}

fn validate(name: &str, description: &str) -> Result<(), Failure> {
    let mut errors = Vec::new();
    if name.trim().is_empty() {
        errors.push(Invalid::NameBlank);
    } else if name.chars().count() > collection::NAME_LENGTH_LIMIT {
        errors.push(Invalid::NameTooLong);
    }
    if description.chars().count() > collection::DESCRIPTION_LENGTH_LIMIT {
        errors.push(Invalid::DescriptionTooLong);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Failure::Invalid(errors))
    }
}

/// Resolves the `tag_name` param to a tag id (creating the tag), `None` when
/// absent or blank — Mastodon's `tag_name=` setter.
async fn resolve_tag(state: &AppState, tag_name: Option<&str>) -> Result<Option<i64>, ApiError> {
    match tag_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => Ok(Some(
            tag::ensure(&state.pool, name.trim_start_matches('#')).await?,
        )),
        None => Ok(None),
    }
}

// ----------------------------------------------------------------------------
// REST entity serializers
// ----------------------------------------------------------------------------

/// The AP id of a collection (remote: stored; local: derived from the layout).
fn collection_uri(domain: &str, collection: &Collection, owner: &Account) -> String {
    match &collection.uri {
        Some(uri) => uri.clone(),
        None => urls::collection_uri_for_actor(&account_uri(domain, owner), collection.id),
    }
}

/// The human web URL of a collection (remote: stored `url`; local: derived).
fn collection_web_url(domain: &str, collection: &Collection, owner: &Account) -> String {
    match (&collection.url, collection.local) {
        (Some(url), _) => url.clone(),
        (None, _) => urls::collection_web_url(domain, &owner.username, collection.id),
    }
}

/// The id of a local `FeaturedItem` (a member of a local collection).
fn featured_item_uri(collection_uri: &str, item_id: i64) -> String {
    format!("{collection_uri}/items/{item_id}")
}

/// The Mastodon `ShallowTag` (`{name, url}`) for a collection's topic tag.
pub(crate) async fn shallow_tag_json(
    pool: &PgPool,
    domain: &str,
    tag_id: i64,
) -> Result<Value, ApiError> {
    let Some(name) = tag::name_of(pool, tag_id).await? else {
        return Ok(Value::Null);
    };
    let lower = name.to_lowercase();
    Ok(json!({
        "name": lower,
        "url": format!("https://{domain}/tags/{lower}"),
    }))
}

/// The Mastodon `CollectionItem` entity. `account_id` only shows for pending
/// or accepted memberships.
pub fn collection_item_entity(item: &CollectionItem) -> Result<Value, ApiError> {
    let mut entity = json!({
        "id": item.id.to_string(),
        "state": item.state,
        "created_at": rfc3339(item.created_at)?,
    });
    if matches!(item.state.as_str(), "pending" | "accepted")
        && let Some(account_id) = item.account_id
    {
        entity["account_id"] = json!(account_id.to_string());
    }
    Ok(entity)
}

/// The Mastodon `Collection` entity, rendered for `viewer` (which decides
/// which items are visible).
pub async fn collection_json(
    pool: &PgPool,
    domain: &str,
    collection: &Collection,
    viewer: Option<i64>,
) -> Result<Value, ApiError> {
    let owner = account::find_by_id(pool, collection.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let tag = match collection.tag_id {
        Some(tag_id) => shallow_tag_json(pool, domain, tag_id).await?,
        None => Value::Null,
    };
    let items = collection::items_for(pool, collection.id, collection.account_id, viewer).await?;
    collection_json_preloaded(domain, collection, &owner, &tag, &items)
}

/// [`collection_json`] with the owner, tag and member items already resolved,
/// so a batch renderer can fetch owners and tags once per page and every
/// referenced collection's items in one grouped query
/// (`collection::items_for_many`) instead of a round trip per collection (QC
/// audit #4).
pub fn collection_json_preloaded(
    domain: &str,
    collection: &Collection,
    owner: &Account,
    tag: &Value,
    items: &[plamenu_db::collection::CollectionItem],
) -> Result<Value, ApiError> {
    let item_count = items.len();
    let items_json: Vec<Value> = items
        .iter()
        .map(collection_item_entity)
        .collect::<Result<_, _>>()?;
    Ok(json!({
        "id": collection.id.to_string(),
        "uri": collection_uri(domain, collection, owner),
        "name": collection.name,
        "description": collection.description,
        "language": collection.language,
        "account_id": collection.account_id.to_string(),
        "local": collection.local,
        "sensitive": collection.sensitive,
        "discoverable": collection.discoverable,
        "url": collection_web_url(domain, collection, owner),
        "item_count": item_count,
        "created_at": rfc3339(collection.created_at)?,
        "updated_at": rfc3339(collection.updated_at)?,
        "tag": tag,
        "items": items_json,
    }))
}

/// [`collection_json`] for a set of collection ids, keyed by id — the batched
/// form the notification renderers use: one `collection::find_by_ids`, one
/// owner `account::find_by_ids`, one grouped `collection::items_for_many` and
/// one `tag::names_of` regardless of how many collections the page
/// references. Unknown ids are simply absent; a collection whose owner row is
/// gone is a data-integrity error, like [`collection_json`]'s `NotFound`.
pub async fn collections_json_map(
    pool: &PgPool,
    domain: &str,
    collection_ids: &[i64],
    viewer: Option<i64>,
) -> Result<HashMap<i64, Value>, ApiError> {
    if collection_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let collections = collection::find_by_ids(pool, collection_ids).await?;
    let mut owner_ids: Vec<i64> = collections.iter().map(|c| c.account_id).collect();
    owner_ids.sort_unstable();
    owner_ids.dedup();
    let owners: HashMap<i64, Account> = account::find_by_ids(pool, &owner_ids)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let tag_ids: Vec<i64> = collections.iter().filter_map(|c| c.tag_id).collect();
    let tag_names = tag::names_of(pool, &tag_ids).await?;
    let found_ids: Vec<i64> = collections.iter().map(|c| c.id).collect();
    let mut items = collection::items_for_many(pool, &found_ids, viewer).await?;
    let mut rendered = HashMap::with_capacity(collections.len());
    for collection in &collections {
        let owner = owners
            .get(&collection.account_id)
            .ok_or(ApiError::NotFound)?;
        let tag = match collection.tag_id.and_then(|id| tag_names.get(&id)) {
            Some(name) => {
                let lower = name.to_lowercase();
                json!({
                    "name": lower,
                    "url": format!("https://{domain}/tags/{lower}"),
                })
            }
            None => Value::Null,
        };
        let items = items.remove(&collection.id).unwrap_or_default();
        rendered.insert(
            collection.id,
            collection_json_preloaded(domain, collection, owner, &tag, &items)?,
        );
    }
    Ok(rendered)
}

/// The Mastodon `CollectionWithAccounts` entity (the `show` response): the
/// collection plus the owner and every visible member account.
pub async fn collection_with_accounts_json(
    pool: &PgPool,
    domain: &str,
    collection: &Collection,
    viewer: Option<i64>,
) -> Result<Value, ApiError> {
    let owner = account::find_by_id(pool, collection.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let items = collection::items_for(pool, collection.id, collection.account_id, viewer).await?;
    let member_ids: Vec<i64> = items.iter().filter_map(|item| item.account_id).collect();
    let mut accounts = vec![account_json(pool, domain, &owner, viewer).await?];
    accounts
        .extend(crate::entities::render_accounts_by_ids(pool, domain, &member_ids, viewer).await?);
    Ok(json!({
        "collection": collection_json(pool, domain, collection, viewer).await?,
        "accounts": accounts,
    }))
}

// ----------------------------------------------------------------------------
// Policy
// ----------------------------------------------------------------------------

/// Mastodon's `AccountPolicy#feature?`: `featurer` may feature `target` when
/// neither blocks the other and the target is featureable. A local target is
/// featureable when it is discoverable and either unlocked, followed by the
/// featurer, or the featurer itself; a remote target is featureable when its
/// advertised `interactionPolicy.canFeature` says this account may be auto- or
/// manually approved.
pub async fn featureable(
    state: &AppState,
    featurer: &Account,
    target: &Account,
) -> Result<bool, ApiError> {
    let mut target = target.clone();
    if !target.is_local()
        && target.feature_approval_policy == 0
        && let Some(uri) = target.uri.as_deref()
        && crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await?
        && let Ok(actor) = state.federation.fetch_actor(uri).await
    {
        target = refresh_remote_actor(state, &actor).await?;
    }

    if block::exists(&state.pool, featurer.id, target.id).await?
        || block::exists(&state.pool, target.id, featurer.id).await?
    {
        return Ok(false);
    }
    Ok(matches!(
        feature_policy_for_account(&state.pool, &target, Some(featurer.id)).await?,
        "automatic" | "manual"
    ))
}

// ----------------------------------------------------------------------------
// AP object building / distribution
// ----------------------------------------------------------------------------

/// Builds the `FeaturedItem` for an accepted membership, resolving its stamp
/// URL (a local member's stamp we serve, a remote member's granted one). The
/// member row comes prefetched from the caller's batch.
fn featured_item_value(
    state: &AppState,
    collection_uri: &str,
    item: &CollectionItem,
    member: &Account,
) -> Result<Value, ApiError> {
    let item_uri = item
        .uri
        .clone()
        .unwrap_or_else(|| featured_item_uri(collection_uri, item.id));
    let member_uri = account_uri(&state.config.domain, member);
    let authorization = if member.is_local() {
        Some(urls::feature_authorization_uri_for_actor(
            &account_uri(&state.config.domain, member),
            item.id,
        ))
    } else {
        item.approval_uri.clone()
    };
    let published = rfc3339(item.created_at)?;
    Ok(featured::featured_item(&FeaturedItemParams {
        id: &item_uri,
        featured_object: &member_uri,
        feature_authorization: authorization.as_deref(),
        published: &published,
    }))
}

/// Builds the bare `FeaturedCollection` object Values for a set of **local**
/// collections, keyed by collection id, in a fixed number of queries: one
/// owner batch, one grouped accepted-items query, one member-account batch
/// and one topic-tag batch, however many collections (and members) the set
/// holds. Collections whose owner row is gone are absent. The per-collection
/// forms below all assemble from this.
pub async fn featured_collection_values_for(
    state: &AppState,
    collections: &[Collection],
) -> Result<HashMap<i64, Value>, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    featured_collection_values_for_conn(state, &mut conn, collections).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn featured_collection_values_for_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    collections: &[Collection],
) -> Result<HashMap<i64, Value>, ApiError> {
    if collections.is_empty() {
        return Ok(HashMap::new());
    }
    let mut owner_ids: Vec<i64> = collections.iter().map(|c| c.account_id).collect();
    owner_ids.sort_unstable();
    owner_ids.dedup();
    let owners: HashMap<i64, Account> = account::find_by_ids(&mut *conn, &owner_ids)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let collection_ids: Vec<i64> = collections.iter().map(|c| c.id).collect();
    // Accepted members only, in display order, as each owner sees them.
    let mut items_by_collection =
        collection::accepted_items_for_owners(&mut *conn, &collection_ids).await?;
    let mut member_ids: Vec<i64> = items_by_collection
        .values()
        .flatten()
        .filter_map(|item| item.account_id)
        .collect();
    member_ids.sort_unstable();
    member_ids.dedup();
    let members: HashMap<i64, Account> = account::find_by_ids(&mut *conn, &member_ids)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let tag_ids: Vec<i64> = collections.iter().filter_map(|c| c.tag_id).collect();
    let tag_names = tag::names_of(&mut *conn, &tag_ids).await?;

    let mut rendered = HashMap::with_capacity(collections.len());
    for collection in collections {
        let Some(owner) = owners.get(&collection.account_id) else {
            continue;
        };
        let collection_uri = collection_uri(&state.config.domain, collection, owner);
        let url = collection_web_url(&state.config.domain, collection, owner);
        let owner_uri = account_uri(&state.config.domain, owner);
        let items = items_by_collection
            .remove(&collection.id)
            .unwrap_or_default();
        let mut item_values = Vec::new();
        for item in &items {
            // A membership row whose account has vanished is skipped, like
            // the per-item lookup this replaced.
            if let Some(member) = item.account_id.and_then(|id| members.get(&id)) {
                item_values.push(featured_item_value(state, &collection_uri, item, member)?);
            }
        }
        let total_items = item_values.len() as u64;
        let topic = collection
            .tag_id
            .and_then(|tag_id| tag_names.get(&tag_id))
            .map(|name| featured_topic_value(&state.config.domain, name));
        let published = rfc3339(collection.created_at)?;
        let updated = rfc3339(collection.updated_at)?;
        rendered.insert(
            collection.id,
            featured::featured_collection_object(&FeaturedCollectionParams {
                id: &collection_uri,
                attributed_to: &owner_uri,
                url: &url,
                name: &collection.name,
                summary: &collection.description,
                language: collection.language.as_deref(),
                sensitive: collection.sensitive,
                discoverable: collection.discoverable,
                published: &published,
                updated: &updated,
                total_items,
                topic,
                items: item_values,
            }),
        );
    }
    Ok(rendered)
}

/// The bare `FeaturedCollection` object Value for one local collection — the
/// single-collection form of [`featured_collection_values_for`], for the
/// create/update services that distribute the object they just wrote.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn featured_collection_value_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    collection: &Collection,
) -> Result<Value, ApiError> {
    featured_collection_values_for_conn(state, conn, std::slice::from_ref(collection))
        .await?
        .remove(&collection.id)
        .ok_or(ApiError::NotFound)
}

/// Serves the standalone `FeaturedCollection` document (with `@context`) for a
/// local collection — `GET /users/{username}/collections/{id}`.
pub async fn featured_collection_document(
    state: &AppState,
    collection: &Collection,
) -> Result<Value, ApiError> {
    let mut object = featured_collection_values_for(state, std::slice::from_ref(collection))
        .await?
        .remove(&collection.id)
        .ok_or(ApiError::NotFound)?;
    object["@context"] = featured::featured_context();
    Ok(object)
}

/// Collections referenced by the links in a local status, reconciled against
/// the recorded set — Mastodon's `ProcessLinksService`. Called on post and on
/// edit; only `FeaturedCollection` links count (matched against known local
/// and remote collections; unknown remote URLs are not fetched). The status'
/// rendered HTML is the link source (Plamenu stores no raw text), so mention
/// and hashtag anchors are naturally ignored — they never resolve to a
/// collection.
pub async fn scan_and_link_collections(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    status: &plamenu_db::status::Status,
) -> Result<(), ApiError> {
    // Resolve each distinct link to a collection, keyed by id, remembering the
    // URL it matched under (stored on the `tagged_objects` row). Collection
    // definitions are committed data — resolved on the pool; only the
    // `tagged_objects` reconciliation writes/reads on `conn`, so the set commits
    // atomically with the status and its outgoing `tag` array reads it back
    // mid-transaction.
    let mut resolved: Vec<(i64, String)> = Vec::new();
    for url in extract_href_urls(&status.content) {
        if resolved.iter().any(|(_, u)| u == url) {
            continue;
        }
        if let Some(collection) = resolve_linked_collection_conn(state, conn, url).await?
            && !resolved.iter().any(|(id, _)| *id == collection.id)
        {
            resolved.push((collection.id, url.to_owned()));
        }
    }
    let previous = plamenu_db::tagged_object::for_status(&mut *conn, status.id).await?;
    for (collection_id, url) in &resolved {
        plamenu_db::tagged_object::add(&mut *conn, status.id, *collection_id, url).await?;
    }
    // Drop references the edit removed from the text.
    let gone: Vec<i64> = previous
        .iter()
        .filter(|prev| !resolved.iter().any(|(id, _)| *id == prev.collection_id))
        .map(|prev| prev.id)
        .collect();
    plamenu_db::tagged_object::remove(&mut *conn, &gone).await?;
    Ok(())
}

/// The `href` targets of `<a>` anchors in rendered status HTML.
fn extract_href_urls(html: &str) -> Vec<&str> {
    let mut urls = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("href=\"") {
        rest = &rest[pos + 6..];
        if let Some(end) = rest.find('"') {
            let url = &rest[..end];
            if url.starts_with("https://") || url.starts_with("http://") {
                urls.push(url);
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    urls
}

/// Resolves a link to a known collection: a local one (by its AP id or web
/// URL, with the owner username verified), or an already-known remote one (by
/// its stored `uri`/`url`). `None` when the link is not a known collection.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn resolve_linked_collection_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    url: &str,
) -> Result<Option<Collection>, ApiError> {
    let domain = &state.config.domain;
    let local_ref = if let Some((account_id, collection_id)) =
        urls::parse_local_numeric_collection_url(domain, url)
    {
        Some((
            format!("https://{domain}/ap/accounts/{account_id}"),
            collection_id,
        ))
    } else {
        urls::parse_local_collection_url(domain, url).map(|(username, collection_id)| {
            (format!("https://{domain}/users/{username}"), collection_id)
        })
    };
    if let Some((actor_uri, collection_id)) = local_ref {
        let Some(collection) = collection::find(&mut *conn, collection_id).await? else {
            return Ok(None);
        };
        let owner = crate::local_identity::find_actor_conn(&mut *conn, domain, &actor_uri).await?;
        let matches = owner.is_some_and(|o| o.id == collection.account_id);
        return Ok(matches.then_some(collection));
    }
    if let Some((username, collection_id)) = urls::parse_local_collection_web_url(domain, url) {
        let Some(collection) = collection::find(&mut *conn, collection_id).await? else {
            return Ok(None);
        };
        let owner = account::find_local_by_username(&mut *conn, username).await?;
        return Ok(owner
            .is_some_and(|o| o.id == collection.account_id)
            .then_some(collection));
    }
    if let Some(collection) = collection::find_by_uri(&mut *conn, url).await? {
        return Ok(Some(collection));
    }
    Ok(collection::find_by_url(&mut *conn, url).await?)
}

/// The `FeaturedCollection` `tag` entries for a local status' referenced
/// collections (Mastodon's `NoteSerializer` folding `tagged_objects` into
/// `tag`). A local collection embeds its full object; a remote one is a bare
/// `{type, id, name}` reference the receiver re-fetches by `id`.
pub async fn note_tagged_collection_tags<'e, E: plamenu_db::PgExecutor<'e>>(
    state: &AppState,
    executor: E,
    status_id: i64,
) -> Result<Vec<Value>, ApiError> {
    // `executor` reads the (possibly just-written) `tagged_objects` set; the
    // per-collection owner/definition lookups below are committed data on the
    // pool.
    let by_status =
        plamenu_db::tagged_object::collections_for_statuses(executor, &[status_id]).await?;
    let Some(collections) = by_status.get(&status_id) else {
        return Ok(Vec::new());
    };
    let mut tags = Vec::with_capacity(collections.len());
    for collection in collections {
        if let Some(value) = note_collection_tag(state, collection).await? {
            tags.push(value);
        }
    }
    Ok(tags)
}

/// [`note_tagged_collection_tags`] for a whole page of statuses, keyed by
/// status id: one `collections_for_statuses` for the page, and each distinct
/// collection materialized **once** however many rows on the page reference it
/// — the same collection tagged onto twenty statuses used to re-fetch its
/// owner and its whole item list twenty times.
pub async fn note_tagged_collection_tags_for(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    status_ids: &[i64],
) -> Result<HashMap<i64, Vec<Value>>, ApiError> {
    let by_status =
        plamenu_db::tagged_object::collections_for_statuses(&mut *conn, status_ids).await?;
    // Materialize each distinct collection once for the page: local ones as
    // full objects via one batched build, remote ones as bare references with
    // one batched owner lookup.
    let mut distinct: Vec<&Collection> = Vec::new();
    for collection in by_status.values().flatten() {
        if !distinct.iter().any(|c| c.id == collection.id) {
            distinct.push(collection);
        }
    }
    let locals: Vec<Collection> = distinct
        .iter()
        .filter(|c| c.local)
        .map(|c| (*c).clone())
        .collect();
    let mut built = featured_collection_values_for_conn(state, conn, &locals).await?;
    let mut remote_owner_ids: Vec<i64> = distinct
        .iter()
        .filter(|c| !c.local)
        .map(|c| c.account_id)
        .collect();
    remote_owner_ids.sort_unstable();
    remote_owner_ids.dedup();
    let remote_owners: HashMap<i64, Account> = account::find_by_ids(&mut *conn, &remote_owner_ids)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    for collection in &distinct {
        // A remote collection whose owner row has gone renders nothing.
        if !collection.local
            && let Some(owner) = remote_owners.get(&collection.account_id)
        {
            built.insert(
                collection.id,
                remote_collection_tag(state, collection, owner),
            );
        }
    }
    let mut tags_by_status: HashMap<i64, Vec<Value>> = HashMap::with_capacity(by_status.len());
    for (status_id, collections) in by_status {
        let tags = collections
            .iter()
            .filter_map(|collection| built.get(&collection.id).cloned())
            .collect();
        tags_by_status.insert(status_id, tags);
    }
    Ok(tags_by_status)
}

/// One collection's `tag` entry: a local collection embeds its full object, a
/// remote one a bare `{type, id, name}` reference the receiver re-fetches by
/// `id`. `None` for a remote collection whose owner row has gone.
async fn note_collection_tag(
    state: &AppState,
    collection: &Collection,
) -> Result<Option<Value>, ApiError> {
    if collection.local {
        return Ok(
            featured_collection_values_for(state, std::slice::from_ref(collection))
                .await?
                .remove(&collection.id),
        );
    }
    let Some(owner) = account::find_by_id(&state.pool, collection.account_id).await? else {
        return Ok(None);
    };
    Ok(Some(remote_collection_tag(state, collection, &owner)))
}

/// The bare `{type, id, name, attributedTo}` reference a remote collection's
/// `tag` entry carries.
fn remote_collection_tag(state: &AppState, collection: &Collection, owner: &Account) -> Value {
    let uri = collection_uri(&state.config.domain, collection, owner);
    json!({
        "type": "FeaturedCollection",
        "id": uri,
        "name": collection.name,
        "attributedTo": account_uri(&state.config.domain, owner),
    })
}

/// Serves the `FeatureAuthorization` stamp for a local accepted membership —
/// `GET /users/{username}/feature_authorizations/{id}`.
pub async fn feature_authorization_document(
    state: &AppState,
    member: &Account,
    item: &CollectionItem,
) -> Result<Value, ApiError> {
    let collection = collection::find(&state.pool, item.collection_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let owner = account::find_by_id(&state.pool, collection.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let collection_uri = collection_uri(&state.config.domain, &collection, &owner);
    let stamp_uri = urls::feature_authorization_uri_for_actor(
        &account_uri(&state.config.domain, member),
        item.id,
    );
    let member_uri = account_uri(&state.config.domain, member);
    Ok(featured::feature_authorization(
        &stamp_uri,
        &member_uri,
        &collection_uri,
    ))
}

/// The `topic` Hashtag tag object for a collection's tag name.
fn featured_topic_value(domain: &str, name: &str) -> Value {
    let lower = name.to_lowercase();
    json!({
        "type": "Hashtag",
        "href": format!("https://{domain}/tags/{lower}"),
        "name": format!("#{name}"),
    })
}

/// The remote inboxes a collection change must reach: the owner's followers
/// (handled by [`crate::actions::fan_out`]) plus every resolved remote member's inbox
/// (pending members included — they hold a `FeatureRequest` to answer). The
/// member rows come in one batch instead of one lookup per membership.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn member_inboxes_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    collection_id: i64,
    owner_id: i64,
) -> Result<Vec<String>, ApiError> {
    let items = collection::items_for(&mut *conn, collection_id, owner_id, Some(owner_id)).await?;
    let member_ids: Vec<i64> = items.iter().filter_map(|item| item.account_id).collect();
    let members: HashMap<i64, Account> = account::find_by_ids(&mut *conn, &member_ids)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let mut inboxes = Vec::new();
    for item in &items {
        if let Some(member) = item.account_id.and_then(|id| members.get(&id))
            && !member.is_local()
        {
            let inbox = member.preferred_inbox().to_owned();
            if !inbox.is_empty() && !inboxes.contains(&inbox) {
                inboxes.push(inbox);
            }
        }
    }
    Ok(inboxes)
}

/// Distributes a collection-level activity to the owner's followers and the
/// collection's remote members.
async fn distribute_collection(
    state: &AppState,
    owner: &Account,
    collection_id: i64,
    activity: &Value,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    distribute_collection_conn(state, &mut conn, owner, collection_id, activity).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
async fn distribute_collection_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    owner: &Account,
    collection_id: i64,
    activity: &Value,
) -> Result<(), ApiError> {
    let extras = member_inboxes_conn(state, conn, collection_id, owner.id).await?;
    crate::actions::fan_out_conn(state, conn, owner, activity, &extras).await?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Local member helpers
// ----------------------------------------------------------------------------

/// Adds a member to a local collection, applying state, notification and
/// distribution: a local member is accepted at once (and notified); a remote
/// one is pending until it answers our `FeatureRequest`.
async fn notify_member_added(
    state: &AppState,
    collection: &Collection,
    owner: &Account,
    member: &Account,
) {
    if member.is_local()
        && let Err(error) = notification::create_for_collection(
            &state.pool,
            member.id,
            owner.id,
            "added_to_collection",
            collection.id,
        )
        .await
    {
        tracing::warn!(%error, "post-commit collection member notification failed");
    }
}

async fn add_member_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    collection: &Collection,
    owner: &Account,
    member: &Account,
) -> Result<CollectionItem, Failure> {
    let item_id = id::next();
    let activity_uri = (!member.is_local()).then(|| {
        urls::feature_request_uri_for_actor(&account_uri(&state.config.domain, owner), item_id)
    });
    let state_str = if member.is_local() {
        "accepted"
    } else {
        "pending"
    };
    let inserted = collection::add_item(
        &mut *conn,
        NewCollectionItem {
            item_id,
            collection_id: collection.id,
            account_id: Some(member.id),
            state: state_str,
            uri: None,
            object_uri: Some(&account_uri(&state.config.domain, member)),
            activity_uri: activity_uri.as_deref(),
            approval_uri: None,
        },
    )
    .await?;
    let Some(item) = inserted else {
        return Err(Invalid::AlreadyAMember.into());
    };
    if member.is_local() {
        // Announce the new item to the owner's followers + members.
        let owner_uri = account_uri(&state.config.domain, owner);
        let coll_uri = collection_uri(&state.config.domain, collection, owner);
        let value = featured_item_value(state, &coll_uri, &item, member)?;
        let activity = featured::add_featured_item(&owner_uri, &coll_uri, &value);
        distribute_collection_conn(state, conn, owner, collection.id, &activity).await?;
    } else {
        send_feature_request_conn(state, conn, owner, collection, member, &item).await?;
    }
    Ok(item)
}

/// Sends a `FeatureRequest` to a remote member's inbox.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn send_feature_request_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    owner: &Account,
    collection: &Collection,
    member: &Account,
    item: &CollectionItem,
) -> Result<(), ApiError> {
    let Some(activity_uri) = item.activity_uri.as_deref() else {
        return Ok(());
    };
    let member_uri = account_uri(&state.config.domain, member);
    let coll_uri = collection_uri(&state.config.domain, collection, owner);
    let request = featured::feature_request(activity_uri, &member_uri, &coll_uri);
    job::enqueue(&mut *conn, owner.id, member.preferred_inbox(), &request).await?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Services (local owner actions)
// ----------------------------------------------------------------------------

/// `CreateCollectionService`: validates, creates the collection, adds the
/// initial members, then distributes an `Add(FeaturedCollection)` and the
/// per-member feature requests.
pub async fn create_collection(
    state: &AppState,
    owner: &Account,
    params: CollectionParams,
    account_ids: &[i64],
) -> Result<Collection, Failure> {
    validate(&params.name, &params.description)?;
    if collection::count_owned(&state.pool, owner.id).await? >= collection::PER_ACCOUNT_LIMIT {
        return Err(Invalid::CollectionLimit.into());
    }
    if i64::try_from(account_ids.len()).unwrap_or(i64::MAX) > collection::MAX_ITEMS {
        return Err(Invalid::ItemLimit.into());
    }
    // Resolve and policy-check every member up front (the whole create fails
    // if any is not featureable, like Mastodon's `build_items`).
    let mut members = Vec::with_capacity(account_ids.len());
    for &account_id in account_ids {
        let member = account::find_by_id(&state.pool, account_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if !featureable(state, owner, &member).await? {
            return Err(Invalid::NotFeatureable.into());
        }
        members.push(member);
    }
    let tag_id = resolve_tag(state, params.tag_name.as_deref()).await?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let collection = collection::create(
        &mut *tx,
        NewCollection {
            account_id: owner.id,
            name: params.name.trim(),
            description: &params.description,
            language: params.language.as_deref(),
            sensitive: params.sensitive,
            discoverable: params.discoverable,
            local: true,
            tag_id,
            uri: None,
            url: None,
            original_number_of_items: None,
        },
    )
    .await?;
    for member in &members {
        add_member_conn(state, &mut tx, &collection, owner, member).await?;
    }
    // Announce the whole collection to the owner's followers.
    let owner_uri = account_uri(&state.config.domain, owner);
    let object = featured_collection_value_conn(state, &mut tx, &collection).await?;
    let activity = featured::add_featured_collection(
        &owner_uri,
        &owner_collections_url(state, owner),
        &object,
    );
    distribute_collection_conn(state, &mut tx, owner, collection.id, &activity).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    for member in &members {
        notify_member_added(state, &collection, owner, member).await;
    }
    Ok(collection)
}

fn owner_collections_url(state: &AppState, owner: &Account) -> String {
    LocalUserUrls::for_account(&state.config.domain, &owner.username, owner.uri.as_deref())
        .featured_collections
}

/// `AddAccountToCollectionService`: featureability-checked single add.
pub async fn add_account(
    state: &AppState,
    collection: &Collection,
    owner: &Account,
    member: &Account,
) -> Result<CollectionItem, Failure> {
    if collection::count_active_items(&state.pool, collection.id).await? >= collection::MAX_ITEMS {
        return Err(Invalid::ItemLimit.into());
    }
    if !featureable(state, owner, member).await? {
        return Err(Invalid::NotFeatureable.into());
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let item = add_member_conn(state, &mut tx, collection, owner, member).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    notify_member_added(state, collection, owner, member).await;
    Ok(item)
}

/// `UpdateCollectionService`: updates attributes, notifies accepted local
/// members of significant changes, and distributes an
/// `Update(FeaturedCollection)`.
pub async fn update_collection(
    state: &AppState,
    collection: &Collection,
    owner: &Account,
    params: CollectionParams,
) -> Result<Collection, Failure> {
    validate(&params.name, &params.description)?;
    // An absent `tag_name` keeps the current tag; a present (even blank) one
    // sets or clears it.
    let tag_id = match &params.tag_name {
        Some(name) => resolve_tag(state, Some(name)).await?,
        None => collection.tag_id,
    };
    let significant = params.name != collection.name
        || params.description != collection.description
        || params.sensitive != collection.sensitive
        || tag_id != collection.tag_id;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let updated = collection::update(
        &mut *tx,
        collection.id,
        ChangeCollection {
            name: params.name.trim(),
            description: &params.description,
            language: params.language.as_deref(),
            sensitive: params.sensitive,
            discoverable: params.discoverable,
            tag_id,
        },
    )
    .await?;
    let owner_uri = account_uri(&state.config.domain, owner);
    let coll_uri = collection_uri(&state.config.domain, &updated, owner);
    let object = featured_collection_value_conn(state, &mut tx, &updated).await?;
    let epoch = OffsetDateTime::now_utc().unix_timestamp();
    let activity = featured::update_featured_collection(&owner_uri, &coll_uri, &object, epoch);
    distribute_collection_conn(state, &mut tx, owner, updated.id, &activity).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if significant && let Err(error) = notify_collection_update(state, &updated, owner).await {
        tracing::warn!(%error, "post-commit collection update notification failed");
    }
    Ok(updated)
}

/// Notifies accepted local members of a collection's metadata change
/// (`NotifyOfCollectionUpdateService`).
async fn notify_collection_update(
    state: &AppState,
    collection: &Collection,
    owner: &Account,
) -> Result<(), ApiError> {
    let items = collection::items_for(&state.pool, collection.id, owner.id, Some(owner.id)).await?;
    for item in &items {
        if item.state == "accepted"
            && let Some(account_id) = item.account_id
            && let Some(member) = account::find_by_id(&state.pool, account_id).await?
            && member.is_local()
        {
            notification::create_for_collection(
                &state.pool,
                member.id,
                owner.id,
                "collection_update",
                collection.id,
            )
            .await?;
        }
    }
    Ok(())
}

/// `DeleteCollectionService`: distributes a `Remove(FeaturedCollection)`, then
/// deletes the collection (items cascade).
pub async fn delete_collection(
    state: &AppState,
    collection: &Collection,
    owner: &Account,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    let owner_uri = account_uri(&state.config.domain, owner);
    let coll_uri = collection_uri(&state.config.domain, collection, owner);
    let activity = featured::remove_featured_collection(
        &owner_uri,
        &owner_collections_url(state, owner),
        &coll_uri,
    );
    distribute_collection_conn(state, &mut tx, owner, collection.id, &activity).await?;
    collection::delete(&mut *tx, owner.id, collection.id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// `DeleteCollectionItemService`: distributes a `Remove(FeaturedItem)` and
/// deletes the membership (owner-initiated removal).
pub async fn delete_item(
    state: &AppState,
    collection: &Collection,
    owner: &Account,
    item: &CollectionItem,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    let owner_uri = account_uri(&state.config.domain, owner);
    let coll_uri = collection_uri(&state.config.domain, collection, owner);
    let item_uri = item
        .uri
        .clone()
        .unwrap_or_else(|| featured_item_uri(&coll_uri, item.id));
    let activity = featured::remove_featured_item(&owner_uri, &coll_uri, &item_uri);
    distribute_collection_conn(state, &mut tx, owner, collection.id, &activity).await?;
    collection::delete_item(&mut *tx, item.id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// `RevokeCollectionItemService`: the featured account removes itself. For a
/// remote collection we send a `Delete(FeatureAuthorization)` to the owner and
/// mark the membership revoked in the same transaction.
pub async fn revoke_item(
    state: &AppState,
    member: &Account,
    item: &CollectionItem,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    let collection = collection::find(&mut *tx, item.collection_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    collection::set_item_state(&mut *tx, item.id, "revoked", None).await?;
    if !collection.local {
        // The owner is a remote account; address the deletion to its inbox.
        let owner = account::find_by_id(&mut *tx, collection.account_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let stamp_uri = urls::feature_authorization_uri_for_actor(
            &account_uri(&state.config.domain, member),
            item.id,
        );
        let collection_uri = collection.uri.clone().unwrap_or_default();
        let member_uri = account_uri(&state.config.domain, member);
        let stamp = featured::feature_authorization(&stamp_uri, &member_uri, &collection_uri);
        let activity = featured::delete_feature_authorization(&member_uri, &stamp);
        if !owner.is_local() {
            job::enqueue(&mut *tx, member.id, owner.preferred_inbox(), &activity).await?;
        }
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Inbound (Phase C): remote collections and the consent handshake
// ----------------------------------------------------------------------------

/// Whether two URIs live on the same host — Mastodon's `non_matching_uri_hosts?`
/// (inverted). A missing host on either side fails closed.
fn same_host(a: &str, b: &str) -> bool {
    matches!((host_of(a), host_of(b)), (Some(x), Some(y)) if x == y)
}

/// Truncates a string to at most `limit` characters (the hard storage limits
/// for remote names/descriptions).
fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Inbound `FeatureRequest`: a remote `owner` asks to feature one of our local
/// accounts (`object_uri`) in their collection (`instrument_uri`). When the
/// account is featureable we accept — recording an accepted membership,
/// notifying the local user, and returning an `Accept` carrying the
/// `FeatureAuthorization` stamp we serve — otherwise we `Reject`.
pub async fn process_feature_request(
    state: &AppState,
    owner: &Account,
    request_uri: &str,
    object_uri: &str,
    instrument_uri: &str,
) -> Result<(), ApiError> {
    let Some(owner_uri) = owner.uri.as_deref() else {
        return Ok(());
    };
    // The request id must live on the owner's host (anti-spoofing).
    if !same_host(owner_uri, request_uri) {
        return Ok(());
    }
    let Some(featured) =
        crate::local_identity::find_actor(&state.pool, &state.config.domain, object_uri).await?
    else {
        return Ok(()); // not one of our accounts to answer for
    };
    let Some(collection) = find_or_fetch_remote_collection(state, owner, instrument_uri).await?
    else {
        return Ok(());
    };
    let accept = featureable(state, owner, &featured).await?;
    if accept {
        let featured_uri = account_uri(&state.config.domain, &featured);
        let item = collection::add_item(
            &state.pool,
            NewCollectionItem {
                item_id: id::next(),
                collection_id: collection.id,
                account_id: Some(featured.id),
                state: "accepted",
                uri: None,
                object_uri: Some(&featured_uri),
                activity_uri: Some(request_uri),
                approval_uri: None,
            },
        )
        .await?;
        // An existing membership (idempotent redelivery) is reused.
        let item = match item {
            Some(item) => item,
            None => collection::find_item_by_account(&state.pool, collection.id, featured.id)
                .await?
                .ok_or(ApiError::NotFound)?,
        };
        notification::create_for_collection(
            &state.pool,
            featured.id,
            owner.id,
            "added_to_collection",
            collection.id,
        )
        .await?;
        let response = featured::accept_feature_request(&FeatureResponseParams {
            domain: &state.config.domain,
            username: &featured.username,
            actor_id: featured.uri.as_deref(),
            item_id: item.id,
            request_activity_uri: request_uri,
            owner_uri,
        });
        job::enqueue(&state.pool, featured.id, owner.preferred_inbox(), &response).await?;
        tracing::info!(account = %featured.username, owner = %owner.username, "feature request accepted");
    } else {
        let response = featured::reject_feature_request(&FeatureResponseParams {
            domain: &state.config.domain,
            username: &featured.username,
            actor_id: featured.uri.as_deref(),
            item_id: id::next(),
            request_activity_uri: request_uri,
            owner_uri,
        });
        job::enqueue(&state.pool, featured.id, owner.preferred_inbox(), &response).await?;
    }
    Ok(())
}

/// `Accept`/`Reject` of a `FeatureRequest` we sent (a local collection
/// featuring the remote `member`). On acceptance we stamp the granted
/// authorization and re-distribute an `Add(FeaturedItem)`; on rejection the
/// membership is marked rejected.
pub async fn process_feature_response(
    state: &AppState,
    member: &Account,
    request_uri: &str,
    approval_uri: Option<&str>,
    accepted: bool,
) -> Result<(), ApiError> {
    let Some(item) = collection::find_item_by_activity_uri(&state.pool, request_uri).await? else {
        return Ok(());
    };
    if item.account_id != Some(member.id) {
        return Ok(()); // the responder is not the featured account
    }
    let collection = collection::find(&state.pool, item.collection_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !collection.local {
        return Ok(());
    }
    let owner = account::find_by_id(&state.pool, collection.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !accepted {
        collection::set_item_state(&state.pool, item.id, "rejected", None).await?;
        return Ok(());
    }
    // The stamp must live on the accepting member's host.
    let Some(approval_uri) = approval_uri.filter(|uri| {
        member
            .uri
            .as_deref()
            .is_some_and(|owner_uri| same_host(uri, owner_uri))
    }) else {
        return Ok(());
    };
    let item =
        collection::set_item_state(&state.pool, item.id, "accepted", Some(approval_uri)).await?;
    tracing::info!(member = %member.username, owner = %owner.username, "feature request accepted by remote");
    // Re-distribute the now-authorized membership.
    let owner_uri = account_uri(&state.config.domain, &owner);
    let coll_uri = collection_uri(&state.config.domain, &collection, &owner);
    let value = featured_item_value(state, &coll_uri, &item, member)?;
    let activity = featured::add_featured_item(&owner_uri, &coll_uri, &value);
    distribute_collection(state, &owner, collection.id, &activity).await?;
    Ok(())
}

/// Inbound `Delete(FeatureAuthorization)`: the remote `member` revokes the
/// consent it granted to one of our local collections. The membership is
/// removed and a `Remove(FeaturedItem)` is distributed.
pub async fn process_feature_authorization_delete(
    state: &AppState,
    member: &Account,
    stamp_uri: &str,
) -> Result<(), ApiError> {
    let Some(item) = collection::find_item_by_approval(&state.pool, member.id, stamp_uri).await?
    else {
        return Ok(());
    };
    let collection = collection::find(&state.pool, item.collection_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !collection.local {
        return Ok(());
    }
    let owner = account::find_by_id(&state.pool, collection.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    delete_item(state, &collection, &owner, &item).await?;
    tracing::info!(member = %member.username, "remote member revoked a feature authorization");
    Ok(())
}

/// Ingests a remote `FeaturedCollection` document (inbound `Add`/`Update`),
/// owned by `owner`: upserts the collection, reconciles its items, and notifies
/// accepted local members. Mirrors `ProcessFeaturedCollectionService`.
pub async fn process_remote_collection(
    state: &AppState,
    owner: &Account,
    json: &Value,
) -> Result<Option<Collection>, ApiError> {
    let Some(owner_uri) = owner.uri.as_deref() else {
        return Ok(None);
    };
    let Some(collection_uri) = json.get("id").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !same_host(owner_uri, collection_uri) {
        return Ok(None);
    }
    if json.get("attributedTo").and_then(id_of) != Some(owner_uri) {
        return Ok(None);
    }
    let name = truncate_chars(
        json.get("name").and_then(Value::as_str).unwrap_or(""),
        collection::NAME_LENGTH_HARD_LIMIT,
    );
    let (description, language) = remote_summary(json);
    let url = json
        .get("url")
        .and_then(id_of)
        .filter(|u| u.starts_with("https://"))
        .unwrap_or(collection_uri);
    let sensitive = json
        .get("sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let discoverable = json
        .get("discoverable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let total_items = json
        .get("totalItems")
        .and_then(Value::as_i64)
        .map(|n| i32::try_from(n).unwrap_or(i32::MAX));
    let tag_id = match json
        .get("topic")
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
    {
        Some(name) => Some(tag::ensure(&state.pool, name.trim_start_matches('#')).await?),
        None => None,
    };
    let new = NewCollection {
        account_id: owner.id,
        name: &name,
        description: &description,
        language: language.as_deref(),
        sensitive,
        discoverable,
        local: false,
        tag_id,
        uri: Some(collection_uri),
        url: Some(url),
        original_number_of_items: total_items,
    };
    let collection = match collection::find_by_uri(&state.pool, collection_uri).await? {
        Some(existing) if existing.account_id == owner.id => {
            collection::update_remote(&state.pool, existing.id, &new).await?
        }
        Some(_) => return Ok(None), // a collection uri claimed by the wrong owner
        None => collection::create(&state.pool, new).await?,
    };
    // Reconcile membership against the advertised items.
    let empty = Vec::new();
    let items = json
        .get("orderedItems")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut keep = Vec::new();
    for item_json in items.iter().take(150) {
        if let Some(uri) = item_json.get("id").and_then(Value::as_str) {
            keep.push(uri.to_owned());
        }
        process_remote_item(state, owner, &collection, item_json).await?;
    }
    collection::delete_items_not_in(&state.pool, collection.id, &keep).await?;
    notify_collection_update(state, &collection, owner).await?;
    Ok(Some(collection))
}

/// The description (sanitized HTML) and language of a remote collection, from
/// its `summaryMap` (preferred, carries the language) or plain `summary`.
fn remote_summary(json: &Value) -> (String, Option<String>) {
    if let Some(map) = json.get("summaryMap").and_then(Value::as_object)
        && let Some((language, text)) = map.iter().next()
    {
        let html = plamenu_ap::text::sanitize_remote_html(text.as_str().unwrap_or(""));
        return (
            truncate_chars(&html, collection::DESCRIPTION_LENGTH_HARD_LIMIT),
            Some(language.clone()),
        );
    }
    let html = plamenu_ap::text::sanitize_remote_html(
        json.get("summary").and_then(Value::as_str).unwrap_or(""),
    );
    (
        truncate_chars(&html, collection::DESCRIPTION_LENGTH_HARD_LIMIT),
        None,
    )
}

/// Ingests one `FeaturedItem` of a remote collection. A local featured account
/// is honored only when it already pre-approved the membership (we never let a
/// remote unilaterally claim our user's consent); a remote one is recorded with
/// its granted stamp.
pub async fn process_remote_item(
    state: &AppState,
    owner: &Account,
    collection: &Collection,
    item_json: &Value,
) -> Result<(), ApiError> {
    let Some(item_uri) = item_json.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(collection_uri) = collection.uri.as_deref() else {
        return Ok(());
    };
    if !same_host(collection_uri, item_uri) {
        return Ok(());
    }
    let Some(object_uri) = item_json.get("featuredObject").and_then(id_of) else {
        return Ok(());
    };
    let approval_uri = item_json.get("featureAuthorization").and_then(id_of);

    if let Some(local) =
        crate::local_identity::find_actor(&state.pool, &state.config.domain, object_uri).await?
    {
        // A local account: only honor a membership it has already approved.
        if let Some(existing) =
            collection::find_item_by_account(&state.pool, collection.id, local.id).await?
            && existing.state == "accepted"
        {
            collection::relink_remote_item(
                &state.pool,
                existing.id,
                None,
                item_uri,
                object_uri,
                None,
            )
            .await?;
        }
        return Ok(());
    }

    // A remote account: actor and stamp must share a host.
    let Some(approval_uri) = approval_uri.filter(|uri| same_host(object_uri, uri)) else {
        return Ok(());
    };
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, object_uri)
        .await?
    {
        return Ok(());
    }
    let member = match account::find_by_uri(&state.pool, object_uri).await? {
        Some(account) => {
            if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account)
                .await?
            {
                return Ok(());
            }
            account
        }
        None => match state.federation.fetch_actor(object_uri).await {
            Ok(actor) => refresh_remote_actor(state, &actor).await?,
            Err(_) => return Ok(()),
        },
    };
    let _ = owner;
    if let Some(existing) =
        collection::find_item_by_uri(&state.pool, collection.id, item_uri).await?
    {
        collection::relink_remote_item(
            &state.pool,
            existing.id,
            Some(member.id),
            item_uri,
            object_uri,
            Some(approval_uri),
        )
        .await?;
    } else {
        collection::add_item(
            &state.pool,
            NewCollectionItem {
                item_id: id::next(),
                collection_id: collection.id,
                account_id: Some(member.id),
                state: "accepted",
                uri: Some(item_uri),
                object_uri: Some(object_uri),
                activity_uri: None,
                approval_uri: Some(approval_uri),
            },
        )
        .await?;
    }
    Ok(())
}

/// Inbound `Remove(FeaturedCollection)`: the remote `owner` retires a
/// collection we track.
pub async fn remove_remote_collection(
    state: &AppState,
    owner: &Account,
    collection_uri: &str,
) -> Result<(), ApiError> {
    if let Some(collection) = collection::find_by_uri(&state.pool, collection_uri).await?
        && collection.account_id == owner.id
    {
        collection::delete_by_uri(&state.pool, collection_uri).await?;
    }
    Ok(())
}

/// Inbound `Add(FeaturedItem)` targeting a known remote collection.
pub async fn add_remote_item(
    state: &AppState,
    owner: &Account,
    collection: &Collection,
    item_json: &Value,
) -> Result<(), ApiError> {
    process_remote_item(state, owner, collection, item_json).await
}

/// Inbound `Remove(FeaturedItem)`: drops the membership from a remote
/// collection we track (no re-distribution — we are the receiver).
pub async fn remove_remote_item(
    state: &AppState,
    collection: &Collection,
    item_uri: &str,
) -> Result<(), ApiError> {
    if let Some(item) = collection::find_item_by_uri(&state.pool, collection.id, item_uri).await? {
        collection::delete_item(&state.pool, item.id).await?;
    }
    Ok(())
}

/// Resolves a `FeaturedCollection` referenced in an inbound Note's `tag`
/// (Mastodon's `process_tagged_collection`): a local collection by its AP id,
/// a known remote one, or — when unknown — a policy-checked fetch ingested
/// under its own declared `attributedTo` owner (which need not be the Note's
/// author). `None` when the id is not a resolvable collection.
pub async fn find_or_fetch_tagged_collection(
    state: &AppState,
    _note_author: &Account,
    id: &str,
) -> Result<Option<Collection>, ApiError> {
    let local_ref = if let Some((account_id, collection_id)) =
        urls::parse_local_numeric_collection_url(&state.config.domain, id)
    {
        Some((
            format!("https://{}/ap/accounts/{account_id}", state.config.domain),
            collection_id,
        ))
    } else {
        urls::parse_local_collection_url(&state.config.domain, id).map(
            |(username, collection_id)| {
                (
                    format!("https://{}/users/{username}", state.config.domain),
                    collection_id,
                )
            },
        )
    };
    if let Some((actor_uri, collection_id)) = local_ref {
        let Some(collection) = collection::find(&state.pool, collection_id).await? else {
            return Ok(None);
        };
        let owner =
            crate::local_identity::find_actor(&state.pool, &state.config.domain, &actor_uri)
                .await?;
        let matches = owner.is_some_and(|o| o.id == collection.account_id);
        return Ok(matches.then_some(collection));
    }
    if let Some(existing) = collection::find_by_uri(&state.pool, id).await? {
        return Ok(Some(existing));
    }
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, id).await? {
        return Ok(None);
    }
    let Ok(json) = state.federation.fetch_object(id).await else {
        return Ok(None);
    };
    if json.get("type").and_then(Value::as_str) != Some("FeaturedCollection") {
        return Ok(None);
    }
    let Some(owner_uri) = json.get("attributedTo").and_then(id_of) else {
        return Ok(None);
    };
    let owner = if let Some(owner) = account::find_by_uri(&state.pool, owner_uri).await? {
        owner
    } else {
        let Ok(actor) = state.federation.fetch_actor(owner_uri).await else {
            return Ok(None);
        };
        refresh_remote_actor(state, &actor).await?
    };
    process_remote_collection(state, &owner, &json).await
}

/// A remote collection by URI owned by `owner`, fetching and ingesting it once
/// when unknown — the lookup an inbound `FeatureRequest` needs before it can
/// record a membership.
async fn find_or_fetch_remote_collection(
    state: &AppState,
    owner: &Account,
    uri: &str,
) -> Result<Option<Collection>, ApiError> {
    if let Some(existing) = collection::find_by_uri(&state.pool, uri).await?
        && existing.account_id == owner.id
    {
        return Ok(Some(existing));
    }
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await? {
        return Ok(None);
    }
    let Ok(json) = state.federation.fetch_object(uri).await else {
        return Ok(None);
    };
    if json.get("type").and_then(Value::as_str) != Some("FeaturedCollection") {
        return Ok(None);
    }
    process_remote_collection(state, owner, &json).await
}

/// Resolves a remote collection this `owner` claims, for `Add`/`Remove` of a
/// `FeaturedItem` whose `target` is the collection's URI.
pub async fn owned_remote_collection(
    state: &AppState,
    owner: &Account,
    collection_uri: &str,
) -> Result<Option<Collection>, ApiError> {
    Ok(collection::find_by_uri(&state.pool, collection_uri)
        .await?
        .filter(|c| c.account_id == owner.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_wording_is_unchanged_by_the_typed_error() {
        let rendered = |errors: Vec<Invalid>| match ApiError::from(Failure::Invalid(errors)) {
            ApiError::Unprocessable(message) => message,
            other => panic!("expected an unprocessable rejection, got {other:?}"),
        };
        assert_eq!(
            rendered(vec![Invalid::NameBlank]),
            "Validation failed: Name can't be blank"
        );
        assert_eq!(
            rendered(vec![Invalid::NameTooLong, Invalid::DescriptionTooLong]),
            "Validation failed: Name is too long (maximum is 40 characters), \
             Description is too long (maximum is 100 characters)"
        );
        assert_eq!(
            rendered(vec![Invalid::CollectionLimit]),
            "Validation failed: You have reached the limit of 10 collections"
        );
        assert_eq!(
            rendered(vec![Invalid::ItemLimit]),
            "Validation failed: Collection items can have at most 25 accounts"
        );
        assert_eq!(
            rendered(vec![Invalid::NotFeatureable]),
            "Validation failed: Account can't be added to collections"
        );
        assert_eq!(
            rendered(vec![Invalid::AlreadyAMember]),
            "Validation failed: Account has already been taken"
        );
    }
}
