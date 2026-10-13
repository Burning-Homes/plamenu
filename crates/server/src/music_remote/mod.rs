//! Canonical remote music resources remain references to their origin.
pub mod artwork;
pub mod catalogue;
pub mod details;
pub mod document;
mod members;
pub mod page;
mod proofs;
pub mod stream;
use crate::{AppState, error::ApiError};
use document::Document;
use plamenu_db::{
    DbError,
    account::{self, Account},
    id,
};
use std::collections::{HashMap, HashSet, VecDeque};
use time::OffsetDateTime;

#[derive(Clone, sqlx::FromRow, serde::Serialize)]
pub struct Resource {
    #[serde(serialize_with = "serialize_id")]
    pub id: i64,
    pub uri: String,
    #[serde(skip)]
    pub controller_id: i64,
    #[serde(skip)]
    pub proof_uri: String,
    #[serde(skip)]
    pub proof_kind: String,
    pub kind: String,
    pub name: String,
    pub summary: String,
    #[serde(serialize_with = "details::serialize_public")]
    #[sqlx(try_from = "details::Details")]
    pub details: sqlx::types::Json<details::Details>,
    pub media_of: Option<String>,
    pub parent_uri: Option<String>,
    pub next_page_uri: Option<String>,
    #[serde(skip)]
    pub audio_url: Option<String>,
    #[serde(skip)]
    pub catalogue_kind: Option<String>,
    #[serde(skip)]
    pub catalogue_key: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: OffsetDateTime,
    #[serde(skip)]
    pub unavailable_at: Option<OffsetDateTime>,
}
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "Serde serializer callback"
)]
pub(crate) fn serialize_id<S: serde::Serializer>(
    value: &i64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}
fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(message.into())
}
async fn permitted(state: &AppState, controller: &Account, viewer: i64) -> Result<bool, ApiError> {
    if controller.domain.is_none()
        || controller.suspended()
        || account::is_deleted(&state.pool, controller.id).await?
        || account::effectively_silenced(&state.pool, controller.id).await?
    {
        return Ok(false);
    }
    if !crate::instance_policy::account_id_visible(&state.pool, controller.id).await? {
        return Ok(false);
    }
    Ok(
        !sqlx::query_scalar::<_, bool>("SELECT account_hidden($1,$2)")
            .bind(Some(viewer))
            .bind(controller.id)
            .fetch_one(&state.pool)
            .await
            .map_err(DbError::from)?,
    )
}
async fn verified_controller(
    state: &AppState,
    uri: &str,
    viewer: i64,
) -> Result<Account, ApiError> {
    document::url(uri)?;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await? {
        return Err(ApiError::NotFound);
    }
    let actor = state
        .federation
        .fetch_actor(uri)
        .await
        .map_err(|_| invalid("Remote music controller is unavailable"))?;
    if actor.id != uri
        || !matches!(
            actor.kind.as_str(),
            "Person" | "Service" | "Application" | "Group" | "Organization"
        )
    {
        return Err(invalid("Remote music controller identity is invalid"));
    }
    let owner = crate::remote::refresh_remote_actor(state, &actor).await?;
    if !permitted(state, &owner, viewer).await? {
        return Err(ApiError::NotFound);
    }
    Ok(owner)
}
async fn fetch(state: &AppState, uri: &str) -> Result<serde_json::Value, ApiError> {
    document::url(uri)?;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await? {
        return Err(ApiError::NotFound);
    }
    let fetched = state
        .federation
        .fetch_activitypub_same_origin(uri)
        .await
        .map_err(|error| match error {
            plamenu_federation::FederationError::Status(401 | 403 | 404 | 410) => {
                ApiError::NotFound
            }
            _ => invalid("Remote music could not be fetched safely"),
        })?;
    if fetched.body_bytes > 2 * 1024 * 1024 {
        return Err(invalid("Remote music document is too large"));
    }
    fetched
        .document
        .ok_or_else(|| invalid("Remote music document is missing"))
}
/// Explicit import is bounded, controller-verified and all-or-nothing. It never
/// saves, follows, copies audio bytes, creates local music or publishes a post.
pub async fn import(state: &AppState, viewer: i64, uri: &str) -> Result<Resource, ApiError> {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        import_inner(state, viewer, uri),
    )
    .await
    .map_err(|_| invalid("Remote music import timed out"))?
}
async fn import_inner(state: &AppState, viewer: i64, uri: &str) -> Result<Resource, ApiError> {
    let uri = uri.trim();
    if catalogue::track_key(uri).is_some() {
        return catalogue::import_track(state, viewer, uri).await;
    }
    let parsed = document::url(uri)?;
    if parsed.path().starts_with("/api/v1/radios/") {
        return catalogue::import_radio(state, viewer, uri).await;
    }
    if parsed.path().starts_with("/streams/") {
        return catalogue::import_stream(state, viewer, uri).await;
    }
    let origin = parsed;
    if origin.host_str() == Some(state.config.domain.as_str()) {
        return Err(invalid("Use the local music reference"));
    }
    let started: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&state.pool)
        .await
        .map_err(DbError::from)?;
    let lookup = resolve_root(state, uri, started).await?;
    let canonical = lookup.uri;
    let mut root = lookup.document;
    let uri = canonical.as_str();
    if document::Kind::parse(&root).is_some_and(|kind| !document::public(&root, kind, false)) {
        retire_uri(state, uri, started).await?;
        return Err(ApiError::NotFound);
    }
    let pages = first_page(state, &mut root, uri, lookup.page).await?;
    let (documents, unavailable) =
        canonical_documents(state, document::parse(&root, uri)?, uri, started).await?;
    let resource = cache_import(state, viewer, uri, documents, unavailable, pages, started).await?;
    catalogue::enrich(state, viewer, &resource).await?;
    get(state, resource.id, viewer).await
}
async fn cache_import(
    state: &AppState,
    viewer: i64,
    uri: &str,
    documents: Vec<Document>,
    unavailable: Vec<String>,
    pages: members::Pages,
    started: OffsetDateTime,
) -> Result<Resource, ApiError> {
    let mut controllers = HashMap::new();
    for document in &documents {
        if !controllers.contains_key(&document.controller) {
            let owner = verified_controller(state, &document.controller, viewer).await?;
            controllers.insert(document.controller.clone(), owner);
        }
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, viewer).await?;
    crate::music_queues::active_owner(&mut tx, viewer).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('music-remote-cache-budget',0))")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let mut uris = documents
        .iter()
        .map(|document| document.uri.clone())
        .collect::<Vec<_>>();
    uris.sort();
    uris.dedup();
    let retained: i64 =
        sqlx::query_scalar("SELECT count(*) FROM music_remote_resources WHERE NOT(uri=ANY($1))")
            .bind(&uris)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if retained + i64::try_from(uris.len()).unwrap_or(10001) > 10000 {
        return Err(invalid("Remote music cache limit reached"));
    }
    let mut controller_ids = controllers
        .values()
        .map(|owner| owner.id)
        .collect::<Vec<_>>();
    controller_ids.sort_unstable();
    let locked: Vec<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE id=ANY($1) AND domain IS NOT NULL AND suspended_at IS NULL AND deleted_at IS NULL AND NOT account_silenced(id) AND instance_domain_allowed(domain) ORDER BY id FOR SHARE")
        .bind(&controller_ids).fetch_all(&mut *tx).await.map_err(DbError::from)?;
    if locked != controller_ids {
        return Err(ApiError::NotFound);
    }
    let mut locked_uris = uris.clone();
    locked_uris.extend(unavailable.iter().cloned());
    locked_uris.sort();
    locked_uris.dedup();
    for uri in locked_uris {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('music-remote:' || $1,0))")
            .bind(uri)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp(),changed_at=clock_timestamp(),name='',summary='',details=ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details,audio_url=NULL,next_page_uri=NULL WHERE uri=ANY($1) AND changed_at<=$2")
        .bind(&unavailable).bind(started).execute(&mut *tx).await.map_err(DbError::from)?;
    store_documents(&mut tx, &documents, &controllers, started).await?;
    members::store(&mut tx, uri, &pages, &documents, started).await?;
    sqlx::query("UPDATE music_remote_resources SET next_page_uri=$1 WHERE uri=$2")
        .bind(pages.next)
        .bind(uri)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    let row: Resource = sqlx::query_as(
        "SELECT * FROM music_remote_resources WHERE uri=$1 AND unavailable_at IS NULL",
    )
    .bind(uri)
    .fetch_optional(&state.pool)
    .await
    .map_err(DbError::from)?
    .ok_or(ApiError::NotFound)?;
    get(state, row.id, viewer).await
}

async fn store_documents(
    conn: &mut sqlx::PgConnection,
    documents: &[Document],
    controllers: &HashMap<String, Account>,
    started: OffsetDateTime,
) -> Result<(), ApiError> {
    let mut stored = HashSet::new();
    for document in documents {
        let owner = controllers
            .get(&document.controller)
            .ok_or(ApiError::NotFound)?;
        if stored.insert(&document.uri) {
            store(conn, document, owner.id, started).await?;
        }
    }
    for document in documents {
        let owner = controllers
            .get(&document.controller)
            .ok_or(ApiError::NotFound)?;
        proofs::store(conn, document, owner.id, started).await?;
    }
    Ok(())
}

async fn retire_uri(state: &AppState, uri: &str, started: OffsetDateTime) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('music-remote:' || $1,0))")
        .bind(uri)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp(),changed_at=clock_timestamp(),name='',summary='',details=ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details,audio_url=NULL,next_page_uri=NULL WHERE uri=$1 AND changed_at<=$2").bind(uri).bind(started).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
async fn fetch_root(
    state: &AppState,
    uri: &str,
    started: OffsetDateTime,
) -> Result<serde_json::Value, ApiError> {
    match fetch(state, uri).await {
        Ok(value) => Ok(value),
        Err(ApiError::NotFound) => {
            retire_uri(state, uri, started).await?;
            Err(ApiError::NotFound)
        }
        Err(error) => Err(error),
    }
}
struct Lookup {
    uri: String,
    document: serde_json::Value,
    page: Option<(String, serde_json::Value)>,
}
async fn resolve_root(
    state: &AppState,
    requested: &str,
    started: OffsetDateTime,
) -> Result<Lookup, ApiError> {
    use plamenu_ap::activity::id_of;
    let origin = document::url(requested)?.origin();
    let mut body = fetch_root(state, requested, started).await?;
    let mut uri = requested.to_owned();
    let mut page = None;
    if matches!(
        body["type"].as_str(),
        Some("CollectionPage" | "OrderedCollectionPage")
    ) {
        if body["id"].as_str() != Some(requested) {
            return Err(invalid("Remote music page identity differs from its URL"));
        }
        let parent = id_of(&body["partOf"])
            .ok_or_else(|| invalid("Remote music page has no library"))?
            .to_owned();
        if document::url(&parent)?.origin() != origin {
            return Err(invalid("Remote music page parent is on a different origin"));
        }
        page = Some((requested.to_owned(), body));
        body = fetch_root(state, &parent, started).await?;
        uri = parent;
        if !matches!(
            document::Kind::parse(&body),
            Some(document::Kind::Library | document::Kind::Playlist)
        ) {
            return Err(invalid("Remote music page parent is not a library"));
        }
    }
    let canonical = body["id"]
        .as_str()
        .ok_or_else(|| invalid("Remote music has no canonical identity"))?
        .to_owned();
    if canonical != uri {
        if document::url(&canonical)?.origin() != origin {
            return Err(invalid(
                "Remote music canonical identity is on a different origin",
            ));
        }
        body = fetch_root(state, &canonical, started).await?;
    }
    Ok(Lookup {
        uri: canonical,
        document: body,
        page,
    })
}
/// Fetch at most the first page plus an explicitly requested page. The parent,
/// public audience, controller and next pointer are validated before caching.
async fn first_page(
    state: &AppState,
    root: &mut serde_json::Value,
    uri: &str,
    requested: Option<(String, serde_json::Value)>,
) -> Result<members::Pages, ApiError> {
    use plamenu_ap::activity::id_of;
    let index = document::parse(root, uri)?;
    let library = index.first().ok_or(ApiError::NotFound)?;
    if !matches!(
        library.kind,
        document::Kind::Library | document::Kind::Playlist
    ) {
        return Ok(members::Pages::default());
    }
    let mut items = root
        .get("items")
        .or_else(|| root.get("orderedItems"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut pages = members::Pages {
        next: None,
        exhaustive: false,
        items: vec![(uri.to_owned(), items.clone())],
    };
    let first = root.get("first").and_then(id_of).map(str::to_owned);
    pages.exhaustive = first.is_none();
    if let Some(first) = &first {
        if document::url(first)?.origin() != document::url(uri)?.origin() {
            return Err(invalid("Remote music page is on a different origin"));
        }
        let body = if let Some((requested, value)) = &requested
            && requested == first
        {
            value.clone()
        } else {
            fetch(state, first).await?
        };
        let parsed = page::parse(uri, &library.controller, first, &body)?;
        pages.items.push((first.to_owned(), parsed.items.clone()));
        items.extend(parsed.items);
        pages.exhaustive = parsed.next.is_none();
        pages.next = parsed.next;
    }
    if let Some((requested, body)) = requested
        && first.as_deref() != Some(requested.as_str())
    {
        let parsed = page::parse(uri, &library.controller, &requested, &body)?;
        pages.items.push((requested, parsed.items.clone()));
        items.extend(parsed.items);
        pages.next = parsed.next;
    }
    if items.len() > 500 {
        return Err(invalid("Remote music lookup contains too many page items"));
    }
    root["items"] = serde_json::json!(items);
    Ok(pages)
}
/// The pinned server embeds credits under the publisher while canonical
/// artist/album pages use its instance actor. Independently served metadata
/// determines entity authority, and an inaccessible album is not cached as a
/// public resource merely because its credit appears on a public track.
async fn canonical_documents(
    state: &AppState,
    documents: Vec<Document>,
    root: &str,
    started: OffsetDateTime,
) -> Result<(Vec<Document>, Vec<String>), ApiError> {
    let root_controller = documents
        .first()
        .ok_or(ApiError::NotFound)?
        .controller
        .clone();
    let mut queue = VecDeque::from(documents);
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut unavailable = Vec::new();
    let mut trusted_audio = HashMap::new();
    let mut track_proofs = HashSet::new();
    while let Some(mut document) = queue.pop_front() {
        if !seen.insert(document.uri.clone()) && document.kind != document::Kind::Track {
            continue;
        }
        if document.kind == document::Kind::Track
            && !track_proofs.insert((document.uri.clone(), document.proof_uri.clone()))
        {
            continue;
        }
        if seen.len() > document::MAX_DOCUMENTS {
            return Err(invalid("Remote music document graph is too large"));
        }
        let legacy = document.kind == document::Kind::Audio
            && document.media_of.as_deref() == Some(document.uri.as_str());
        let proven_track = document.kind == document::Kind::Track
            && document.proof_kind == document::Kind::Audio
            && trusted_audio.get(&document.proof_uri) == Some(&document.controller);
        if document.uri != root
            && (legacy
                || matches!(
                    document.kind,
                    document::Kind::Artist | document::Kind::Album
                )
                || (document.controller != root_controller && !proven_track))
        {
            if legacy {
                let mut verified = legacy_audio(state, &document, started).await?;
                let actual = verified.remove(0);
                trusted_audio.insert(actual.uri.clone(), actual.controller.clone());
                if actual.uri != document.uri {
                    seen.remove(&document.uri);
                }
                seen.insert(actual.uri.clone());
                document = actual;
                queue.extend(verified);
            } else {
                let source = match fetch(state, &document.uri).await {
                    Ok(value) => value,
                    Err(ApiError::NotFound)
                        if matches!(
                            document.kind,
                            document::Kind::Artist | document::Kind::Album
                        ) =>
                    {
                        unavailable.push(document.uri);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let mut verified = document::parse(&source, &document.uri)?;
                let authoritative = verified.remove(0);
                if authoritative.kind != document.kind {
                    return Err(invalid(
                        "Embedded music type differs from its canonical source",
                    ));
                }
                if authoritative.kind == document::Kind::Audio {
                    trusted_audio
                        .insert(authoritative.uri.clone(), authoritative.controller.clone());
                }
                document = authoritative;
                queue.extend(verified);
            }
        }
        result.push(document);
    }
    Ok((result, unavailable))
}
async fn legacy_audio(
    state: &AppState,
    document: &Document,
    started: OffsetDateTime,
) -> Result<Vec<Document>, ApiError> {
    let lookup = resolve_root(state, &document.uri, started).await?;
    let mut verified = document::parse(&lookup.document, &lookup.uri)?;
    let actual = verified.first_mut().ok_or(ApiError::NotFound)?;
    if actual.kind != document::Kind::Audio
        || actual.media_of.as_deref() != Some(document.uri.as_str())
    {
        return Err(invalid(
            "Legacy music reference differs from its canonical recording",
        ));
    }
    actual.parent.clone_from(&document.parent);
    Ok(verified)
}

async fn store(
    conn: &mut sqlx::PgConnection,
    document: &Document,
    owner: i64,
    started: OffsetDateTime,
) -> Result<(), ApiError> {
    let changed=sqlx::query("INSERT INTO music_remote_resources(id,uri,controller_id,kind,name,summary,media_of,parent_uri,audio_url,fetched_at,changed_at,proof_uri,details,proof_kind,catalogue_kind,catalogue_key) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,clock_timestamp(),$11,$12,$13,$14,$15) ON CONFLICT(uri) DO UPDATE SET kind=EXCLUDED.kind,name=EXCLUDED.name,summary=EXCLUDED.summary,details=EXCLUDED.details,media_of=EXCLUDED.media_of,proof_uri=EXCLUDED.proof_uri,proof_kind=EXCLUDED.proof_kind,catalogue_kind=EXCLUDED.catalogue_kind,catalogue_key=EXCLUDED.catalogue_key,parent_uri=EXCLUDED.parent_uri,audio_url=EXCLUDED.audio_url,fetched_at=EXCLUDED.fetched_at,unavailable_at=NULL,changed_at=clock_timestamp() WHERE music_remote_resources.controller_id=EXCLUDED.controller_id AND music_remote_resources.tombstoned_at IS NULL AND music_remote_resources.changed_at <= EXCLUDED.fetched_at")
        .bind(id::next()).bind(&document.uri).bind(owner).bind(document.kind.text()).bind(&document.name).bind(&document.summary).bind(&document.media_of).bind(&document.parent).bind(&document.audio_url).bind(started).bind(&document.proof_uri).bind(&document.details).bind(document.proof_kind.text()).bind(document.catalogue.as_ref().map(|c|c.0.as_str())).bind(document.catalogue.as_ref().map(|c|c.1.as_str())).execute(conn).await.map_err(DbError::from)?;
    if changed.rows_affected() != 1 {
        return Err(invalid("Remote music changed while it was being fetched"));
    }
    Ok(())
}
/// Current viewer/controller policy is checked before revealing cached metadata.
pub async fn get(state: &AppState, id: i64, viewer: i64) -> Result<Resource, ApiError> {
    for_ids(state, &[id], viewer)
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)
}

/// The inbox calls this only after HTTP-signature verification. It updates
/// known origin-controlled cache rows; importing unsolicited graphs is separate.
pub async fn incoming(
    state: &AppState,
    sender: &Account,
    raw: &serde_json::Value,
) -> Result<(), ApiError> {
    use plamenu_ap::activity::id_of;
    let Some(action) = raw["type"]
        .as_str()
        .filter(|kind| matches!(*kind, "Update" | "Delete"))
    else {
        return Ok(());
    };
    let Some(uri) = id_of(&raw["object"]) else {
        return Ok(());
    };
    let controller = crate::entities::account_uri(&state.config.domain, sender);
    let parsed = (action == "Update").then(|| document::parse(&raw["object"], uri));
    let docs = parsed.and_then(Result::ok).filter(|docs| {
        docs.first()
            .is_some_and(|root| root.controller == controller)
    });
    let mut uris = vec![uri.to_owned()];
    if let Some(docs) = &docs {
        uris.extend(
            docs.iter()
                .filter(|doc| doc.controller == controller)
                .map(|doc| doc.uri.clone()),
        );
    }
    uris.sort();
    uris.dedup();
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    for uri in uris {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('music-remote:' || $1,0))")
            .bind(uri)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    let known: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM music_remote_resources WHERE uri=$1 AND controller_id=$2 FOR UPDATE",
    )
    .bind(uri)
    .bind(sender.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if known.is_none() {
        return Ok(());
    }
    if let Some(docs) = docs {
        for document in docs.iter().filter(|doc| doc.controller == controller) {
            update_known(&mut tx, document, sender.id).await?;
        }
    } else {
        sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp(),tombstoned_at=CASE WHEN $3 THEN clock_timestamp() ELSE tombstoned_at END,changed_at=clock_timestamp(),name='',summary='',details=ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details,audio_url=NULL,next_page_uri=NULL WHERE uri=$1 AND controller_id=$2 AND catalogue_kind IS NULL").bind(uri).bind(sender.id).bind(action=="Delete").execute(&mut *tx).await.map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
async fn update_known(
    conn: &mut sqlx::PgConnection,
    document: &Document,
    sender: i64,
) -> Result<(), ApiError> {
    let changed=sqlx::query_scalar::<_,OffsetDateTime>("UPDATE music_remote_resources SET kind=$3,next_page_uri=NULL,name=$4,summary=$5,media_of=$6,parent_uri=$7,audio_url=$8,proof_uri=$9,details=$10,proof_kind=$11,unavailable_at=NULL,changed_at=clock_timestamp(),fetched_at=clock_timestamp() WHERE uri=$1 AND controller_id=$2 AND catalogue_kind IS NULL AND tombstoned_at IS NULL AND EXISTS(SELECT 1 FROM music_remote_resources proof WHERE proof.uri=$9 AND (proof.uri=$1 OR proof.unavailable_at IS NULL)) RETURNING fetched_at")
        .bind(&document.uri).bind(sender).bind(document.kind.text()).bind(&document.name).bind(&document.summary).bind(&document.media_of).bind(&document.parent).bind(&document.audio_url).bind(&document.proof_uri).bind(&document.details).bind(document.proof_kind.text()).fetch_optional(&mut *conn).await.map_err(DbError::from)?;
    if let Some(started) = changed {
        proofs::store(conn, document, sender, started).await?;
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct ResourceView<'a> {
    #[serde(flatten)]
    resource: &'a Resource,
    url: String,
    stream_url: Option<String>,
    artwork_url: Option<String>,
}
impl Resource {
    #[must_use]
    pub fn view(&self) -> ResourceView<'_> {
        ResourceView {
            resource: self,
            url: format!("/music/remote/{}", self.id),
            stream_url: self.stream_url(),
            artwork_url: self.artwork_url(),
        }
    }
    #[must_use]
    pub fn artwork_url(&self) -> Option<String> {
        self.details
            .image_url
            .as_ref()
            .map(|_| format!("/music/remote/{}/artwork", self.id))
    }
    fn stream_url(&self) -> Option<String> {
        matches!(self.kind.as_str(), "audio" | "track")
            .then(|| format!("/music/remote/{}/stream", self.id))
    }
    #[must_use]
    pub fn entity(&self) -> serde_json::Value {
        serde_json::json!({"id":self.id.to_string(),"kind":"remote","resource_kind":self.kind,"name":self.name,"title":self.name,"description":self.summary,"details":self.details.public_value(),"uri":self.uri,"url":format!("/music/remote/{}",self.id),"media_of":self.media_of,"parent_uri":self.parent_uri,"stream_url":self.stream_url(),"artwork_url":self.artwork_url(),"image_url":self.artwork_url(),"artist":self.details.artist,"album":self.details.album,"genre":self.details.genre,"tags":self.details.tags})
    }
}
// A proof must reach a live self-controlled root within the document depth budget.
// Every inherited audience and controller policy remains a current dependency.
macro_rules! live_proof {
    ($resource:literal) => {concat!("EXISTS(WITH RECURSIVE chain AS (SELECT p.*,1 AS depth,ARRAY[p.uri] AS path,false AS cycle FROM music_remote_resources p WHERE p.uri=",$resource,".proof_uri AND p.kind=",$resource,".proof_kind AND (",$resource,".kind!='track' OR p.uri=",$resource,".uri OR p.kind!='audio' OR p.media_of=",$resource,".uri) UNION ALL SELECT p.*,c.depth+1,c.path||p.uri,p.uri=ANY(c.path) FROM music_remote_resources p JOIN chain c ON p.uri=c.proof_uri AND p.kind=c.proof_kind WHERE c.uri!=c.proof_uri AND NOT c.cycle AND c.depth<7) SELECT 1 FROM chain c JOIN accounts owner ON owner.id=c.controller_id HAVING bool_or(c.uri=c.proof_uri) AND bool_and(NOT c.cycle AND c.unavailable_at IS NULL AND owner.domain IS NOT NULL AND owner.deleted_at IS NULL AND owner.suspended_at IS NULL AND NOT account_silenced(owner.id) AND instance_domain_allowed(owner.domain) AND NOT account_hidden($2,owner.id)))")};
}
macro_rules! live_resource {
    ($resource:literal) => {concat!("(",live_proof!($resource)," OR (",$resource,".kind='track' AND EXISTS(SELECT 1 FROM music_remote_track_proofs candidate WHERE candidate.uri=",$resource,".uri AND candidate.controller_id=",$resource,".controller_id AND ",live_proof!("candidate"),")))")};
}
macro_rules! readable_query {
    ($predicate:literal, $suffix:literal) => { concat!("SELECT r.* FROM music_remote_resources r JOIN accounts a ON a.id=r.controller_id WHERE ", $predicate,
    " AND r.unavailable_at IS NULL AND ",live_resource!("r")," AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.deleted_at IS NULL AND NOT account_silenced(a.id) AND instance_domain_allowed(a.domain) AND NOT account_hidden($2,a.id) AND NOT EXISTS(SELECT 1 FROM music_remote_resources target JOIN accounts target_owner ON target_owner.id=target.controller_id WHERE target.uri=r.media_of AND (target.unavailable_at IS NOT NULL OR NOT ",live_resource!("target")," OR target_owner.suspended_at IS NOT NULL OR target_owner.deleted_at IS NOT NULL OR account_silenced(target_owner.id) OR NOT instance_domain_allowed(target_owner.domain) OR account_hidden($2,target_owner.id))) ", $suffix) };
}
pub(super) use live_proof;
pub(super) use live_resource;
pub(super) use readable_query;

pub async fn for_ids(
    state: &AppState,
    ids: &[i64],
    viewer: i64,
) -> Result<Vec<Resource>, ApiError> {
    for_ids_pool(&state.pool, ids, Some(viewer)).await
}

/// Discover cached public metadata through the same controller and proof gates
/// as detail pages. Playback still verifies the origin's current availability.
pub async fn catalogue(
    state: &AppState,
    viewer: i64,
    query: &str,
    before: Option<i64>,
) -> Result<Vec<Resource>, ApiError> {
    catalogue_section(state, viewer, query, before, "all").await
}

pub async fn catalogue_section(
    state: &AppState,
    viewer: i64,
    query: &str,
    before: Option<i64>,
    kind: &str,
) -> Result<Vec<Resource>, ApiError> {
    if !matches!(
        kind,
        "all" | "live" | "artist" | "album" | "library" | "playlist"
    ) {
        return Err(invalid("Invalid music section"));
    }
    if query.chars().count() > 200 || query.contains('\0') {
        return Err(invalid("Invalid music search"));
    }
    let mut resources: Vec<Resource> = sqlx::query_as(readable_query!(
        "($4='all' OR ($4='live' AND r.catalogue_kind='stream') OR r.kind=$4) AND ($1::bigint IS NULL OR r.id<$1) AND strpos(lower(concat_ws(' ',r.name,(r.details).artist,(r.details).album)),lower($3))>0 AND (r.kind!='audio' OR r.media_of IS NULL OR NOT EXISTS(SELECT 1 FROM music_remote_resources t WHERE t.uri=r.media_of AND t.kind='track' AND t.unavailable_at IS NULL))",
        "ORDER BY r.id DESC LIMIT 61"
    )).bind(before).bind(Some(viewer)).bind(query.trim()).bind(kind).fetch_all(&state.pool).await.map_err(DbError::from)?;
    proofs::decorate(&state.pool, &mut resources, Some(viewer)).await?;
    Ok(resources)
}
pub(crate) async fn for_ids_pool(
    pool: &sqlx::PgPool,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Resource>, ApiError> {
    if ids.len() > 100 {
        return Err(invalid("Remote music batch is too large"));
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut resources: Vec<Resource> = sqlx::query_as(readable_query!("r.id=ANY($1)", ""))
        .bind(ids)
        .bind(viewer)
        .fetch_all(pool)
        .await
        .map_err(DbError::from)?;
    proofs::decorate(pool, &mut resources, viewer).await?;
    let parents = resources
        .iter()
        .filter(|resource| resource.kind == "track" && resource.details.image_url.is_none())
        .filter_map(|resource| resource.parent_uri.clone())
        .collect::<Vec<_>>();
    if !parents.is_empty() {
        let covers: Vec<Resource> =
            sqlx::query_as(readable_query!("r.uri=ANY($1) AND r.kind='album'", ""))
                .bind(&parents)
                .bind(viewer)
                .fetch_all(pool)
                .await
                .map_err(DbError::from)?;
        let covers = covers
            .into_iter()
            .filter_map(|resource| {
                resource
                    .details
                    .image_url
                    .clone()
                    .map(|image| (resource.uri, image))
            })
            .collect::<HashMap<_, _>>();
        for resource in &mut resources {
            if resource.kind == "track" && resource.details.image_url.is_none() {
                resource.details.image_url = resource
                    .parent_uri
                    .as_ref()
                    .and_then(|parent| covers.get(parent))
                    .cloned();
            }
        }
    }
    Ok(resources)
}
pub(crate) async fn for_uris_pool(
    pool: &sqlx::PgPool,
    uris: &[String],
    viewer: Option<i64>,
) -> Result<Vec<Resource>, ApiError> {
    if uris.len() > 200 {
        return Err(invalid("Too many remote catalogue references"));
    }
    if uris.is_empty() {
        return Ok(Vec::new());
    }
    let mut resources = sqlx::query_as(readable_query!("r.uri=ANY($1)", ""))
        .bind(uris)
        .bind(viewer)
        .fetch_all(pool)
        .await
        .map_err(DbError::from)?;
    proofs::decorate(pool, &mut resources, viewer).await?;
    Ok(resources)
}

pub(crate) async fn media_for_uris_pool(
    pool: &sqlx::PgPool,
    uris: &[String],
    viewer: Option<i64>,
) -> Result<HashMap<String, Resource>, ApiError> {
    if uris.len() > 100 {
        return Err(invalid("Remote music batch is too large"));
    }
    if uris.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Resource> = sqlx::query_as(concat!(
        "SELECT DISTINCT ON(media_of) * FROM (",
        readable_query!(
            "r.media_of=ANY($1) AND r.kind='audio' AND r.audio_url IS NOT NULL",
            ""
        ),
        ") available ORDER BY media_of,id DESC"
    ))
    .bind(uris)
    .bind(viewer)
    .fetch_all(pool)
    .await
    .map_err(DbError::from)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.media_of.clone().map(|uri| (uri, row)))
        .collect())
}

/// Feed metadata uses the same current origin policy and selected rendition.
pub(crate) async fn feed_metadata_pool(
    pool: &sqlx::PgPool,
    domain: &str,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<HashMap<i64, (String, OffsetDateTime, String)>, ApiError> {
    let resources = for_ids_pool(pool, ids, viewer).await?;
    let uris = resources
        .iter()
        .filter(|row| row.audio_url.is_none())
        .map(|row| row.uri.clone())
        .collect::<Vec<_>>();
    let media = media_for_uris_pool(pool, &uris, viewer).await?;
    let mut result = HashMap::new();
    for row in resources {
        let selected = if row.audio_url.is_some() {
            Some(&row)
        } else {
            media.get(&row.uri)
        };
        let Some(selected) = selected else {
            continue;
        };
        let Some(url) = selected.audio_url.as_deref() else {
            continue;
        };
        if !crate::instance_policy::can_federate_url(pool, domain, url).await? {
            continue;
        }
        let mime = selected
            .details
            .audio_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".into());
        let updated = row.fetched_at.max(selected.fetched_at);
        result.insert(row.id, (row.summary, updated, mime));
    }
    Ok(result)
}

/// Canonical playable references are resolved under the same current cache policy.
pub(crate) async fn playables_pool(
    pool: &sqlx::PgPool,
    domain: &str,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<HashMap<i64, crate::music_collections::Playable>, ApiError> {
    let resources = for_ids_pool(pool, ids, viewer).await?;
    let uris = resources
        .iter()
        .filter(|row| row.kind == "track" && row.audio_url.is_none())
        .map(|row| row.uri.clone())
        .collect::<Vec<_>>();
    let media = media_for_uris_pool(pool, &uris, viewer).await?;
    let mut result = HashMap::new();
    for row in resources {
        if !matches!(row.kind.as_str(), "track" | "audio") {
            continue;
        }
        let url = row.audio_url.as_ref().or_else(|| {
            media
                .get(&row.uri)
                .and_then(|media| media.audio_url.as_ref())
        });
        let Some(url) = url else {
            continue;
        };
        if !crate::instance_policy::can_federate_url(pool, domain, url).await? {
            continue;
        }
        result.insert(
            row.id,
            crate::music_collections::Playable {
                entry_id: String::new(),
                title: row.name.clone(),
                artist: row.details.artist.clone(),
                url: format!("/music/remote/{}", row.id),
                stream_url: format!("/music/remote/{}/stream", row.id),
                duration: row.details.duration.unwrap_or(0.0),
                image_url: row.artwork_url(),
            },
        );
    }
    Ok(result)
}

pub struct Contents {
    pub items: Vec<Resource>,
    pub before: Option<i64>,
}
/// Collapse rendition references before pagination so multiple uploads cannot
/// repeat or displace the canonical track across library pages.
pub async fn contents(
    state: &AppState,
    parent: i64,
    viewer: i64,
    before: Option<i64>,
) -> Result<Contents, ApiError> {
    if before.is_some_and(|cursor| cursor <= 0) {
        return Err(invalid("Invalid remote music cursor"));
    }
    let root = get(state, parent, viewer).await?;
    let mut cursor = before;
    let mut items = Vec::new();
    loop {
        let ids: Vec<i64> = if matches!(root.kind.as_str(), "library" | "playlist") {
            members::ids(state, &root.uri, viewer, cursor).await?
        } else {
            sqlx::query_scalar(concat!("SELECT DISTINCT COALESCE(target.id,r.id) AS id FROM (",readable_query!("((r.catalogue_kind IS NULL AND r.parent_uri=$1) OR r.uri IN(SELECT resource_uri FROM music_remote_members WHERE parent_uri=$1))",""),") r LEFT JOIN music_remote_resources target ON r.kind='audio' AND target.uri=r.media_of AND target.kind='track' WHERE ($3::bigint IS NULL OR COALESCE(target.id,r.id)<$3) ORDER BY id DESC LIMIT 100"))
            .bind(&root.uri).bind(Some(viewer)).bind(cursor).fetch_all(&state.pool).await.map_err(DbError::from)?
        };
        if ids.is_empty() {
            break;
        }
        cursor = ids.last().copied();
        let sources = for_ids(state, &ids, viewer).await?;
        let by_id = sources
            .into_iter()
            .map(|source| (source.id, source))
            .collect::<HashMap<_, _>>();
        for id in &ids {
            if let Some(source) = by_id.get(id) {
                if items.len() == 60 {
                    get(state, parent, viewer).await?;
                    return Ok(Contents {
                        before: items.last().map(|source: &Resource| source.id),
                        items,
                    });
                }
                items.push(source.clone());
            }
        }
        if ids.len() < 100 {
            break;
        }
    }
    get(state, parent, viewer).await?;
    Ok(Contents {
        items,
        before: None,
    })
}

/// Only a verified remote Audio publication may bridge the legacy status view.
pub(crate) async fn status_reference(
    state: &AppState,
    status_id: i64,
    viewer: i64,
) -> Result<Option<i64>, ApiError> {
    let Some(status) = plamenu_db::status::find_by_id(&state.pool, status_id).await? else {
        return Ok(None);
    };
    if status.object_type.as_deref() != Some("Audio")
        || status.reblog_of_id.is_some()
        || plamenu_db::status::is_deleted(&state.pool, status_id).await?
        || !crate::entities::can_view(&state.pool, &status, Some(viewer)).await?
    {
        return Ok(None);
    }
    let remote: Option<i64> = sqlx::query_scalar("SELECT r.id FROM music_remote_resources r JOIN accounts a ON a.id=r.controller_id WHERE r.uri=$1 AND r.controller_id=$2 AND r.kind='audio' AND a.domain IS NOT NULL")
        .bind(&status.uri).bind(status.account_id).fetch_optional(&state.pool).await.map_err(DbError::from)?;
    let Some(remote) = remote else {
        return Ok(None);
    };
    Ok(Some(canonical(state, remote, viewer).await?.id))
}

/// Identity-only lookup lets an account remove its bookmark after withdrawal.
pub(crate) async fn status_identity(
    pool: &sqlx::PgPool,
    status: i64,
) -> Result<Option<i64>, ApiError> {
    Ok(sqlx::query_scalar("-- STUBKEEP: identity-only lookup permits bookmark removal after withdrawal\nSELECT COALESCE(track.id,audio.id) FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources audio ON audio.uri=s.uri AND audio.controller_id=s.account_id AND audio.kind='audio' LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE s.id=$1 AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL")
        .bind(status).fetch_optional(pool).await.map_err(DbError::from)?)
}

/// Creation preserves both the legacy post audience and canonical origin policy.
pub(crate) async fn normalize_sources(
    state: &AppState,
    entries: &mut [crate::music_collections::Entry],
    viewer: i64,
) -> Result<(), ApiError> {
    if entries.len() > 1000 {
        return Err(invalid("Too many remote music references"));
    }
    for page in entries.chunks_mut(100) {
        let ids = page
            .iter()
            .filter_map(|entry| entry.status_id)
            .collect::<Vec<_>>();
        let mappings: Vec<(i64,i64)> = sqlx::query_as("-- STUBKEEP: identity mapping; subsequent deleted_ids and audience checks govern reads\nSELECT s.id,COALESCE(track.id,audio.id) FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources audio ON audio.uri=s.uri AND audio.controller_id=s.account_id AND audio.kind='audio' LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE s.id=ANY($1) AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL")
            .bind(&ids).fetch_all(&state.pool).await.map_err(DbError::from)?;
        if !mappings.is_empty() {
            let rows = plamenu_db::status::find_by_ids(&state.pool, &ids).await?;
            let deleted = plamenu_db::status::deleted_ids(&state.pool, &ids).await?;
            let visible = crate::entities::filter_viewable(&state.pool, &rows, Some(viewer))
                .await?
                .into_iter()
                .filter(|row| !deleted.contains(&row.id))
                .map(|row| row.id)
                .collect::<HashSet<_>>();
            let mappings = mappings.into_iter().collect::<HashMap<_, _>>();
            for entry in page.iter_mut() {
                if let Some(status) = entry.status_id
                    && let Some(remote) = mappings.get(&status)
                {
                    if !visible.contains(&status) {
                        return Err(ApiError::NotFound);
                    }
                    entry.status_id = None;
                    entry.remote_id = Some(*remote);
                }
            }
        }
        normalize_entries(&state.pool, page).await?;
    }
    Ok(())
}

/// Resolve historical publications for reads without widening their post audience.
/// Unreadable verified aliases are omitted, never rendered through stale attachments.
pub(crate) async fn readable_entries(
    pool: &sqlx::PgPool,
    entries: &[crate::music_collections::Entry],
    viewer: Option<i64>,
) -> Result<Vec<crate::music_collections::Entry>, ApiError> {
    let mut result = Vec::with_capacity(entries.len());
    for page in entries.chunks(100) {
        let ids = page
            .iter()
            .filter_map(|entry| entry.status_id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            let mut normalized = page.to_vec();
            normalize_entries(pool, &mut normalized).await?;
            result.extend(normalized);
            continue;
        }
        let mappings: Vec<(i64,i64)> = sqlx::query_as("-- STUBKEEP: identity mapping; subsequent deleted_ids and audience checks govern reads\nSELECT s.id,COALESCE(track.id,audio.id) FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources audio ON audio.uri=s.uri AND audio.controller_id=s.account_id AND audio.kind='audio' LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE s.id=ANY($1) AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL")
            .bind(&ids).fetch_all(pool).await.map_err(DbError::from)?;
        let mappings = mappings.into_iter().collect::<HashMap<_, _>>();
        let visible = if mappings.is_empty() {
            HashSet::new()
        } else {
            let rows = plamenu_db::status::find_by_ids(pool, &ids).await?;
            let deleted = plamenu_db::status::deleted_ids(pool, &ids).await?;
            let authors = rows.iter().map(|row| row.account_id).collect::<Vec<_>>();
            let blocked: Vec<i64> = sqlx::query_scalar("SELECT target_account_id FROM blocks WHERE account_id=$1 AND target_account_id=ANY($2)")
                .bind(viewer).bind(authors).fetch_all(pool).await.map_err(DbError::from)?;
            crate::entities::filter_viewable(pool, &rows, viewer)
                .await?
                .into_iter()
                .filter(|row| !deleted.contains(&row.id) && !blocked.contains(&row.account_id))
                .map(|row| row.id)
                .collect()
        };
        let mut normalized = page
            .iter()
            .filter_map(|entry| {
                let mut entry = entry.clone();
                if let Some(status) = entry.status_id
                    && let Some(remote) = mappings.get(&status)
                {
                    if !visible.contains(&status) {
                        return None;
                    }
                    entry.status_id = None;
                    entry.remote_id = Some(*remote);
                }
                Some(entry)
            })
            .collect::<Vec<_>>();
        normalize_entries(pool, &mut normalized).await?;
        result.extend(normalized);
    }
    Ok(result)
}

/// Identity folding may remove an unavailable personal reference without revealing metadata.
pub(crate) async fn normalize_entries(
    pool: &sqlx::PgPool,
    entries: &mut [crate::music_collections::Entry],
) -> Result<(), ApiError> {
    let ids = entries
        .iter()
        .filter_map(|entry| entry.remote_id)
        .collect::<Vec<_>>();
    if ids.len() > 1000 {
        return Err(invalid("Too many remote music references"));
    }
    if ids.is_empty() {
        return Ok(());
    }
    let aliases: Vec<(i64,i64)> = sqlx::query_as("SELECT audio.id,track.id FROM music_remote_resources audio JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE audio.id=ANY($1) AND audio.kind='audio'")
        .bind(&ids).fetch_all(pool).await.map_err(DbError::from)?;
    let aliases = aliases.into_iter().collect::<HashMap<_, _>>();
    for entry in entries {
        if let Some(canonical) = entry.remote_id.and_then(|id| aliases.get(&id)) {
            entry.remote_id = Some(*canonical);
        }
    }
    Ok(())
}

async fn canonical(state: &AppState, id: i64, viewer: i64) -> Result<Resource, ApiError> {
    let resource = get(state, id, viewer).await?;
    if resource.kind == "audio"
        && let Some(uri) = &resource.media_of
    {
        let target:Option<i64>=sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1 AND kind='track' AND unavailable_at IS NULL").bind(uri).fetch_optional(&state.pool).await.map_err(DbError::from)?;
        if let Some(target) = target {
            return get(state, target, viewer).await;
        }
    }
    Ok(resource)
}
pub async fn save(
    state: &AppState,
    owner: i64,
    reference: &crate::music_saved::Reference,
) -> Result<crate::music_saved::Saved, ApiError> {
    let (_, id) = reference.key()?;
    let source = canonical(state, id, owner).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    sqlx::query("INSERT INTO music_saved_remote(id,account_id,resource_id) VALUES($1,$2,$3) ON CONFLICT(account_id,resource_id) DO NOTHING").bind(plamenu_db::id::next()).bind(owner).bind(source.id).execute(&mut *tx).await.map_err(DbError::from)?;
    let (saved_id, created): (i64, OffsetDateTime) = sqlx::query_as(
        "SELECT id,created_at FROM music_saved_remote WHERE account_id=$1 AND resource_id=$2",
    )
    .bind(owner)
    .bind(source.id)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(crate::music_saved::Saved {
        id: saved_id.to_string(),
        reference: crate::music_saved::Reference {
            kind: crate::music_saved::Kind::Remote,
            id: source.id.to_string(),
        },
        created_at: crate::entities::rfc3339(created)?,
        source: Some(get(state, source.id, owner).await?.entity()),
    })
}
pub async fn contains(
    state: &AppState,
    owner: i64,
    reference: &crate::music_saved::Reference,
) -> Result<bool, ApiError> {
    let (_, id) = reference.key()?;
    let resource = canonical(state, id, owner).await?;
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_saved_remote WHERE account_id=$1 AND resource_id=ANY($2))").bind(owner).bind(vec![id,resource.id]).fetch_one(&state.pool).await.map_err(DbError::from)?)
}
pub async fn remove_reference(
    state: &AppState,
    owner: i64,
    reference: &crate::music_saved::Reference,
) -> Result<(), ApiError> {
    let (_, id) = reference.key()?;
    let canonical = match canonical(state, id, owner).await {
        Ok(row) => row.id,
        Err(ApiError::NotFound) => id,
        Err(error) => return Err(error),
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    sqlx::query("DELETE FROM music_saved_remote WHERE account_id=$1 AND resource_id=ANY($2)")
        .bind(owner)
        .bind(vec![id, canonical])
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
